//! Сквозной тест потока данных: репозиторий читает строку, сущность получает property, use case читает
//! значение. Проверяет связку `scan_flow` + `scan_parents` + `Index::traces` без CLI.

use std::fs;
use std::path::PathBuf;

use callgraph::flow::{Index, scan_flow, scan_parents};

fn fixture_dir(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "callgraph-flow-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    for (path, source) in files {
        let target = dir.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, source).unwrap();
    }
    dir
}

fn fixture() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "callgraph-flow-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("src/Repo.php"),
        r#"<?php
namespace App;

class Repo
{
    public function find(int $id): ?Item
    {
        $row = $this->connection->fetchAssociative('SELECT * FROM items WHERE id = :id', ['id' => $id]);
        return new Item((int)$row['id'], $row['service_type']);
    }
}
"#,
    )
    .unwrap();
    fs::write(
        dir.join("src/Item.php"),
        r#"<?php
namespace App;

class Item extends ItemBase
{
    public function __construct(private int $id, string $serviceType)
    {
        parent::__construct($serviceType);
    }
}
"#,
    )
    .unwrap();
    fs::write(
        dir.join("src/ItemBase.php"),
        r#"<?php
namespace App;

abstract class ItemBase
{
    public function __construct(private string $serviceType)
    {
    }

    public function getServiceType(): string
    {
        return $this->serviceType;
    }
}
"#,
    )
    .unwrap();
    fs::write(
        dir.join("src/UseCase.php"),
        r#"<?php
namespace App;

class UseCase
{
    public function __construct(private Repo $repo)
    {
    }

    public function run(int $id): string
    {
        $item = $this->repo->find($id);
        if ($item->getServiceType() === 'avia') {
            return 'avia';
        }
        return 'other';
    }
}
"#,
    )
    .unwrap();
    dir
}

#[test]
fn column_reaches_the_use_case_read_through_the_entity() {
    let dir = fixture();
    let files = scan_flow(&dir).unwrap();
    let parents = scan_parents(&dir).unwrap();
    let index = Index::build_with_parents(&files, &parents);
    let traces = index.traces("service_type");
    let _ = fs::remove_dir_all(&dir);

    let read = traces
        .iter()
        .find(|trace| trace.method.ends_with("UseCase::run"))
        .unwrap_or_else(|| panic!("no use-case read: {traces:#?}"));
    assert!(read.chain.iter().any(|atom| atom == "key:service_type"));
    assert!(
        read.chain
            .iter()
            .any(|atom| atom.ends_with("ItemBase::serviceType")),
        "{:?}",
        read.chain
    );
    assert!(
        read.chain
            .iter()
            .any(|atom| atom.contains("getServiceType")),
        "{:?}",
        read.chain
    );
}

#[test]
fn literal_call_does_not_inherit_another_sites_column() {
    let dir = fixture_dir(
        "precise",
        &[
            (
                "src/H.php",
                r#"<?php
namespace App;

function isEn($value): bool
{
    return $value === 'en';
}

class Client
{
    public string $nameEn;

    public function check(Client $other): void
    {
        $literal = isEn('en');
        if ($literal) {
            $x = 1;
        }
        $real = isEn($other->nameEn);
        if ($real) {
            $y = 2;
        }
    }
}
"#,
            ),
            (
                "src/Repo.php",
                r#"<?php
namespace App;

class Repo
{
    public function load(): Client
    {
        $row = $this->connection->fetchAssociative('SELECT * FROM clients');
        $client = new Client();
        $client->nameEn = $row['name_en'];
        return $client;
    }
}
"#,
            ),
        ],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    let lines: Vec<(String, u32)> = traces
        .iter()
        .map(|trace| (trace.method.clone(), trace.line))
        .collect();
    assert!(
        lines
            .iter()
            .any(|(method, _)| method.ends_with("Client::check")),
        "{lines:?}"
    );
    assert!(
        !traces.iter().any(|trace| trace.expr.contains("isEn('en')")),
        "literal call inherited another call site: {traces:#?}"
    );
}

#[test]
fn allocation_site_binds_the_property_read() {
    let dir = fixture_dir(
        "alloc",
        &[
            (
                "src/Item.php",
                r#"<?php
namespace App;

class Item
{
    public function __construct(private string $name)
    {
    }

    public function getName(): string
    {
        return $this->name;
    }
}
"#,
            ),
            (
                "src/UseCase.php",
                r#"<?php
namespace App;

class UseCase
{
    public function literal(): void
    {
        $item = new Item('literal');
        if ($item->getName() === 'literal') {
            $a = 1;
        }
    }

    public function fromRow(): void
    {
        $row = $this->connection->fetchAssociative('SELECT * FROM items');
        $item = new Item($row['name_en']);
        if ($item->getName() === 'row') {
            $b = 2;
        }
    }
}
"#,
            ),
        ],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("UseCase::fromRow")),
        "{traces:#?}"
    );
    assert!(
        !traces
            .iter()
            .any(|trace| trace.method.ends_with("UseCase::literal")),
        "the literal allocation inherited another object's column: {traces:#?}"
    );
}

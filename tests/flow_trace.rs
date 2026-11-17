//! Сквозной тест потока данных: репозиторий читает строку, сущность получает property, use case читает
//! значение. Проверяет связку `scan_flow` + `scan_parents` + `Index::traces` без CLI.

use std::fs;
use std::path::PathBuf;

use callgraph::flow::{Index, scan_flow, scan_parents};

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

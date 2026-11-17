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

#[test]
fn setter_write_binds_to_the_allocation() {
    let dir = fixture_dir(
        "setter",
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

    public function setName(string $name): void
    {
        $this->name = $name;
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
        $item->setName('literal');
        if ($item->getName()) {
            $a = 1;
        }
    }

    public function fromRow(): void
    {
        $row = $this->connection->fetchAssociative('SELECT * FROM items');
        $item = new Item('start');
        $item->setName($row['name_en']);
        if ($item->getName()) {
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
        "the setter write did not bind to the allocation: {traces:#?}"
    );
    assert!(
        !traces
            .iter()
            .any(|trace| trace.method.ends_with("UseCase::literal")),
        "the literal setter write inherited another object's column: {traces:#?}"
    );
}

#[test]
fn this_chain_binds_to_the_enclosing_allocation() {
    let dir = fixture_dir(
        "this-chain",
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

    public function load(): string
    {
        return $this->name;
    }

    public function run(): string
    {
        return $this->load();
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
        if ($item->run()) {
            $a = 1;
        }
    }

    public function fromRow(): void
    {
        $row = $this->connection->fetchAssociative('SELECT * FROM items');
        $item = new Item($row['name_en']);
        if ($item->run()) {
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

    let matching = traces
        .iter()
        .filter(|trace| trace.method.ends_with("UseCase::fromRow") && trace.expr.contains("run"))
        .collect::<Vec<_>>();
    assert!(
        !matching.is_empty(),
        "the this-chain did not bind to the enclosing allocation: {traces:#?}"
    );
    assert!(
        matching
            .iter()
            .any(|trace| trace.chain.iter().any(|atom| atom.contains("load"))),
        "the this-chain did not pass through load: {matching:#?}"
    );
    assert!(
        !traces
            .iter()
            .any(|trace| trace.method.ends_with("UseCase::literal") && trace.expr.contains("run")),
        "the literal allocation inherited the chain: {traces:#?}"
    );
}

#[test]
fn setter_through_parameter_binds() {
    let dir = fixture_dir(
        "setter-param",
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

    public function setName(string $name): void
    {
        $this->name = $name;
    }

    public function getName(): string
    {
        return $this->name;
    }
}
"#,
            ),
            (
                "src/Applier.php",
                r#"<?php
namespace App;

class Applier
{
    public function apply(Item $item, string $value): void
    {
        $item->setName($value);
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
    public function run(): void
    {
        $row = $this->connection->fetchAssociative('SELECT * FROM items');
        $item = new Item('start');
        (new Applier())->apply($item, $row['name_en']);
        if ($item->getName()) {
            $a = 1;
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

    let read = traces
        .iter()
        .find(|trace| trace.method.ends_with("UseCase::run") && trace.expr.contains("getName"))
        .unwrap_or_else(|| panic!("the setter through a parameter did not bind: {traces:#?}"));
    assert!(
        read.expr.contains("getName"),
        "unexpected read expr: {}",
        read.expr
    );
}

#[test]
fn must_marks_reads_reached_on_every_path() {
    let dir = fixture_dir(
        "path",
        &[(
            "src/UseCase.php",
            r#"<?php
namespace App;

class UseCase
{
    public function maybe(array $row, bool $c): string
    {
        $name = 'default';
        if ($c) {
            $name = $row['name_en'];
        }
        return $name;
    }

    public function always(array $row): string
    {
        $name = $row['name_en'];
        if ($row) {
            $x = 1;
        }
        return $name;
    }
}

class Runner
{
    public function runMaybe(array $row, bool $c): string
    {
        return (new UseCase())->maybe($row, $c);
    }

    public function runAlways(array $row): string
    {
        return (new UseCase())->always($row);
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    let must = |method: &str| {
        traces
            .iter()
            .find(|trace| trace.method.ends_with(method))
            .unwrap_or_else(|| panic!("no trace for {method}: {traces:#?}"))
            .must
    };
    assert!(must("Runner::runAlways"), "{traces:#?}");
    assert!(!must("Runner::runMaybe"), "{traces:#?}");
}

#[test]
fn interface_dispatch_reaches_the_consumer() {
    let dir = fixture_dir(
        "iface",
        &[(
            "src/Ifaces.php",
            r#"<?php
namespace App;

interface Src { public function name(): string; }

class DbSrc implements Src
{
    public function __construct(private array $row) {}
    public function name(): string { return $this->row['name_en']; }
}

class FixedSrc implements Src
{
    public function name(): string { return 'fixed'; }
}

class User
{
    public function show(Src $src): void
    {
        if ($src->name() === 'x') { $a = 1; }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build_with_parents(&files, &scan_parents(&dir).unwrap());
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);
    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("User::show")),
        "{traces:#?}"
    );
}

#[test]
fn trait_method_reaches_the_consumer() {
    let dir = fixture_dir(
        "trait",
        &[(
            "src/TraitCase.php",
            r#"<?php
namespace App;

trait HasName
{
    public function getName(): string { return $this->name; }
}

class Item
{
    use HasName;
    private string $name;
    public function __construct(string $name) { $this->name = $name; }
}

class User
{
    public function run(array $row): void
    {
        $item = new Item($row['name_en']);
        if ($item->getName() === 'x') { $b = 2; }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build_with_parents(&files, &scan_parents(&dir).unwrap());
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);
    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("User::run")),
        "{traces:#?}"
    );
}

#[test]
fn try_destructuring_and_yield_keep_the_column() {
    let dir = fixture_dir(
        "legacy",
        &[(
            "src/Legacy.php",
            r#"<?php
namespace App;

class Legacy
{
    public function tryCase(array $row): void
    {
        try { $x = $row['name_en']; } catch (\Throwable $e) { $x = 'x'; }
        if ($x) { $a = 1; }
    }

    public function destructure(array $row): void
    {
        list($name) = [$row['name_en']];
        if ($name) { $b = 2; }
    }

    public function gen(array $row): \Generator
    {
        yield $row['name_en'];
    }

    public function consume(array $row): void
    {
        foreach ($this->gen($row) as $value) {
            if ($value) { $c = 3; }
        }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);
    let methods: Vec<&str> = traces.iter().map(|trace| trace.method.as_str()).collect();
    assert!(
        methods.iter().any(|m| m.ends_with("Legacy::tryCase")),
        "{methods:?}"
    );
    assert!(
        methods.iter().any(|m| m.ends_with("Legacy::destructure")),
        "{methods:?}"
    );
    assert!(
        methods.iter().any(|m| m.ends_with("Legacy::consume")),
        "{methods:?}"
    );
}

#[test]
fn array_column_keeps_the_column_key() {
    let dir = fixture_dir(
        "column",
        &[(
            "src/ColumnCase.php",
            r#"<?php
namespace App;

class ColumnCase
{
    public function run(array $rows): void
    {
        $names = array_column($rows, 'name_en');
        if ($names) { $a = 1; }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);
    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("ColumnCase::run")),
        "{traces:#?}"
    );
}

#[test]
fn fetch_object_property_is_a_column() {
    let dir = fixture_dir(
        "fetch-object",
        &[(
            "src/FetchObject.php",
            r#"<?php
namespace App;

class UseCase
{
    public function run(object $stmt): void
    {
        $row = $stmt->fetchObject();
        if ($row->name_en) {
            $a = 1;
        }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build_with_parents(&files, &scan_parents(&dir).unwrap());
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("UseCase::run") && trace.expr.contains("name_en")),
        "fetchObject() property read was not bound to the column: {traces:#?}"
    );
}

#[test]
fn magic_get_property_is_a_column() {
    let dir = fixture_dir(
        "magic-get",
        &[(
            "src/Magic.php",
            r#"<?php
namespace App;

class Row
{
    private array $data = [];

    public function __get(string $name): mixed
    {
        return $this->data[$name] ?? null;
    }

    public function __isset(string $name): bool
    {
        return isset($this->data[$name]);
    }
}

class UseCase
{
    public function run(Row $obj): void
    {
        if ($obj->name_en) {
            $a = 1;
        }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build_with_parents(&files, &scan_parents(&dir).unwrap());
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("UseCase::run") && trace.expr.contains("name_en")),
        "the __get property read was not bound to the column: {traces:#?}"
    );
}

#[test]
fn extract_variables_are_columns() {
    let dir = fixture_dir(
        "extract",
        &[(
            "src/Extract.php",
            r#"<?php
namespace App;

class UseCase
{
    public function run(array $row): void
    {
        extract($row);
        if ($name_en) {
            $a = 1;
        }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces.iter().any(|trace| {
            trace.method.ends_with("UseCase::run") && trace.expr.contains("name_en")
        }),
        "extract() variables were not bound to the columns: {traces:#?}"
    );
}

#[test]
fn get_object_vars_exposes_properties() {
    let dir = fixture_dir(
        "object-vars",
        &[(
            "src/Vars.php",
            r#"<?php
namespace App;

class Item
{
    public string $name_en = '';

    public function toArray(): array
    {
        return get_object_vars($this);
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build_with_parents(&files, &scan_parents(&dir).unwrap());
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    let exposes = traces.iter().any(|trace| {
        trace.method.ends_with("Item::toArray")
            && trace.chain.iter().any(|atom| {
                atom == "key:name_en" || atom == "prop:name_en" || atom.ends_with("::name_en")
            })
    });
    assert!(
        exposes,
        "get_object_vars($this) did not expose the property as a column: {traces:#?}"
    );
}

#[test]
fn compact_keeps_variable_origins() {
    let dir = fixture_dir(
        "compact",
        &[(
            "src/Compact.php",
            r#"<?php
namespace App;

class UseCase
{
    public function run(array $row): void
    {
        $name_en = $row['name_en'];
        $data = compact('name_en');
        if ($data) {
            $a = 1;
        }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("UseCase::run") && trace.expr == "$data"),
        "compact() lost the variable origins: {traces:#?}"
    );
}

#[test]
fn match_value_keeps_the_branch() {
    let dir = fixture_dir(
        "match",
        &[(
            "src/Match.php",
            r#"<?php
namespace App;

class UseCase
{
    public function run(array $row, string $k): void
    {
        $name = match ($k) {
            'a' => $row['name_en'],
            default => 'x',
        };
        if ($name) {
            $a = 1;
        }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("UseCase::run") && trace.expr == "$name"),
        "match arm lost the column before the read: {traces:#?}"
    );
}

#[test]
fn array_map_arrow_keeps_the_column() {
    let dir = fixture_dir(
        "array-map",
        &[(
            "src/MapCase.php",
            r#"<?php
namespace App;

class MapCase
{
    public function run(array $rows): void
    {
        $names = array_map(fn($r) => $r['name_en'], $rows);
        if ($names) {
            $a = 1;
        }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("MapCase::run") && trace.expr == "$names"),
        "array_map() arrow lost the column before the read: {traces:#?}"
    );
}

#[test]
fn array_values_and_filter_keep_the_column() {
    let dir = fixture_dir(
        "array-values",
        &[(
            "src/ValuesCase.php",
            r#"<?php
namespace App;

class ValuesCase
{
    public function run(array $rows): void
    {
        $names = array_values(array_filter($rows));
        foreach ($names as $row) {
            if ($row['name_en']) {
                $a = 1;
            }
        }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces.iter().any(
            |trace| trace.method.ends_with("ValuesCase::run") && trace.expr.contains("name_en")
        ),
        "array_values()/array_filter() lost the column: {traces:#?}"
    );
}

#[test]
fn named_argument_reaches_the_constructor() {
    let dir = fixture_dir(
        "named-arg",
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
                "src/NamedUseCase.php",
                r#"<?php
namespace App;

class NamedUseCase
{
    public function run(array $row): void
    {
        $item = new Item(name: $row['name_en']);
        if ($item->getName()) {
            $a = 1;
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
        traces.iter().any(|trace| {
            trace.method.ends_with("NamedUseCase::run") && trace.expr.contains("getName")
        }),
        "the named argument did not reach the constructor: {traces:#?}"
    );
}

#[test]
fn spread_argument_reaches_the_constructor() {
    let dir = fixture_dir(
        "spread-arg",
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
                "src/SpreadUseCase.php",
                r#"<?php
namespace App;

class SpreadUseCase
{
    public function run(array $row): void
    {
        $args = [$row['name_en']];
        $item = new Item(...$args);
        if ($item->getName()) {
            $a = 1;
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
        traces.iter().any(|trace| {
            trace.method.ends_with("SpreadUseCase::run") && trace.expr.contains("getName")
        }),
        "the spread argument did not reach the constructor: {traces:#?}"
    );
}

#[test]
fn static_property_carries_the_column() {
    let dir = fixture_dir(
        "static-prop",
        &[(
            "src/StaticCache.php",
            r#"<?php
namespace App;

class StaticCache
{
    private static string $name = '';

    public function store(array $row): void
    {
        self::$name = $row['name_en'];
    }

    public function check(): void
    {
        if (self::$name) {
            $a = 1;
        }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("StaticCache::check")),
        "the static property did not carry the column across methods: {traces:#?}"
    );
}

#[test]
fn foreach_key_comparison_is_a_column() {
    let dir = fixture_dir(
        "foreach-key",
        &[(
            "src/KeyCase.php",
            r#"<?php
namespace App;

class KeyCase
{
    public function run(array $row): void
    {
        foreach ($row as $key => $value) {
            if ($key === 'name_en') {
                $a = 1;
            }
        }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces.iter().any(|trace| {
            trace.method.ends_with("KeyCase::run")
                && trace.chain.iter().any(|atom| atom == "key:name_en")
        }),
        "the foreach key comparison did not carry key:name_en: {traces:#?}"
    );
}

#[test]
fn global_variable_carries_the_column() {
    let dir = fixture_dir(
        "global-var",
        &[(
            "src/GlobalCase.php",
            r#"<?php
namespace App;

class GlobalCase
{
    public function store(array $row): void
    {
        global $name;
        $name = $row['name_en'];
    }

    public function check(): void
    {
        global $name;
        if ($name) {
            $a = 1;
        }
    }
}
"#,
        )],
    );
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("name_en");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("GlobalCase::check")),
        "the global variable did not carry the column across methods: {traces:#?}"
    );
}

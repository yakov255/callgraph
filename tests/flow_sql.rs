//! Сквозной тест SQL-разбора: `$row['service_type']` из `fetchAllAssociative` даёт
//! `sql:api_queue.queue.service_type` даже через параметр метода и геттер.

use std::fs;
use std::path::PathBuf;

use callgraph::flow::{Index, scan_flow, scan_parents};

fn fixture() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "callgraph-sql-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("src/QueueRepository.php"),
        r#"<?php
namespace App;

class QueueRepository
{
    private $connection;

    public function getByServiceType(string $type): array
    {
        $sql = <<<SQL
SELECT q.id, q.service_type, q.record_type
FROM api_queue.queue q
WHERE q.service_type = :type
SQL;
        $rows = $this->connection->fetchAllAssociative($sql, ['type' => $type]);
        $result = [];
        foreach ($rows as $row) {
            $result[] = $this->makeEntity($row);
        }
        return $result;
    }

    public function findOne(): ?Queue
    {
        $row = $this->connection->fetchAssociative('SELECT service_type FROM api_queue.queue LIMIT 1');
        if ($row === false) {
            return null;
        }
        return $this->makeEntity($row);
    }

    private function makeEntity(array $row): Queue
    {
        return new Queue((int)$row['id'], $row['service_type'], $row['record_type']);
    }
}
"#,
    )
    .unwrap();
    fs::write(
        dir.join("src/Queue.php"),
        r#"<?php
namespace App;

class Queue
{
    public function __construct(
        private int $id,
        private string $serviceType,
        private string $recordType
    ) {
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
        dir.join("src/GetQueueOperation.php"),
        r#"<?php
namespace App;

class GetQueueOperation
{
    public function __construct(private QueueRepository $repo)
    {
    }

    public function run(string $type): void
    {
        foreach ($this->repo->getByServiceType($type) as $queue) {
            if ($queue->getServiceType() === 'avia') {
                $a = 1;
            }
        }
        $one = $this->repo->findOne();
        if ($one !== null && $one->getServiceType() === 'rail') {
            $b = 2;
        }
    }
}
"#,
    )
    .unwrap();
    dir
}

#[test]
fn sql_column_reaches_the_use_case() {
    let dir = fixture();
    let files = scan_flow(&dir).unwrap();
    let parents = scan_parents(&dir).unwrap();
    let index = Index::build_with_parents(&files, &parents);
    let traces = index.traces("api_queue.queue.service_type");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        traces
            .iter()
            .any(|trace| trace.method.ends_with("GetQueueOperation::run")),
        "{traces:#?}"
    );
    assert!(
        traces.iter().any(|trace| {
            trace
                .chain
                .iter()
                .any(|atom| atom == "sql:api_queue.queue.service_type")
        }),
        "{traces:#?}"
    );
}

#[test]
fn bare_column_still_matches_the_sql_atom() {
    let dir = fixture();
    let files = scan_flow(&dir).unwrap();
    let index = Index::build(&files);
    let traces = index.traces("service_type");
    let _ = fs::remove_dir_all(&dir);
    assert!(
        traces.iter().any(|trace| trace
            .chain
            .iter()
            .any(|atom| atom == "sql:api_queue.queue.service_type")),
        "{traces:#?}"
    );
}

//! Тесты хранилища сносок и разбора id.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::store::{load, save};
use super::{RefEntry, parse_id};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_root(name: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("callgraph-refs-{name}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn entry(number: usize, fqcn: &str) -> RefEntry {
    RefEntry {
        number,
        fqcn: fqcn.to_owned(),
        path: Some(PathBuf::from("/tmp/Foo.php")),
        kind: "psr-4".to_owned(),
        source: "own".to_owned(),
        line: Some(12),
    }
}

#[test]
fn save_and_load_roundtrip() {
    let root = temp_root("roundtrip");
    let id = save(&root, vec![entry(1, "App\\Foo")]).unwrap();
    let book = load(&root, id).unwrap();

    assert_eq!(book.id, id);
    assert_eq!(book.entries.len(), 1);
    assert_eq!(book.entries[0].fqcn, "App\\Foo");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ids_increase_and_empty_books_are_not_saved() {
    let root = temp_root("ids");
    assert!(save(&root, Vec::new()).is_none());
    let first = save(&root, vec![entry(1, "A")]).unwrap();
    let second = save(&root, vec![entry(1, "B")]).unwrap();

    assert!(second > first);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn parses_reference_ids() {
    assert_eq!(parse_id("r7"), Some(7));
    assert_eq!(parse_id(" 12 "), Some(12));
    assert_eq!(parse_id("x"), None);
}

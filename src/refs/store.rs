//! Хранилище ссылок на типы: `<project>/.callgraph/refs/<id>.json`.
//!
//! Каждый ответ `show --body --refs` получает короткий id (`rN`); по нему `callgraph refs rN k`
//! находит определение типа. Нумерация — максимальный существующий id + 1, запись атомарная.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Одна сноска: тип, найденный файл и откуда он взялся.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefEntry {
    /// Номер сноски в ответе (`#1`).
    pub number: usize,
    /// Полное имя типа без ведущего `\`.
    pub fqcn: String,
    /// Файл определения (абсолютный путь), если найден.
    pub path: Option<PathBuf>,
    /// Правило автозагрузки: `psr-4`, `psr-0`, `classmap`; `unknown` — не найден.
    pub kind: String,
    /// `own` или имя vendor-пакета.
    pub source: String,
    /// Строка объявления в файле (если известна).
    pub line: Option<u32>,
}

/// Набор сносок одного ответа.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefBook {
    pub id: u64,
    pub entries: Vec<RefEntry>,
}

/// Каталог сносок проекта.
fn dir(root: &Path) -> PathBuf {
    root.join(crate::project::STORE_DIR).join("refs")
}

/// Сохраняет новый набор и возвращает его id (`None`, если запись не удалась).
pub fn save(root: &Path, entries: Vec<RefEntry>) -> Option<u64> {
    if entries.is_empty() {
        return None;
    }
    crate::project::ProjectStore::at_root(root).ensure().ok()?;
    let dir = dir(root);
    std::fs::create_dir_all(&dir).ok()?;
    let id = next_id(&dir);
    let book = RefBook { id, entries };
    let text = serde_json::to_string(&book).ok()?;
    let path = dir.join(format!("{id}.json"));
    let tmp = dir.join(format!("{id}.json.tmp"));
    std::fs::write(&tmp, text).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    Some(id)
}

/// Загружает набор по id.
pub fn load(root: &Path, id: u64) -> Option<RefBook> {
    let text = std::fs::read_to_string(dir(root).join(format!("{id}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

fn next_id(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 1;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.path().file_stem()?.to_str()?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1
}

//! Сноски типов: `show --body --refs` и `refs rN k`.
//!
//! Ответ с `--refs` нумерует именованные типы в сигнатурах и печатает их определения.
//! Полученный id (`rN`) передаётся в `callgraph refs rN k` — команда показывает исходник класса
//! и снова нумерует типы в его сигнатурах, поэтому цепочка раскрывается по шагам.

mod annotate;
mod store;
#[cfg(test)]
mod tests;
mod types;

pub use annotate::{annotate_body, footnotes, trailer};
pub use store::{RefBook, RefEntry, load, save};

use std::path::Path;

use crate::lang::php::PhpBackend;

/// Исходник класса из сноски: строки контейнера с маркерами типов и новые сноски.
pub struct ClassSource {
    pub header: String,
    pub lines: Vec<(u32, String)>,
    pub entries: Vec<RefEntry>,
}

/// Рендерит объявление класса по записи сноски.
pub fn class_source(root: &Path, entry: &RefEntry) -> Option<ClassSource> {
    let path = entry.path.as_deref()?;
    let bytes = std::fs::read(path).ok()?;
    let file = path.to_string_lossy();
    let analysis = PhpBackend::with_defaults().analyze_source(&file, &bytes);
    let graph = analysis.graph;
    let namespace = graph.namespace.as_deref().filter(|ns| !ns.is_empty());
    let wanted = entry.fqcn.trim_start_matches('\\');
    let container = graph.containers.iter().find(|container| {
        let full = match namespace {
            Some(namespace) => format!("{namespace}\\{}", container.name),
            None => container.name.clone(),
        };
        full == wanted
    })?;
    let text = crate::io::read_source_lossy(path).ok()?;
    let source: Vec<&str> = text.lines().collect();
    let start = container.line;
    let end = container.end_line;
    let lines: Vec<(u32, String)> = (start..=end)
        .filter_map(|number| {
            source
                .get(number.saturating_sub(1) as usize)
                .map(|line| (number, (*line).to_owned()))
        })
        .collect();
    let (lines, entries) = annotate::annotate_lines(root, &file, &graph, &lines);
    let relative = path.strip_prefix(root).unwrap_or(path);
    let header = format!(
        "== {wanted}  {}  L{start}-{end}  [{} · {}]",
        relative.display(),
        entry.kind,
        entry.source
    );
    Some(ClassSource {
        header,
        lines,
        entries,
    })
}

/// Блок сносок для одного ответа (используется `refs`).
pub fn render_footnotes(entries: &[RefEntry], root: &Path) -> String {
    footnotes(entries, root)
}

/// Сохранённый набор сносок по id (`rN` или `N`).
pub fn book(root: &Path, id: u64) -> Option<RefBook> {
    store::load(root, id)
}

/// Параметр `rN` / `N` → номер.
pub fn parse_id(text: &str) -> Option<u64> {
    text.trim().trim_start_matches('r').parse().ok()
}

//! A locate bemenete: a projekt minden feldolgozott fájljának szűkített gráfja. Alapból a projekt-gráfból
//! (`facts.bin`; előtte a változott / új / törölt fájlok frissítve, mint a `callers`-nél — forrásfájl elemzése csak a
//! változottaknál), gráf nélkül (`CALLGRAPH_NO_GRAPH=1`, írásvédett projekt) minden fájl elemzésével. A bejárás a
//! `FileWalker::for_registry` szabályai szerint (`.callgraphignore`, generált fájlok); bináris fájl kimarad.

use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::analysis::Analyzer;
use crate::error::Result;
use crate::io::FileWalker;
use crate::lang::LanguageRole;
use crate::model::FileGraph;

/// Каталоги, которые пропускаем и в vendor (кроме самого vendor).
fn vendor_skip_dirs() -> Vec<String> {
    [
        "node_modules",
        ".git",
        "cache",
        "runtime",
        "tmp",
        ".idea",
        ".junie",
        "__pycache__",
        "venv",
        ".venv",
        "site-packages",
        "testdata",
    ]
    .map(String::from)
    .to_vec()
}

/// Egy fájl a kereséshez: az abszolút út, a gráfja, és hogy kódfájl-e (nem dokumentum / adat).
pub(crate) struct SourceFile<'g> {
    pub path: &'g Path,
    pub graph: &'g FileGraph,
    pub code: bool,
}

/// A projekt fájljainak forrása (a gyökér és az elemző befecskendezve).
pub(crate) struct ProjectSource<'a> {
    root: &'a Path,
    analyzer: &'a Analyzer,
    /// A projekt-gráfból olvasson-e (`CALLGRAPH_NO_GRAPH=1` kikapcsolja).
    graph: bool,
    /// Индексировать ли vendor (по умолчанию нет: `.gitignore`).
    vendor: bool,
}

impl<'a> ProjectSource<'a> {
    pub(crate) fn new(root: &'a Path, analyzer: &'a Analyzer) -> Self {
        Self {
            root,
            analyzer,
            graph: std::env::var_os("CALLGRAPH_NO_GRAPH").is_none(),
            vendor: false,
        }
    }

    /// `--with-vendor`: vendor-файлы разбираются отдельно и не попадают в проектный граф.
    pub(crate) fn with_vendor(mut self, vendor: bool) -> Self {
        self.vendor = vendor;
        self
    }

    /// A `visit` a projekt fájljaival (a bejárás sorrendjében); a gráfok kölcsönzöttek, a hívás idejére.
    pub(crate) fn with_files<R>(&self, visit: impl FnOnce(&[SourceFile<'_>]) -> R) -> Result<R> {
        let registry = self.analyzer.registry();
        let files = crate::graph::legacy_order(
            registry,
            FileWalker::for_registry(registry).collect(&[self.root.to_path_buf()], &mut |_| {}),
        );
        let shared = if self.graph {
            crate::graph::shared(self.root)
        } else {
            None
        };
        if let Some(shared) = shared {
            let mut graph = crate::graph::lock(&shared);
            graph.refresh(&files, true, self.analyzer)?;
            graph.load_facts(&files, self.analyzer)?;
            // a mentés hibája (pl. közben írásvédetté vált tároló) nem akasztja meg a választ
            let _ = graph.save();
            let rows: Vec<SourceFile<'_>> = files
                .iter()
                .filter_map(|path| {
                    let stored = graph.slim_graph(&path.to_string_lossy())?;
                    Some(self.entry(path, stored))
                })
                .filter(|file| !file.graph.binary)
                .collect();
            if !self.vendor {
                return Ok(visit(&rows));
            }
            let owned = self.vendor_graphs();
            let vendor_rows: Vec<SourceFile<'_>> = owned
                .iter()
                .filter(|(_, graph)| !graph.binary)
                .map(|(path, graph)| self.entry(path, graph))
                .collect();
            let combined: Vec<SourceFile<'_>> = rows.into_iter().chain(vendor_rows).collect();
            return Ok(visit(&combined));
        }
        let graphs: Vec<(&PathBuf, FileGraph)> = files
            .par_iter()
            .filter_map(|path| Some((path, self.analyzer.analyze_path(path).ok()?.graph)))
            .collect();
        let mut rows: Vec<SourceFile<'_>> = graphs
            .iter()
            .filter(|(_, graph)| !graph.binary)
            .map(|(path, graph)| self.entry(path, graph))
            .collect();
        let owned = if self.vendor {
            self.vendor_graphs()
        } else {
            Vec::new()
        };
        let vendor_rows: Vec<SourceFile<'_>> = owned
            .iter()
            .filter(|(_, graph)| !graph.binary)
            .map(|(path, graph)| self.entry(path, graph))
            .collect();
        rows.extend(vendor_rows);
        Ok(visit(&rows))
    }

    /// Файлы vendor: отдельный разбор, без записи в проектный граф.
    fn vendor_graphs(&self) -> Vec<(PathBuf, FileGraph)> {
        let map = crate::autoload::AutoloadMap::discover(self.root);
        let mut dirs: Vec<PathBuf> = map
            .roots()
            .map(|(_, vendor)| vendor.to_path_buf())
            .collect();
        dirs.sort();
        dirs.dedup();
        let mut files = Vec::new();
        for dir in dirs {
            if !dir.is_dir() {
                continue;
            }
            let walker = FileWalker::for_registry(self.analyzer.registry())
                .with_gitignore(false)
                .with_skip_dirs(vendor_skip_dirs());
            files.extend(walker.collect(&[dir], &mut |_| {}));
        }
        files
            .par_iter()
            .filter_map(|path| Some((path.clone(), self.analyzer.analyze_path(path).ok()?.graph)))
            .collect()
    }

    fn entry<'g>(&self, path: &'g Path, graph: &'g FileGraph) -> SourceFile<'g> {
        let code = self
            .analyzer
            .registry()
            .for_path(path)
            .is_some_and(|language| language.role() != LanguageRole::Document);
        SourceFile { path, graph, code }
    }
}

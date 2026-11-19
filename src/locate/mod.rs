//! `locate`: kiindulópont-kereső. Az agent a feladatából kiszedett neveket és fogalmakat (függvény-, osztály-,
//! fájlnevek, domain-szavak) egy hívásban adja át; a válasz a kész projekt-térképből (forrásfájl olvasása és
//! újraelemzés nélkül) egy rövid, rangsorolt fájl-lista, fájlonként az indoklással.
//!
//! Felépítés (Facade: `Locator`): `source` (a projekt fájljainak szűkített gráfja) → `callers` (név szerinti
//! hívószám) → `matcher` (fájlonkénti egyezések) → `rank` (összesítés, rangsor) → `reasons` + `format` (kimenet).
//! A kifejezések bontása és az egyes / többes szám tűrés: `terms`. A név-index kérésenként a memóriában készül
//! (a kifejezések előkészítve, a tények egy menetben); perzisztálni ráér, ha a mérés szerint kell.

mod callers;
mod format;
mod matcher;
mod rank;
mod reasons;
mod source;
mod terms;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::analysis::Analyzer;
use crate::error::{CallgraphError, Result};
use crate::packages::Inertia;

use callers::CallerCounts;
use format::Report;
use matcher::Matcher;
use rank::{FileMatch, rank};
use reasons::{describe, group, pick};
use terms::Term;

// a projekt fájljainak forrása: a `uses` csomag-használat is ezen át járja be a projekt-gráfot
pub(crate) use source::ProjectSource;

/// Сырые данные ответа `locate` (для текстового, `--files` и `--json` вывода).
pub(crate) struct Located {
    pub root: PathBuf,
    pub terms: Vec<Term>,
    pub ranked: Vec<FileMatch>,
    pub notes: HashMap<String, usize>,
    pub issues: Vec<(String, u32)>,
}

/// A kiindulópont-kereső egy projekt-gyökérre.
pub struct Locator {
    root: PathBuf,
    analyzer: Analyzer,
}

impl Locator {
    /// A (kanonikus alakra hozott) gyökérre.
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
            analyzer: Analyzer::with_defaults(),
        }
    }

    /// A kifejezésekre adott válasz szövege és a szintaktikai hibás fájlok száma (`--strict`); `all`: minden egyező
    /// fájl (alapból az első 5). Kifejezés nélkül használati hiba.
    pub fn run<S: AsRef<str>>(&self, inputs: &[S], all: bool) -> Result<(String, usize)> {
        self.run_options(inputs, all, false, false, false, false)
    }

    /// Как `run`, но с машинным выводом: `exact` — только точные определения/имена файлов,
    /// `files` — по пути на строку, `json` — структура.
    pub fn run_options<S: AsRef<str>>(
        &self,
        inputs: &[S],
        all: bool,
        exact: bool,
        files: bool,
        json: bool,
        with_vendor: bool,
    ) -> Result<(String, usize)> {
        let located = self.data(inputs, exact, with_vendor)?;
        let issues = located.issues.len();
        if files {
            let mut text = String::new();
            for file in &located.ranked {
                text.push_str(&file.path);
                text.push('\n');
            }
            return Ok((text, issues));
        }
        if json {
            let items: Vec<serde_json::Value> = located
                .ranked
                .iter()
                .map(|file| {
                    let reasons: Vec<String> = pick(group(&file.hits), 3)
                        .iter()
                        .map(|reason| describe(reason, &located.terms))
                        .collect();
                    serde_json::json!({
                        "path": file.path,
                        "score": file.score,
                        "exact": file.has_exact(),
                        "reasons": reasons,
                    })
                })
                .collect();
            let text = serde_json::json!({
                "root": located.root,
                "files": items,
            })
            .to_string();
            return Ok((text, issues));
        }
        let root = located.root.to_string_lossy();
        let shown = crate::query::strip_verbatim(&root);
        let branch = crate::query::branch::root_suffix(&located.root);
        let mut text = crate::tf!("root: {shown}{branch}\n", "gyökér: {shown}{branch}\n");
        let report = Report {
            terms: &located.terms,
            ranked: &located.ranked,
            all,
            notes: &located.notes,
            root: &located.root,
        };
        text.push_str(&report.render());
        text.push_str(&crate::query::format::parse_trailer(
            &located.issues,
            crate::i18n::lang(),
        ));
        Ok((text, issues))
    }

    /// Сырые данные locate: термины, ранжированные файлы, заметки и ошибки разбора.
    pub(crate) fn data<S: AsRef<str>>(
        &self,
        inputs: &[S],
        exact: bool,
        with_vendor: bool,
    ) -> Result<Located> {
        let terms = Term::parse_all(inputs);
        if terms.is_empty() {
            return Err(CallgraphError::Usage(usage().into()));
        }
        // jegyzet-jelölők (`  📝 N`) a fájl-sorokon: egyszer betöltve; jegyzetfájl nélkül üres
        let marks = crate::notes::NoteMarks::load(&self.root);
        // a csomagtérkép (a „pkg” indokláshoz): egyszer, a `.callgraph/packages.bin`-ből; manifest nélkül nincs
        let packages = crate::packages::open(&self.root);
        let packages = packages.has_parts().then_some(&packages);
        // Inertia-oldalak (a „page ←” indokláshoz): csak Inertia-projektben, a render-helyek a bejárt tényekből
        let mut inertia = packages.and_then(|packages| {
            Inertia::detect(&self.root, packages, &crate::packages::DiskProjectFiles)
        });
        let mut notes: HashMap<String, usize> = HashMap::new();
        let (mut ranked, issues) = ProjectSource::new(&self.root, &self.analyzer)
            .with_vendor(with_vendor)
            .with_files(|files| {
                if let Some(inertia) = inertia.as_mut() {
                    let code = files
                        .iter()
                        .filter(|file| file.code)
                        .map(|file| (file.path, file.graph));
                    inertia.scan(code, &crate::packages::DiskProjectFiles);
                }
                let inertia = inertia.as_ref();
                let issues: Vec<(String, u32)> = files
                    .iter()
                    .filter_map(|file| {
                        crate::query::format::parse_issue(&self.root, file.path, file.graph)
                    })
                    .collect();
                let callers = CallerCounts::build(files.iter().map(|file| file.graph));
                let matcher = Matcher::new(&terms, &callers).with_packages(packages);
                let matched: Vec<FileMatch> = files
                    .iter()
                    .filter_map(|file| {
                        let rel = self.relative(file.path);
                        let mut hits = matcher.file_hits(&rel, file.graph, file.code);
                        if file.code {
                            matcher.package_hits(file.path, file.graph, &mut hits);
                            if let Some(inertia) = inertia {
                                matcher.page_hits(&rel, inertia, &mut hits);
                            }
                        }
                        if !hits.is_empty() && !marks.is_empty() {
                            let methods = file.graph.methods.iter();
                            let n = crate::notes::live_count(
                                marks.of(&rel),
                                methods.map(|m| (m.container.as_deref(), m.name.as_str())),
                            );
                            if n > 0 {
                                notes.insert(rel.clone(), n);
                            }
                        }
                        let multi = matcher.multi_terms(&hits);
                        FileMatch::new(rel, hits).map(|file| file.with_multi(multi))
                    })
                    .collect();
                let ranked = rank(matched, rank::is_test_path);
                (ranked, issues)
            })?;
        if exact {
            ranked.retain(FileMatch::has_exact);
        }
        Ok(Located {
            root: self.root.clone(),
            terms,
            ranked,
            notes,
            issues,
        })
    }

    /// Az út a gyökérhez képest (a fájlnév- és út-egyezéshez).
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

/// A parancs használata (a CLI és a hibaüzenet közös szövege).
pub(crate) fn usage() -> &'static str {
    crate::t!(
        "callgraph locate <term>... [--all] [--project <alias|dir>]  (names and domain terms from the task: functions, classes, files, columns, routes; \"a quoted phrase\" = one term, matched as a whole against strings; route::<name> = a route name: its definition and route() calls; --name = a CSS custom property)",
        "callgraph locate <kifejezés>... [--all] [--project <alias|mappa>]  (a feladat nevei és fogalmai: függvények, osztályok, fájlok, oszlopok, útvonalak; \"idézőjeles kifejezés\" = egyetlen kifejezés, egészében a stringekre illesztve; route::<név> = útvonalnév: a definíciója és a route() hívásai; --nev = CSS custom property)"
    )
}

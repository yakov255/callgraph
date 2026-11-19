//! `callgraph find impl|sub|trait <Symbol>`: точный список реализаций интерфейса, наследников
//! класса и пользователей trait — по контейнерам графа, без ранжирования `locate`.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::Command;
use crate::error::{CallgraphError, Result};
use crate::query::{QueryEngine, SelectorSet};
use crate::skill::SkillHome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Implements,
    Extends,
    Trait,
}

impl Relation {
    /// `impl` / `sub` / `trait` → вид связи.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "impl" | "implements" => Some(Self::Implements),
            "sub" | "extends" | "subclasses" => Some(Self::Extends),
            "trait" | "traits" => Some(Self::Trait),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Implements => "implements",
            Self::Extends => "extends",
            Self::Trait => "trait",
        }
    }
}

pub struct FindRelationCommand {
    relation: Relation,
    symbol: String,
    files_only: bool,
    json: bool,
    project: Option<String>,
    home: SkillHome,
}

impl FindRelationCommand {
    pub fn new(
        relation: Relation,
        symbol: String,
        files_only: bool,
        json: bool,
        project: Option<String>,
        home: SkillHome,
    ) -> Self {
        Self {
            relation,
            symbol,
            files_only,
            json,
            project,
            home,
        }
    }
}

impl Command for FindRelationCommand {
    fn run(&self, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
        let root = super::project_root(&self.home, self.project.as_deref())?;
        let engine = QueryEngine::new(Some(root.as_path()), None).use_project_graph(true);
        let selectors = SelectorSet::parse(&[PathBuf::from(".")])
            .map_err(|error| CallgraphError::Usage(error.to_string()))?;
        let rows = engine.query(&selectors)?;
        let mut hits: Vec<(String, u32, String)> = Vec::new();
        for row in &rows {
            of_kind(&row.graph, self.relation, &self.symbol, &mut hits);
        }
        hits.sort();
        hits.dedup();
        if self.json {
            let items: Vec<serde_json::Value> = hits
                .iter()
                .map(|(file, line, name)| {
                    serde_json::json!({"file": file, "line": line, "type": name})
                })
                .collect();
            writeln!(
                out,
                "{}",
                serde_json::json!({
                    "relation": self.relation.label(),
                    "symbol": self.symbol,
                    "types": items,
                })
            )?;
            return Ok(0);
        }
        if self.files_only {
            let mut files: Vec<&str> = hits.iter().map(|(file, _, _)| file.as_str()).collect();
            files.dedup();
            for file in files {
                writeln!(out, "{}", relative(&root, Path::new(file)))?;
            }
            return Ok(0);
        }
        writeln!(
            out,
            "{}",
            crate::tf!(
                "{}: {} · {} types",
                "{}: {} · {} típus",
                self.relation.label(),
                self.symbol,
                hits.len()
            )
        )?;
        for (file, line, name) in &hits {
            writeln!(
                out,
                "{}:{}  {name} {} {}",
                relative(&root, Path::new(file)),
                line,
                self.relation.label(),
                self.symbol
            )?;
        }
        if hits.is_empty() {
            writeln!(err, "{}", crate::t!("no matches", "nincs találat"))?;
        }
        Ok(0)
    }
}

fn of_kind(
    graph: &crate::model::FileGraph,
    relation: Relation,
    symbol: &str,
    hits: &mut Vec<(String, u32, String)>,
) {
    for container in &graph.containers {
        let list = match relation {
            Relation::Implements => container.implements.as_ref(),
            Relation::Extends => container.extends.as_ref(),
            Relation::Trait => container.traits.as_ref(),
        };
        let matches = list.is_some_and(|list| list.iter().any(|name| same(name, symbol)));
        if matches {
            hits.push((graph.file.clone(), container.line, container.name.clone()));
        }
    }
}

/// Сравнение FQCN: полное совпадение или совпадение по последнему сегменту.
fn same(full: &str, wanted: &str) -> bool {
    let full = full.trim_start_matches('\\');
    let wanted = wanted.trim_start_matches('\\');
    full.eq_ignore_ascii_case(wanted)
        || full
            .rsplit('\\')
            .next()
            .is_some_and(|tail| tail.eq_ignore_ascii_case(wanted))
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::same;

    #[test]
    fn matches_full_and_short_names() {
        assert!(same("Layer\\Domain\\Foo", "Layer\\Domain\\Foo"));
        assert!(same("Layer\\Domain\\Foo", "Foo"));
        assert!(same("\\Layer\\Domain\\Foo", "Foo"));
        assert!(!same("Layer\\Domain\\Foo", "Bar"));
    }
}

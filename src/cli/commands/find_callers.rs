//! `callgraph find callers <name> --json`: машинный список прямых вызовов (глубина 1).
//!
//! Для `--depth > 1` остаётся текстовый вывод `find callers`: JSON описывает ровно один уровень.

use std::io::Write;
use std::path::PathBuf;

use crate::cli::Command;
use crate::error::{CallgraphError, Result};
use crate::query::{QueryEngine, SelectorSet};
use crate::skill::SkillHome;

pub struct FindCallersJsonCommand {
    name: String,
    project: Option<String>,
    home: SkillHome,
}

impl FindCallersJsonCommand {
    pub fn new(name: String, project: Option<String>, home: SkillHome) -> Self {
        Self {
            name,
            project,
            home,
        }
    }
}

impl Command for FindCallersJsonCommand {
    fn run(&self, out: &mut dyn Write, _err: &mut dyn Write) -> Result<i32> {
        let root = super::project_root(&self.home, self.project.as_deref())?;
        let engine = QueryEngine::new(Some(root.as_path()), None).use_project_graph(true);
        let selectors = SelectorSet::parse(&[PathBuf::from(".")])
            .map_err(|error| CallgraphError::Usage(error.to_string()))?;
        let rows = engine.query_filtered(&selectors, Some(&self.name), None)?;
        let mut items = Vec::new();
        for row in &rows {
            for call in &row.graph.calls {
                if call.is_dynamic() {
                    continue;
                }
                items.push(serde_json::json!({
                    "file": row.graph.file,
                    "caller": call.caller,
                    "container": call.container,
                    "line": call.line,
                    "target": call.target,
                    "resolved": call.resolved,
                    "category": call.category,
                }));
            }
        }
        writeln!(
            out,
            "{}",
            serde_json::json!({"pattern": self.name, "callers": items})
        )?;
        Ok(0)
    }
}

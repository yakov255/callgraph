//! `callgraph context <file>:<line> [-n N]`: строки вокруг места с номерами и границами метода.
//!
//! Замена `sed -n '745,790p'`: окно вокруг строки, целевая строка помечена `>`, в заголовке —
//! метод, в который попадает строка (если он есть в карте).

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::Command;
use crate::error::{CallgraphError, Result};
use crate::query::{QueryEngine, SelectorSet};
use crate::skill::SkillHome;

pub struct ContextCommand {
    target: String,
    around: u32,
    project: Option<String>,
    home: SkillHome,
}

impl ContextCommand {
    pub fn new(target: String, around: u32, project: Option<String>, home: SkillHome) -> Self {
        Self {
            target,
            around,
            project,
            home,
        }
    }
}

impl Command for ContextCommand {
    fn run(&self, out: &mut dyn Write, _err: &mut dyn Write) -> Result<i32> {
        let root = super::project_root(&self.home, self.project.as_deref())?;
        let (file, line) = split_target(&self.target).ok_or_else(usage)?;
        let path = if Path::new(&file).is_absolute() {
            PathBuf::from(&file)
        } else {
            root.join(&file)
        };
        let text = crate::io::read_source_lossy(&path).map_err(|_| {
            CallgraphError::Usage(crate::tf!("Not found: {file}", "Nem található: {file}"))
        })?;
        let lines: Vec<&str> = text.lines().collect();
        if line == 0 || line as usize > lines.len() {
            return Err(CallgraphError::Usage(crate::tf!(
                "line {line} is outside the file",
                "a {line}. sor a fájlon kívül van"
            )));
        }
        let range = method_range(&root, &file, &path, line);
        let start = line.saturating_sub(self.around).max(1);
        let end = (line + self.around).min(lines.len() as u32);
        let relative = path.strip_prefix(&root).unwrap_or(&path).display();
        match &range {
            Some((container, method, first, last)) => {
                writeln!(out, "== {relative}  {container}::{method}  L{first}-{last}")?
            }
            None => writeln!(out, "== {relative}  L{start}-{end}")?,
        }
        for number in start..=end {
            let mark = if number == line { '>' } else { ' ' };
            writeln!(
                out,
                "{mark}{number:>5}  {}",
                lines.get(number as usize - 1).copied().unwrap_or_default()
            )?;
        }
        Ok(0)
    }
}

/// Метод, в который попадает строка: `(container, name, start, end)`.
fn method_range(
    root: &Path,
    file: &str,
    path: &Path,
    line: u32,
) -> Option<(String, String, u32, u32)> {
    let selector = if path.is_absolute() {
        path.display().to_string()
    } else {
        file.to_owned()
    };
    let engine = QueryEngine::new(Some(root), None).use_project_graph(true);
    let selectors = SelectorSet::parse(&[PathBuf::from(selector)]).ok()?;
    let rows = engine.query(&selectors).ok()?;
    let graph = &rows.first()?.graph;
    let method = graph
        .methods
        .iter()
        .filter(|method| {
            let end = method.end_line.unwrap_or(method.line);
            method.line <= line && line <= end
        })
        .min_by_key(|method| method.end_line.unwrap_or(method.line) - method.line)?;
    Some((
        method
            .container
            .clone()
            .unwrap_or_else(|| "<file>".to_owned()),
        method.name.clone(),
        method.line,
        method.end_line.unwrap_or(method.line),
    ))
}

fn split_target(value: &str) -> Option<(String, u32)> {
    let (file, line) = value.rsplit_once(':')?;
    let line = line.trim().parse().ok()?;
    (!file.is_empty()).then_some((file.to_owned(), line))
}

fn usage() -> CallgraphError {
    CallgraphError::Usage(
        crate::t!(
            "callgraph context <file>:<line> [-n N] [--project <alias|dir>]",
            "callgraph context <fájl>:<sor> [-n N] [--project <alias|mappa>]"
        )
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::split_target;

    #[test]
    fn splits_file_and_line() {
        assert_eq!(
            split_target("src/A.php:120"),
            Some(("src/A.php".to_owned(), 120))
        );
        assert_eq!(
            split_target("Layer/A.php:7"),
            Some(("Layer/A.php".to_owned(), 7))
        );
        assert_eq!(split_target("A.php"), None);
        assert_eq!(split_target("A.php:x"), None);
    }
}

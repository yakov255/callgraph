//! `callgraph find path <glob>`: поиск файлов по пути/имени (замена `find -path`), без AST.
//!
//! Шаблон: `*`, `?`, `**`, `{a,b}`. Без `/` сверяется имя файла, с `/` — путь от корня проекта.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::Command;
use crate::error::Result;
use crate::skill::SkillHome;

use super::find_text::{SKIP, expand_globs, glob_match};

pub struct FindPathCommand {
    glob: String,
    json: bool,
    project: Option<String>,
    home: SkillHome,
}

impl FindPathCommand {
    pub fn new(glob: String, json: bool, project: Option<String>, home: SkillHome) -> Self {
        Self {
            glob,
            json,
            project,
            home,
        }
    }
}

impl Command for FindPathCommand {
    fn run(&self, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
        let root = super::project_root(&self.home, self.project.as_deref())?;
        let globs = expand_globs(std::slice::from_ref(&self.glob));
        let mut found = Vec::new();
        walk(&root, &root, &globs, &mut found);
        found.sort();
        if self.json {
            let items: Vec<&String> = found.iter().collect();
            writeln!(
                out,
                "{}",
                serde_json::json!({"glob": self.glob, "files": items})
            )?;
            return Ok(0);
        }
        for path in &found {
            writeln!(out, "{path}")?;
        }
        if found.is_empty() {
            writeln!(err, "{}", crate::t!("no matches", "nincs találat"))?;
        }
        Ok(0)
    }
}

fn walk(root: &Path, dir: &Path, globs: &[String], out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if path.is_dir() {
            if !SKIP.contains(&name) && !name.starts_with('.') {
                walk(root, &path, globs, out);
            }
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let matched = globs.iter().any(|glob| {
            if glob.contains('/') {
                glob_match(glob, &relative)
            } else {
                glob_match(glob, name)
            }
        });
        if matched {
            out.push(relative);
        }
    }
}

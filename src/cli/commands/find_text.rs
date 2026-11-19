//! `callgraph find text <pattern> [--glob '*.neon,*.yaml'] [--files] [--json]`: текстовый слой
//! для файлов, которых нет в AST-графе (`.neon`, `.yaml`, `.xml`, `.feature`, `.env`, `.dist`).
//!
//! Подстрокa, без regex: для регулярки остаётся `rg`. Уважает те же пропускаемые каталоги
//! (`vendor`, `node_modules`, `.git`, `.callgraph`, `target`).

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::Command;
use crate::error::{CallgraphError, Result};
use crate::skill::SkillHome;

/// Каталоги, которые не обходим.
pub(crate) const SKIP: &[&str] = &[
    "vendor",
    "node_modules",
    ".git",
    ".callgraph",
    ".idea",
    ".vscode",
    "target",
    ".cache",
];

/// Максимальный размер файла: большие бинарники/логи не читаем.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

pub struct FindTextCommand {
    pattern: String,
    globs: Option<Vec<String>>,
    files_only: bool,
    json: bool,
    project: Option<String>,
    home: SkillHome,
}

impl FindTextCommand {
    pub fn new(
        pattern: String,
        globs: Option<Vec<String>>,
        files_only: bool,
        json: bool,
        project: Option<String>,
        home: SkillHome,
    ) -> Self {
        Self {
            pattern,
            globs,
            files_only,
            json,
            project,
            home,
        }
    }
}

impl Command for FindTextCommand {
    fn run(&self, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
        let root = super::project_root(&self.home, self.project.as_deref())?;
        let globs = self.globs.as_deref().map(expand_globs);
        let mut hits = Vec::new();
        walk(&root, &root, globs.as_deref(), &self.pattern, &mut hits);
        if self.json {
            let items: Vec<serde_json::Value> = hits
                .iter()
                .map(|(file, line, text)| {
                    serde_json::json!({"file": file, "line": line, "text": text})
                })
                .collect();
            writeln!(
                out,
                "{}",
                serde_json::json!({"pattern": self.pattern, "hits": items})
            )?;
            return Ok(0);
        }
        if self.files_only {
            let files: BTreeSet<&str> = hits.iter().map(|(file, _, _)| file.as_str()).collect();
            for file in files {
                writeln!(out, "{file}")?;
            }
            return Ok(0);
        }
        writeln!(
            out,
            "{}",
            crate::tf!(
                "text: {} · {} hits",
                "text: {} · {} találat",
                self.pattern,
                hits.len()
            )
        )?;
        for (file, line, text) in &hits {
            writeln!(out, "{file}:{line}  {text}")?;
        }
        if hits.is_empty() {
            writeln!(err, "{}", crate::t!("no matches", "nincs találat"))?;
        }
        Ok(0)
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    globs: Option<&[String]>,
    pattern: &str,
    hits: &mut Vec<(String, u32, String)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if !SKIP.contains(&name) && !name.starts_with('.') {
                walk(root, &path, globs, pattern, hits);
            }
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        if let Some(globs) = globs {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let matched = globs.iter().any(|glob| {
                if glob.contains('/') {
                    glob_match(glob, &relative)
                } else {
                    glob_match(glob, name)
                }
            });
            if !matched {
                continue;
            }
        }
        let Ok(meta) = path.metadata() else {
            continue;
        };
        if meta.len() > MAX_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.iter().take(1024).any(|byte| *byte == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        for (index, line) in text.lines().enumerate() {
            if line.contains(pattern) {
                hits.push((
                    relative.clone(),
                    index as u32 + 1,
                    line.trim().chars().take(200).collect(),
                ));
            }
        }
    }
}

/// `{neon,yaml}` → несколько шаблонов.
pub(crate) fn expand_globs(globs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for glob in globs {
        if let (Some(open), Some(close)) = (glob.find('{'), glob.find('}'))
            && open < close
        {
            let (head, tail) = (&glob[..open], &glob[close + 1..]);
            for item in glob[open + 1..close].split(',') {
                out.push(format!("{head}{item}{tail}"));
            }
            continue;
        }
        out.push(glob.clone());
    }
    out
}

/// Простой glob: `*` (кроме `/`), `?`, `**` (любая глубина).
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(pattern: &[char], text: &[char]) -> bool {
        match pattern.first() {
            None => text.is_empty(),
            Some('*') => {
                if pattern.get(1) == Some(&'*') {
                    // `**` — любые символы, включая `/`
                    let rest = &pattern[2..];
                    (0..=text.len()).any(|skip| go(rest, &text[skip..]))
                } else {
                    let rest = &pattern[1..];
                    go(rest, text)
                        || (!text.is_empty() && text[0] != '/' && go(pattern, &text[1..]))
                }
            }
            Some('?') => !text.is_empty() && text[0] != '/' && go(&pattern[1..], &text[1..]),
            Some(c) => text.first() == Some(c) && go(&pattern[1..], &text[1..]),
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    go(&pattern, &text)
}

/// Ошибка использования (для пустого шаблона).
pub fn usage_error() -> CallgraphError {
    CallgraphError::Usage(
        crate::t!(
            "callgraph find text <pattern> [--glob '*.neon,*.yaml'] [--files] [--json] [--project <alias|dir>]",
            "callgraph find text <minta> [--glob '*.neon,*.yaml'] [--files] [--json] [--project <alias|mappa>]"
        )
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::{expand_globs, glob_match};

    #[test]
    fn globs_match_names_and_paths() {
        assert!(glob_match("*.neon", "phpstan.neon"));
        assert!(!glob_match("*.neon", "app/.phpstan.neon"));
        assert!(glob_match("**/*.neon", "app/raketa/api/phpstan.neon"));
        assert!(glob_match("src/**/*.php", "src/a/b/C.php"));
        assert!(!glob_match("*.neon", "app/x.yaml"));
    }

    #[test]
    fn braces_expand_to_several_globs() {
        assert_eq!(
            expand_globs(&["*.{neon,yaml}".to_owned()]),
            ["*.neon".to_owned(), "*.yaml".to_owned()]
        );
    }
}

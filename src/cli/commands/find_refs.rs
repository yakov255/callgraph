//! `callgraph find refs <Symbol> [--kind use,type,…] [--files] [--json]`: все места, где
//! упоминается символ — не только вызовы.
//!
//! Категории: `use`, `new`, `instanceof`, `catch`, `implements`, `extends`, `class` (`X::class`),
//! `type` (свойства, параметры, возвраты), `docblock`, `comment`. Поиск идёт по исходникам
//! файлов проекта, поэтому находит и DI-массивы, и `@param`, и голые типы.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::Command;
use crate::error::{CallgraphError, Result};
use crate::query::{QueryEngine, SelectorSet};
use crate::skill::SkillHome;

/// Одно упоминание.
struct Hit {
    file: String,
    line: u32,
    kind: &'static str,
    text: String,
}

pub struct FindRefsCommand {
    symbol: String,
    kinds: Option<Vec<String>>,
    files_only: bool,
    json: bool,
    project: Option<String>,
    home: SkillHome,
}

impl FindRefsCommand {
    pub fn new(
        symbol: String,
        kinds: Option<Vec<String>>,
        files_only: bool,
        json: bool,
        project: Option<String>,
        home: SkillHome,
    ) -> Self {
        Self {
            symbol,
            kinds,
            files_only,
            json,
            project,
            home,
        }
    }
}

impl Command for FindRefsCommand {
    fn run(&self, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
        let root = super::project_root(&self.home, self.project.as_deref())?;
        let engine = QueryEngine::new(Some(root.as_path()), None).use_project_graph(true);
        let selectors = SelectorSet::parse(&[PathBuf::from(".")])
            .map_err(|error| CallgraphError::Usage(error.to_string()))?;
        let rows = engine.query(&selectors)?;
        let target = self.symbol.trim().trim_start_matches('\\');
        let short = target.rsplit('\\').next().unwrap_or(target);
        let fqcn = target.contains('\\').then_some(target);
        let mut hits = Vec::new();
        for row in &rows {
            let path = Path::new(&row.graph.file);
            let Ok(text) = crate::io::read_source_lossy(path) else {
                continue;
            };
            let names = match_names(fqcn, short, &row.graph);
            for (line_number, line) in text.lines().enumerate() {
                let number = line_number as u32 + 1;
                let Some(name) = names.iter().find(|name| has_word(line, name)) else {
                    continue;
                };
                if fqcn.is_some() && !resolves_to(line, name, fqcn, &row.graph) {
                    continue;
                }
                let kind = classify(line, name);
                if let Some(filter) = &self.kinds
                    && !filter.iter().any(|wanted| wanted == kind)
                {
                    continue;
                }
                hits.push(Hit {
                    file: row.graph.file.clone(),
                    line: number,
                    kind,
                    text: line.trim().chars().take(160).collect(),
                });
            }
        }
        hits.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
        hits.dedup_by(|a, b| a.file == b.file && a.line == b.line);
        if self.json {
            let items: Vec<serde_json::Value> = hits
                .iter()
                .map(|hit| {
                    serde_json::json!({
                        "file": hit.file,
                        "line": hit.line,
                        "kind": hit.kind,
                        "text": hit.text,
                    })
                })
                .collect();
            writeln!(
                out,
                "{}",
                serde_json::json!({"symbol": target, "refs": items})
            )?;
            return Ok(0);
        }
        if self.files_only {
            let files: BTreeSet<&str> = hits.iter().map(|hit| hit.file.as_str()).collect();
            for file in files {
                writeln!(out, "{}", relative(&root, Path::new(file)))?;
            }
            return Ok(0);
        }
        writeln!(
            out,
            "{}",
            crate::tf!(
                "refs: {target} · {} refs in {} files",
                "refs: {target} · {} hivatkozás {} fájlban",
                hits.len(),
                hits.iter()
                    .map(|hit| &hit.file)
                    .collect::<BTreeSet<_>>()
                    .len()
            )
        )?;
        for hit in &hits {
            writeln!(
                out,
                "{}:{}  [{}]  {}",
                relative(&root, Path::new(&hit.file)),
                hit.line,
                hit.kind,
                hit.text
            )?;
        }
        if hits.is_empty() {
            writeln!(err, "{}", crate::t!("no references", "nincs hivatkozás"))?;
        }
        Ok(0)
    }
}

/// Имена, которые могут встретиться в файле: короткое имя плюс алиасы данного FQCN.
fn match_names(fqcn: Option<&str>, short: &str, graph: &crate::model::FileGraph) -> Vec<String> {
    let mut names = vec![short.to_owned()];
    if let Some(fqcn) = fqcn {
        names.push(fqcn.to_owned());
        for alias in aliases(fqcn, graph) {
            if !names.contains(&alias) {
                names.push(alias);
            }
        }
    }
    names
}

/// Алиасы файла, указывающие на данный FQCN.
fn aliases(fqcn: &str, graph: &crate::model::FileGraph) -> Vec<String> {
    let mut out = Vec::new();
    for (alias, full) in graph.imports.parts().0 {
        if full.trim_start_matches('\\') == fqcn {
            out.push(alias.to_owned());
        }
    }
    out
}

/// Проверяет, что имя в файле действительно ведёт к искомому FQCN.
fn resolves_to(
    line: &str,
    name: &str,
    fqcn: Option<&str>,
    graph: &crate::model::FileGraph,
) -> bool {
    let Some(fqcn) = fqcn else {
        return true;
    };
    // полное имя написано прямо в строке
    if name.trim_start_matches('\\') == fqcn {
        return true;
    }
    // алиас из imports — уже проверен при сборе имён
    if name != fqcn.rsplit('\\').next().unwrap_or(fqcn) {
        return true;
    }
    // короткое имя: либо импортировано как этот FQCN, либо тот же namespace
    if graph
        .imports
        .get(name)
        .is_some_and(|full| full.trim_start_matches('\\') == fqcn)
    {
        return true;
    }
    match graph.namespace.as_deref().filter(|ns| !ns.is_empty()) {
        Some(namespace) => format!("{namespace}\\{name}").trim_start_matches('\\') == fqcn,
        None => name == fqcn || !line.contains('\\'),
    }
}

/// Содержит ли строка имя как отдельное слово.
fn has_word(line: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(at) = line[from..].find(name) {
        let start = from + at;
        let before = line[..start].chars().next_back();
        let after = line[start + name.len()..].chars().next();
        // `\` — разделитель namespace, а не часть имени: `\Foo\Bar` находит `Bar`.
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if boundary(before) && boundary(after) {
            return true;
        }
        from = start + name.len();
    }
    false
}

/// Категория упоминания по строке.
fn classify(line: &str, name: &str) -> &'static str {
    let trimmed = line.trim_start();
    let comment = trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with('*')
        || trimmed.starts_with("/*")
        || trimmed.starts_with("<!--");
    if comment {
        return if trimmed.starts_with('*')
            && (line.contains("@param")
                || line.contains("@return")
                || line.contains("@var")
                || line.contains("@throws"))
        {
            "docblock"
        } else {
            "comment"
        };
    }
    if trimmed.starts_with("use ") || trimmed.starts_with("use\t") {
        return "use";
    }
    if line.contains("new ") && line.contains(name) {
        return "new";
    }
    if line.contains("instanceof ") && line.contains(name) {
        return "instanceof";
    }
    if line.contains("catch ") || line.contains("catch(") {
        return "catch";
    }
    if line.contains(&format!("class {name}"))
        || line.contains(&format!("interface {name}"))
        || line.contains(&format!("trait {name}"))
        || line.contains(&format!("enum {name}"))
    {
        return "definition";
    }
    if line.contains("implements") {
        return "implements";
    }
    if line.contains(" extends ") {
        return "extends";
    }
    if let Some(at) = line.find(&format!("{name}::")) {
        let after = &line[at + name.len() + 2..];
        let token: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if token == "class" {
            return "class";
        }
        if !token.is_empty()
            && token
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        {
            return "class-const";
        }
        if !token.is_empty() {
            return "static";
        }
    }
    "type"
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn classifies_reference_categories() {
        assert_eq!(classify("use Foo\\Bar;", "Bar"), "use");
        assert_eq!(classify("        return new Bar();", "Bar"), "new");
        assert_eq!(classify("if ($x instanceof Bar) {", "Bar"), "instanceof");
        assert_eq!(classify("} catch (Bar $e) {", "Bar"), "catch");
        assert_eq!(classify("class A implements Bar", "Bar"), "implements");
        assert_eq!(classify("class A extends Bar", "Bar"), "extends");
        assert_eq!(classify("$x = Bar::class;", "Bar"), "class");
        assert_eq!(classify("$x = Bar::LIMIT;", "Bar"), "class-const");
        assert_eq!(classify("private ?Bar $bar;", "Bar"), "type");
        assert_eq!(classify("     * @param Bar $bar", "Bar"), "docblock");
        assert_eq!(classify("// Bar is gone", "Bar"), "comment");
        assert_eq!(classify("class Bar extends Base", "Bar"), "definition");
    }
}

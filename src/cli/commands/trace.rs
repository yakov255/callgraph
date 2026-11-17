//! `callgraph trace <term>... [--project <alias|путь>] [--json] [--full]`: цепочки потока данных от источника
//! (ключ массива / колонка / property) до места чтения (`crate::flow`).
//!
//! Термы группируются в порядке аргументов; внутри терма порядок задаёт `Index::traces`.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

use crate::cli::Command;
use crate::error::Result;
use crate::flow::{Index, Trace, scan_flow, scan_parents};
use crate::skill::SkillHome;

/// Верхняя граница числа блоков (текст) и элементов (JSON) без `--full`.
const LIMIT: usize = 200;

pub struct TraceCommand {
    terms: Vec<String>,
    project: Option<String>,
    json: bool,
    full: bool,
    must_only: bool,
    home: SkillHome,
}

impl TraceCommand {
    pub fn new(
        terms: Vec<String>,
        project: Option<String>,
        json: bool,
        full: bool,
        home: SkillHome,
    ) -> Self {
        Self {
            terms,
            project,
            json,
            full,
            must_only: false,
            home,
        }
    }

    /// `--must`: только чтения, где колонка доходит на каждом пути.
    pub fn must_only(mut self, on: bool) -> Self {
        self.must_only = on;
        self
    }
}

impl Command for TraceCommand {
    fn run(&self, out: &mut dyn Write, _err: &mut dyn Write) -> Result<i32> {
        let root = super::project_root(&self.home, self.project.as_deref())?;
        let files = scan_flow(&root)?;
        let parents = scan_parents(&root)?;
        let index = Index::build_with_parents(&files, &parents);
        let groups: Vec<(String, Vec<Trace>)> = self
            .terms
            .iter()
            .map(|term| {
                let mut traces = index.traces(term);
                if self.must_only {
                    traces.retain(|trace| trace.must);
                }
                (term.clone(), traces)
            })
            .collect();
        if self.json {
            writeln!(out, "{}", render_json(&root, &groups, self.full)?)?;
            return Ok(0);
        }
        out.write_all(render_text(&root, &self.terms, &groups, self.full).as_bytes())?;
        Ok(0)
    }
}

/// Путь относительно `root`, если файл лежит внутри проекта; иначе — как есть.
fn relative(root: &Path, file: &str) -> String {
    Path::new(file)
        .strip_prefix(root)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| file.to_string())
}

/// Текстовый вывод: строка `root:`, затем по терму строка счётчика и блоки из трёх строк.
fn render_text(
    root: &Path,
    terms: &[String],
    groups: &[(String, Vec<Trace>)],
    full: bool,
) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "root: {}", root.display());
    let mut shown = 0;
    for (i, term) in terms.iter().enumerate() {
        let traces = groups.get(i).map(|(_, t)| t.as_slice()).unwrap_or(&[]);
        if traces.is_empty() {
            let _ = writeln!(text, "trace: {term} · no reads");
            continue;
        }
        let files: BTreeSet<&str> = traces.iter().map(|t| t.file.as_str()).collect();
        let must = traces.iter().filter(|t| t.must).count();
        if must > 0 {
            let _ = writeln!(
                text,
                "trace: {term} · {} reads in {} files · {must} must",
                traces.len(),
                files.len()
            );
        } else {
            let _ = writeln!(
                text,
                "trace: {term} · {} reads in {} files",
                traces.len(),
                files.len()
            );
        }
        for trace in traces {
            if !full && shown >= LIMIT {
                let _ = writeln!(text, "… more (--full)");
                return text;
            }
            let _ = writeln!(
                text,
                "{}  {}  L{}{}",
                relative(root, &trace.file),
                trace.method,
                trace.line,
                if trace.must { "  [must]" } else { "" }
            );
            let _ = writeln!(text, "  {}", trace.expr);
            let _ = writeln!(text, "  {}", trace.chain.join(" → "));
            shown += 1;
        }
    }
    text
}

/// JSON-вывод: один массив объектов, обрезанный тем же лимитом.
fn render_json(root: &Path, groups: &[(String, Vec<Trace>)], full: bool) -> Result<String> {
    let mut items: Vec<serde_json::Value> = Vec::new();
    'outer: for (term, traces) in groups {
        for trace in traces {
            if !full && items.len() >= LIMIT {
                break 'outer;
            }
            items.push(serde_json::json!({
                "term": term,
                "must": trace.must,
                "column": trace.column,
                "file": relative(root, &trace.file),
                "method": trace.method,
                "line": trace.line,
                "expr": trace.expr,
                "chain": trace.chain,
            }));
        }
    }
    Ok(serde_json::to_string_pretty(&items)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(file: &str, method: &str, line: u32, expr: &str, chain: &[&str]) -> Trace {
        Trace {
            column: "service_type".into(),
            must: false,
            chain: chain.iter().map(|s| s.to_string()).collect(),
            file: file.into(),
            method: method.into(),
            line,
            expr: expr.into(),
        }
    }

    fn one(term: &str, traces: Vec<Trace>) -> (Vec<String>, Vec<(String, Vec<Trace>)>) {
        (vec![term.to_string()], vec![(term.to_string(), traces)])
    }

    #[test]
    fn text_renders_relative_path_and_chain() {
        let root = Path::new("/project");
        let (terms, groups) = one(
            "service_type",
            vec![trace(
                "/project/src/Repository/QueueRepository.php",
                r"Api\Queue\Repository\QueueRepository::getById",
                60,
                "$row['service_type']",
                &[
                    "key:service_type",
                    r"c:Api\Queue\Repository::getByServiceType",
                    r"prop:Api\Queue\Repository::serviceType",
                ],
            )],
        );
        let text = render_text(root, &terms, &groups, false);
        assert!(text.starts_with("root: /project\n"));
        assert!(text.contains("trace: service_type · 1 reads in 1 files\n"));
        assert!(text.contains(concat!(
            "src/Repository/QueueRepository.php",
            "  Api\\Queue\\Repository\\QueueRepository::getById  L60\n",
            "  $row['service_type']\n",
            "  key:service_type → c:Api\\Queue\\Repository::getByServiceType",
            " → prop:Api\\Queue\\Repository::serviceType\n",
        )));
    }

    #[test]
    fn path_outside_root_stays_absolute() {
        let root = Path::new("/project");
        let (terms, groups) = one(
            "service_type",
            vec![trace("/other/x.php", "f", 1, "$x", &["lit"])],
        );
        let text = render_text(root, &terms, &groups, false);
        assert!(text.contains("/other/x.php  f  L1\n"));
    }

    #[test]
    fn may_trace_has_no_must_suffix() {
        let root = Path::new("/project");
        let (terms, groups) = one(
            "service_type",
            vec![trace("/project/src/a.php", "f", 64, "$x", &["lit"])],
        );
        let text = render_text(root, &terms, &groups, false);
        assert!(text.contains("src/a.php  f  L64\n"));
        assert!(!text.contains("[must]"));
    }

    #[test]
    fn must_trace_is_marked_in_text_and_json() {
        let root = Path::new("/project");
        let mut item = trace("/project/src/a.php", "f", 64, "$x", &["lit"]);
        item.must = true;
        let (terms, groups) = one("service_type", vec![item]);
        let text = render_text(root, &terms, &groups, false);
        assert!(text.contains("src/a.php  f  L64  [must]\n"));
        assert!(text.contains("trace: service_type · 1 reads in 1 files · 1 must\n"));
        let json = render_json(root, &groups, false).unwrap();
        assert!(json.contains("\"must\": true"));
    }

    #[test]
    fn empty_term_reports_no_reads() {
        let (terms, groups) = one("missing", Vec::new());
        let text = render_text(Path::new("/project"), &terms, &groups, false);
        assert!(text.contains("trace: missing · no reads\n"));
        assert_eq!(text.matches('\n').count(), 2);
    }

    #[test]
    fn terms_keep_given_order() {
        let terms = vec!["b".to_string(), "a".to_string()];
        let groups = vec![
            (
                "b".to_string(),
                vec![trace("/p/x.php", "f", 2, "$y", &["lit"])],
            ),
            (
                "a".to_string(),
                vec![trace("/p/x.php", "f", 1, "$x", &["lit"])],
            ),
        ];
        let text = render_text(Path::new("/p"), &terms, &groups, false);
        let first = text.find("trace: b").unwrap();
        let second = text.find("trace: a").unwrap();
        assert!(first < second);
    }

    #[test]
    fn cap_marks_truncated_output() {
        let traces: Vec<Trace> = (0..=LIMIT as u32)
            .map(|line| trace("/p/x.php", "f", line, "$x", &["lit"]))
            .collect();
        let (terms, groups) = one("service_type", traces);
        let capped = render_text(Path::new("/p"), &terms, &groups, false);
        assert!(capped.contains("… more (--full)\n"));
        assert_eq!(capped.matches("$x\n").count(), LIMIT);
        let full = render_text(Path::new("/p"), &terms, &groups, true);
        assert!(!full.contains("… more (--full)"));
        assert_eq!(full.matches("$x\n").count(), LIMIT + 1);
    }
}

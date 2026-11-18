//! Сноски типов в выводе: маркеры `[#k]` в сигнатурах и блок определений.
//!
//! Маркер ставится после каждого именованного типа; один класс — один номер в ответе.
//! Тип разрешается по imports/namespace файла, затем через composer-autoload (`crate::autoload`).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::autoload::AutoloadMap;
use crate::model::FileGraph;
use crate::query::QueryResult;

use super::store::{self, RefEntry};
use super::types;

/// Сноска в ответе.
struct Numbered {
    entries: Vec<RefEntry>,
    numbers: HashMap<String, usize>,
}

impl Numbered {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            numbers: HashMap::new(),
        }
    }

    /// Номер типа: существующий или новый (с резолвом через autoload).
    fn number(&mut self, fqcn: &str, map: &AutoloadMap, from: &Path) -> usize {
        if let Some(number) = self.numbers.get(fqcn) {
            return *number;
        }
        let number = self.entries.len() + 1;
        let hit = map.resolve(fqcn, Some(from));
        let entry = match hit {
            Some(hit) => RefEntry {
                number,
                fqcn: fqcn.to_owned(),
                path: Some(hit.path),
                kind: hit.kind.label().to_owned(),
                source: match hit.source {
                    crate::autoload::ClassSource::Own => "own".to_owned(),
                    crate::autoload::ClassSource::Vendor(name) => name,
                },
                line: None,
            },
            None if is_internal(fqcn) => RefEntry {
                number,
                fqcn: fqcn.to_owned(),
                path: None,
                kind: "internal".to_owned(),
                source: "php".to_owned(),
                line: None,
            },
            None => RefEntry {
                number,
                fqcn: fqcn.to_owned(),
                path: None,
                kind: "unknown".to_owned(),
                source: "unknown".to_owned(),
                line: None,
            },
        };
        self.numbers.insert(fqcn.to_owned(), number);
        self.entries.push(entry);
        number
    }
}

/// Вхождение типа в строке: текст типа и якорь (имя параметра / возвращаемое значение).
enum Anchor {
    Param(String),
    Return,
}

/// Сноски для одного файла.
struct Pass<'a> {
    map: &'a AutoloadMap,
    file: &'a str,
    graph: &'a FileGraph,
    numbered: &'a mut Numbered,
}

impl Pass<'_> {
    /// Обрабатывает одну строку с номером: вставляет маркеры в сигнатуру метода.
    fn line(&mut self, number: u32, text: &str) -> String {
        let Some(occurrences) = occurrences(self.graph, number) else {
            return text.to_owned();
        };
        let mut inserts: Vec<(usize, String)> = Vec::new();
        for (type_text, anchor) in occurrences {
            let at = match &anchor {
                Anchor::Param(variable) => {
                    let variable = format!("${variable}");
                    let Some(var_at) = text.find(&variable) else {
                        continue;
                    };
                    text[..var_at].rfind(type_text)
                }
                Anchor::Return => text.rfind(type_text),
            };
            let Some(start) = at else {
                continue;
            };
            for (_, end, name) in types::named(type_text) {
                let fqcn =
                    resolve_name(&name, self.graph, occurrence_container(self.graph, number));
                let id = self.numbered.number(&fqcn, self.map, Path::new(self.file));
                inserts.push((start + end, format!("[#{id}]")));
            }
        }
        insert_markers(text, inserts)
    }
}

/// Встроенные классы и интерфейсы PHP: исходника в проекте нет, но это не «неизвестный» тип.
fn is_internal(fqcn: &str) -> bool {
    const INTERNAL: &[&str] = &[
        "ArrayAccess",
        "ArrayIterator",
        "ArrayObject",
        "BackedEnum",
        "Closure",
        "Countable",
        "DateTime",
        "DateInterval",
        "DateTimeImmutable",
        "DateTimeInterface",
        "DateTimeZone",
        "DatePeriod",
        "Error",
        "Exception",
        "Generator",
        "Iterator",
        "IteratorAggregate",
        "JsonSerializable",
        "LogicException",
        "RuntimeException",
        "Stringable",
        "Throwable",
        "Traversable",
        "UnitEnum",
        "WeakMap",
        "stdClass",
    ];
    INTERNAL.contains(&fqcn.trim_start_matches('\\'))
}

/// Вставляет маркеры справа налево, чтобы не сбить байтовые позиции.
fn insert_markers(text: &str, mut inserts: Vec<(usize, String)>) -> String {
    if inserts.is_empty() {
        return text.to_owned();
    }
    inserts.sort_by_key(|(at, _)| *at);
    inserts.dedup_by_key(|(at, _)| *at);
    let mut out = text.to_owned();
    for (at, marker) in inserts.into_iter().rev() {
        if at <= out.len() && out.is_char_boundary(at) {
            out.insert_str(at, &marker);
        }
    }
    out
}

/// Типы сигнатуры метода на его строке: `(текст типа, якорь)`.
fn occurrences(graph: &FileGraph, line: u32) -> Option<Vec<(&str, Anchor)>> {
    let method = graph.methods.iter().find(|method| method.line == line)?;
    let mut out = Vec::new();
    for param in &method.params {
        let Some(dollar) = param.find('$') else {
            continue;
        };
        let variable = param[dollar + 1..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>();
        let raw = param[..dollar]
            .trim_end()
            .trim_end_matches("...")
            .trim_end();
        let ty = strip_modifiers(raw);
        if !ty.is_empty() {
            out.push((ty, Anchor::Param(variable)));
        }
    }
    if let Some(ret) = method
        .return_type
        .as_deref()
        .filter(|ret| !ret.trim().is_empty())
    {
        out.push((ret, Anchor::Return));
    }
    (!out.is_empty()).then_some(out)
}

/// Убирает модификаторы promoted-параметра (`private readonly Foo $x`).
fn strip_modifiers(text: &str) -> &str {
    let mut rest = text;
    loop {
        let trimmed = rest.trim_start();
        let Some(word) = trimmed.split_whitespace().next() else {
            return trimmed;
        };
        if matches!(
            word,
            "public" | "protected" | "private" | "readonly" | "final"
        ) {
            rest = &trimmed[word.len()..];
        } else {
            return trimmed;
        }
    }
}

/// Контейнер метода (для `self` / `parent`).
fn occurrence_container(graph: &FileGraph, line: u32) -> Option<&crate::model::Container> {
    let method = graph.methods.iter().find(|method| method.line == line)?;
    let name = method.container.as_deref()?;
    graph.containers.iter().find(|c| c.name == name)
}

/// Полное имя типа по imports и namespace файла.
fn resolve_name(
    name: &str,
    graph: &FileGraph,
    container: Option<&crate::model::Container>,
) -> String {
    let name = name.trim_start_matches('\\');
    match name.to_ascii_lowercase().as_str() {
        "self" | "static" => {
            if let Some(container) = container {
                return qualified(&container.name, graph);
            }
        }
        "parent" => {
            if let Some(parent) = container.and_then(|c| c.extends.as_ref())
                && let Some(parent) = parent.first()
            {
                return parent.trim_start_matches('\\').to_owned();
            }
        }
        _ => {}
    }
    let first = name.split('\\').next().unwrap_or(name);
    if let Some(full) = graph.imports.get(first) {
        let rest = &name[first.len()..];
        return format!("{full}{rest}");
    }
    qualified(name, graph)
}

/// `Foo` → `Namespace\Foo`, если namespace есть.
fn qualified(name: &str, graph: &FileGraph) -> String {
    match graph.namespace.as_deref().filter(|ns| !ns.is_empty()) {
        Some(namespace) => format!("{namespace}\\{name}"),
        None => name.to_owned(),
    }
}

/// Размечает готовый вывод `--body`: отслеживает файл по заголовкам `== path`.
pub fn annotate_body(text: &str, rows: &[QueryResult], root: &Path) -> String {
    let graphs: HashMap<&str, &FileGraph> = rows
        .iter()
        .map(|row| (row.graph.file.as_str(), &row.graph))
        .collect();
    let map = AutoloadMap::discover(root);
    let mut numbered = Numbered::new();
    let mut out = String::with_capacity(text.len() + 256);
    let mut file: Option<&str> = None;
    for line in text.lines() {
        let annotated = match (file, numbered_line(line)) {
            (Some(file), Some((number, content))) => match graphs.get(file) {
                Some(graph) => {
                    let mut pass = Pass {
                        map: &map,
                        file,
                        graph,
                        numbered: &mut numbered,
                    };
                    format_numbered(number, &pass.line(number, content))
                }
                None => line.to_owned(),
            },
            _ => {
                if let Some(path) = header_path(line) {
                    file = Some(path);
                }
                line.to_owned()
            }
        };
        out.push_str(&annotated);
        out.push('\n');
    }
    if numbered.entries.is_empty() {
        return out;
    }
    out.push_str(&footnotes(&numbered.entries, root));
    let entries = numbered.entries;
    if let Some(id) = store::save(root, entries.clone()) {
        out.push_str(&trailer(id, &entries));
    }
    out
}

/// Размечает произвольные строки одного файла (исходник класса для `refs`).
/// Возвращает строки с маркерами и собранные сноски.
pub fn annotate_lines(
    root: &Path,
    file: &str,
    graph: &FileGraph,
    lines: &[(u32, String)],
) -> (Vec<(u32, String)>, Vec<RefEntry>) {
    let map = AutoloadMap::discover(root);
    let mut numbered = Numbered::new();
    let mut out = Vec::with_capacity(lines.len());
    for (number, content) in lines {
        let mut pass = Pass {
            map: &map,
            file,
            graph,
            numbered: &mut numbered,
        };
        out.push((*number, pass.line(*number, content)));
    }
    (out, numbered.entries)
}

/// Путь из заголовка `== /abs/path  ...` (только у `--body`).
fn header_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("== ")?;
    let end = rest.find("  ").unwrap_or(rest.len());
    Some(&rest[..end])
}

/// `1143      public function …` → `(1143, "        public function …")`.
fn numbered_line(line: &str) -> Option<(u32, &str)> {
    let trimmed = line.trim_start();
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || trimmed[digits..].len() < 2 {
        return None;
    }
    let number = trimmed[..digits].parse().ok()?;
    let rest = &trimmed[digits..];
    (rest.len() >= 2 && rest.starts_with("  ")).then_some((number, &rest[2..]))
}

fn format_numbered(number: u32, content: &str) -> String {
    format!("{number:>6}  {content}")
}

/// Блок определений в конце ответа.
pub fn footnotes(entries: &[RefEntry], root: &Path) -> String {
    let mut out = String::new();
    for entry in entries {
        let fqcn = format!("\\{}", entry.fqcn);
        match &entry.path {
            Some(path) => {
                let relative = path.strip_prefix(root).unwrap_or(path);
                writeln!(
                    out,
                    "  #{:<3} {fqcn}  {}  [{} · {}]",
                    entry.number,
                    relative.display(),
                    entry.kind,
                    entry.source
                )
                .unwrap();
            }
            None if entry.kind == "internal" => {
                writeln!(out, "  #{:<3} {fqcn}  [internal · php]", entry.number).unwrap();
            }
            None => {
                writeln!(
                    out,
                    "  #{:<3} {fqcn}  {}",
                    entry.number,
                    crate::t!("not found", "nem található")
                )
                .unwrap();
            }
        }
    }
    out
}

/// Строка-приглашение: как раскрыть сноски.
pub fn trailer(id: u64, entries: &[RefEntry]) -> String {
    let numbers: Vec<String> = entries
        .iter()
        .map(|entry| entry.number.to_string())
        .collect();
    crate::tf!(
        "↪ refs r{id} {}  (show the referenced definition)\n",
        "↪ refs r{id} {}  (a hivatkozott definíció megjelenítése)\n",
        numbers.join(",")
    )
}

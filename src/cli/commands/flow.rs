//! `callgraph flow map [--project <alias|путь>] [--json] [--table <имя>] [--full]`: сопоставление
//! таблица ↔ property, построенное из PHP-классов и flow-плагинов (`crate::flow`).
//!
//! Статический анализ не везде может вычитать имя таблицы / колонки (массив `$fieldsMap` в legacy ORM,
//! магические методы). Это дополняют обработчики и ручные правила `.callgraph-flow.json`; результат — вход для
//! будущего графа потока данных.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use crate::cli::Command;
use crate::error::Result;
use crate::flow::{Mapping, scan_project};
use crate::skill::SkillHome;

/// Верхняя граница вывода (в больших проектах список обрезается).
const LIMIT: usize = 400;

pub struct FlowCommand {
    project: Option<String>,
    json: bool,
    table: Option<String>,
    full: bool,
    home: SkillHome,
}

impl FlowCommand {
    pub fn new(
        project: Option<String>,
        json: bool,
        table: Option<String>,
        full: bool,
        home: SkillHome,
    ) -> Self {
        Self {
            project,
            json,
            table,
            full,
            home,
        }
    }
}

impl Command for FlowCommand {
    fn run(&self, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
        let root = super::project_root(&self.home, self.project.as_deref())?;
        let mapping = scan_project(&root)?;
        for warning in &mapping.warnings {
            writeln!(err, "⚠ {warning}")?;
        }
        if self.json {
            write_json(out, &mapping, self.table.as_deref())?;
            return Ok(0);
        }
        write_text(out, &root, &mapping, self.table.as_deref(), self.full)?;
        Ok(0)
    }
}

fn write_text(
    out: &mut dyn Write,
    root: &std::path::Path,
    mapping: &Mapping,
    table_filter: Option<&str>,
    full: bool,
) -> std::io::Result<()> {
    writeln!(out, "root: {}", root.display())?;
    let tables: BTreeSet<&str> = mapping.columns.values().map(|c| c.table.as_str()).collect();
    let classes: BTreeSet<&str> = mapping.columns.keys().map(|(c, _)| c.as_str()).collect();
    let mut handlers: BTreeMap<&str, usize> = BTreeMap::new();
    for column in mapping.columns.values() {
        *handlers.entry(column.handler.as_str()).or_default() += 1;
    }
    let handlers: Vec<String> = handlers.iter().map(|(h, n)| format!("{h} {n}")).collect();
    writeln!(
        out,
        "flow map: {} tables · {} columns · {} classes · {}",
        tables.len(),
        mapping.columns.len(),
        classes.len(),
        if handlers.is_empty() {
            "no handler matched".to_string()
        } else {
            handlers.join(", ")
        }
    )?;

    // таблица → [(колонка, класс, property, handler)]
    let mut by_table: BTreeMap<&str, Vec<(&str, &str, &str, &str)>> = BTreeMap::new();
    for ((class, property), column) in &mapping.columns {
        if table_filter.is_some_and(|f| !column.table.eq_ignore_ascii_case(f)) {
            continue;
        }
        by_table.entry(column.table.as_str()).or_default().push((
            column.column.as_str(),
            class.as_str(),
            property.as_str(),
            column.handler.as_str(),
        ));
    }
    let mut lines = 0;
    for (table, mut columns) in by_table {
        columns.sort();
        writeln!(out, "{table}")?;
        lines += 1;
        for (column, class, property, handler) in columns {
            if !full && lines >= LIMIT {
                writeln!(out, "… more rows (--full)")?;
                return Ok(());
            }
            writeln!(out, "  {column}  ←  {class}::${property}  ({handler})")?;
            lines += 1;
        }
    }
    Ok(())
}

fn write_json(
    out: &mut dyn Write,
    mapping: &Mapping,
    table_filter: Option<&str>,
) -> std::io::Result<()> {
    let columns: Vec<serde_json::Value> = mapping
        .columns
        .iter()
        .filter(|(_, c)| table_filter.is_none_or(|f| c.table.eq_ignore_ascii_case(f)))
        .map(|((class, property), c)| {
            serde_json::json!({
                "class": class,
                "property": property,
                "table": c.table,
                "column": c.column,
                "handler": c.handler,
            })
        })
        .collect();
    let value = serde_json::json!({ "columns": columns });
    writeln!(out, "{}", serde_json::to_string_pretty(&value)?)
}

//! Разбор SQL-литералов (MySQL 8.4): таблица, выходные колонки, алиасы.
//!
//! Контракт для `local` (источники строк) и `index` (`row:`/`sql:` атомы):
//! `parse_sql` возвращает `None` для неразобранного/динамического SQL.

use std::collections::BTreeMap;

use sqlparser::ast::{Expr, ObjectName, Select, SelectItem, SetExpr, Statement, TableFactor};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

/// Результат разбора одного SELECT.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqlShape {
    /// `schema.table` (без backtick), если таблица определена.
    pub table: Option<String>,
    /// Выходные имена колонок (алиас, если задан).
    pub columns: Vec<String>,
    /// Выходное имя → исходная колонка (`st` → `service_type`; для identity совпадает).
    pub aliases: BTreeMap<String, String>,
}

/// Разобрать SQL; `None` — не SELECT / синтаксис не поддержан / динамический SQL.
pub fn parse_sql(sql: &str) -> Option<SqlShape> {
    let cleaned = preprocess(sql);
    let dialect = MySqlDialect {};
    let statements = Parser::parse_sql(&dialect, &cleaned).ok()?;
    let Statement::Query(query) = statements.first()? else {
        return None;
    };
    let select = first_select(&query.body)?;
    Some(shape(select))
}

/// Левое (первое) `SELECT` из тела запроса: сам SELECT, `WITH`-обёртка или UNION.
fn first_select(body: &SetExpr) -> Option<&Select> {
    match body {
        SetExpr::Select(select) => Some(select),
        SetExpr::Query(query) => first_select(&query.body),
        SetExpr::SetOperation { left, .. } => first_select(left),
        _ => None,
    }
}

fn shape(select: &Select) -> SqlShape {
    let mut out = SqlShape {
        table: select
            .from
            .first()
            .and_then(|from| table_name(&from.relation)),
        ..SqlShape::default()
    };
    for item in &select.projection {
        match item {
            SelectItem::UnnamedExpr(expr) => {
                if let Some(name) = column_name(expr) {
                    out.aliases.insert(name.clone(), name.clone());
                    out.columns.push(name);
                }
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                let name = alias.value.clone();
                if let Some(source) = column_name(expr) {
                    out.aliases.insert(name.clone(), source);
                }
                out.columns.push(name);
            }
            SelectItem::ExprWithAliases { aliases, .. } => {
                out.columns
                    .extend(aliases.iter().map(|alias| alias.value.clone()));
            }
            // `*` и `t.*`: набор колонок неизвестен.
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {}
        }
    }
    out
}

/// Имя колонки из выражения: `id` или `t.id` (последняя часть).
fn column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.clone()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|ident| ident.value.clone()),
        _ => None,
    }
}

/// `schema.table` из простого `TableFactor::Table`; производные таблицы дают `None`.
fn table_name(relation: &TableFactor) -> Option<String> {
    let TableFactor::Table { name, .. } = relation else {
        return None;
    };
    object_name(name)
}

fn object_name(name: &ObjectName) -> Option<String> {
    let parts = name
        .0
        .iter()
        .map(|part| part.as_ident().map(|ident| ident.value.as_str()))
        .collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then(|| parts.join("."))
}

/// Заменить PHP-интерполяцию и printf-плейсхолдеры на `NULL`, чтобы разобрать статическую часть.
pub fn preprocess(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '$' {
            // `${...}`
            if chars.get(i + 1) == Some(&'{')
                && let Some(end) = matching(&chars, i + 1)
            {
                out.push_str("NULL");
                i = end + 1;
                continue;
            }
            // `$name`
            let start = i + 1;
            if chars
                .get(start)
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
            {
                let mut j = start;
                while chars
                    .get(j)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    j += 1;
                }
                // `$this->name` (в том числе цепочка `$this->a->b`)
                if chars.get(start..j) == Some(&['t', 'h', 'i', 's'][..])
                    && chars.get(j) == Some(&'-')
                    && chars.get(j + 1) == Some(&'>')
                {
                    let mut k = j;
                    while chars.get(k) == Some(&'-') && chars.get(k + 1) == Some(&'>') {
                        k += 2;
                        while chars
                            .get(k)
                            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                        {
                            k += 1;
                        }
                    }
                    out.push_str("NULL");
                    i = k;
                    continue;
                }
                // `$name[...]`, в том числе вложенные индексы
                let mut k = j;
                while chars.get(k) == Some(&'[') {
                    match matching(&chars, k) {
                        Some(end) => k = end + 1,
                        None => break,
                    }
                }
                out.push_str("NULL");
                i = k;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        // `{$...}`
        if c == '{'
            && chars.get(i + 1) == Some(&'$')
            && let Some(end) = matching(&chars, i)
        {
            out.push_str("NULL");
            i = end + 1;
            continue;
        }
        // printf `%s`, `%d`
        if c == '%' && matches!(chars.get(i + 1), Some('s') | Some('d')) {
            out.push_str("NULL");
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }

    // `IN :name` → `IN (NULL)`; после подстановок.
    let chars: Vec<char> = out.chars().collect();
    let mut result = String::with_capacity(out.len());
    let mut i = 0;
    while i < chars.len() {
        if word_at(&chars, i, "in") && !ident_before(&chars, i) {
            let mut j = i + 2;
            while chars.get(j).is_some_and(|c| c.is_whitespace()) {
                j += 1;
            }
            if chars.get(j) == Some(&':') {
                let mut k = j + 1;
                while chars
                    .get(k)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    k += 1;
                }
                if k > j + 1 {
                    result.push_str("IN (NULL)");
                    i = k;
                    continue;
                }
            }
        }
        // `VALUES $x` уже стал `VALUES NULL` — восстановить скобки.
        if word_at(&chars, i, "values") && !ident_before(&chars, i) {
            let mut j = i + 6;
            while chars.get(j).is_some_and(|c| c.is_whitespace()) {
                j += 1;
            }
            if word_at(&chars, j, "null") {
                result.push_str("VALUES (NULL)");
                i = j + 4;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }

    result.trim().trim_end_matches(';').trim_end().to_string()
}

/// Индекс закрывающей скобки для открывающей в `open`; вложенность учитывается.
fn matching(chars: &[char], open: usize) -> Option<usize> {
    let (left, right) = match chars.get(open)? {
        '{' => ('{', '}'),
        '[' => ('[', ']'),
        _ => return None,
    };
    let mut depth = 0usize;
    for (offset, c) in chars.iter().enumerate().skip(open) {
        if *c == left {
            depth += 1;
        } else if *c == right {
            depth -= 1;
            if depth == 0 {
                return Some(offset);
            }
        }
    }
    None
}

/// Слово `word` (ASCII, без учёта регистра) начинается в позиции `at`.
fn word_at(chars: &[char], at: usize, word: &str) -> bool {
    let word: Vec<char> = word.chars().collect();
    chars.get(at..at + word.len()).is_some_and(|slice| {
        slice
            .iter()
            .zip(&word)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Перед позицией `at` стоит символ идентификатора (значит, это не отдельное слово).
fn ident_before(chars: &[char], at: usize) -> bool {
    at > 0
        && chars
            .get(at - 1)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
}

/// Короткое имя метода БД, который возвращает строки/выполняет запрос.
pub fn is_db_call(name: &str) -> bool {
    const METHODS: &[&str] = &[
        "fetchAssociative",
        "fetchAllAssociative",
        "fetchOne",
        "fetchNumeric",
        "fetchFirstColumn",
        "fetchArray",
        "fetchAll",
        "fetch",
        "fetchObject",
        "executeQuery",
        "executeStatement",
        "query",
        "queryAll",
        "queryOne",
        "queryScalar",
        "select",
        "selectOne",
        "prepare",
    ];
    let short = name
        .rsplit("::")
        .next()
        .unwrap_or(name)
        .trim_start_matches('\\');
    METHODS.iter().any(|m| m.eq_ignore_ascii_case(short))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(sql: &str) -> SqlShape {
        parse_sql(sql).unwrap_or_else(|| panic!("не разобрано: {sql}"))
    }

    #[test]
    fn simple_select() {
        let got = shape("SELECT id, name FROM users");
        assert_eq!(got.table.as_deref(), Some("users"));
        assert_eq!(got.columns, vec!["id", "name"]);
        assert_eq!(got.aliases.get("id").map(String::as_str), Some("id"));
        assert_eq!(got.aliases.get("name").map(String::as_str), Some("name"));
    }

    #[test]
    fn alias_and_expression_alias() {
        let got = shape("SELECT service_type AS st, COUNT(*) AS total FROM orders");
        assert_eq!(got.table.as_deref(), Some("orders"));
        assert_eq!(got.columns, vec!["st", "total"]);
        assert_eq!(
            got.aliases.get("st").map(String::as_str),
            Some("service_type")
        );
        assert!(!got.aliases.contains_key("total"), "{:?}", got.aliases);
    }

    #[test]
    fn schema_table_with_backticks() {
        let got = shape("SELECT `id` FROM `shop`.`orders`");
        assert_eq!(got.table.as_deref(), Some("shop.orders"));
        assert_eq!(got.columns, vec!["id"]);
    }

    #[test]
    fn join_takes_first_table() {
        let got = shape("SELECT u.id FROM users u JOIN orders o ON o.user_id = u.id");
        assert_eq!(got.table.as_deref(), Some("users"));
        assert_eq!(got.columns, vec!["id"]);
    }

    #[test]
    fn cte_is_parsed() {
        let got = shape("WITH recent AS (SELECT id FROM orders) SELECT id, total FROM recent");
        assert_eq!(got.table.as_deref(), Some("recent"));
        assert_eq!(got.columns, vec!["id", "total"]);
    }

    #[test]
    fn wildcard_keeps_table_and_unknown_columns() {
        let got = shape("SELECT * FROM `users`");
        assert_eq!(got.table.as_deref(), Some("users"));
        assert!(got.columns.is_empty(), "{:?}", got.columns);

        let got = shape("SELECT u.* FROM users u");
        assert_eq!(got.table.as_deref(), Some("users"));
        assert!(got.columns.is_empty(), "{:?}", got.columns);
    }

    #[test]
    fn named_placeholder() {
        let got = shape("SELECT id, email FROM users WHERE id = :id");
        assert_eq!(got.table.as_deref(), Some("users"));
        assert_eq!(got.columns, vec!["id", "email"]);
    }

    #[test]
    fn dynamic_values_are_preprocessed_away() {
        let sql = "SELECT id FROM users WHERE id IN :ids AND name = '{$name}'";
        let cleaned = preprocess(sql);
        assert!(cleaned.contains("IN (NULL)"), "{cleaned}");
        assert!(cleaned.contains("NULL"), "{cleaned}");
        let got = shape(sql);
        assert_eq!(got.table.as_deref(), Some("users"));
        assert_eq!(got.columns, vec!["id"]);

        let insert = preprocess("INSERT INTO t VALUES $values");
        assert_eq!(insert, "INSERT INTO t VALUES (NULL)");
    }

    #[test]
    fn php_interpolation_and_printf() {
        assert_eq!(
            preprocess("SELECT * FROM t WHERE a = $a"),
            "SELECT * FROM t WHERE a = NULL"
        );
        assert_eq!(
            preprocess("SELECT * FROM t WHERE a = $row['id']"),
            "SELECT * FROM t WHERE a = NULL"
        );
        assert_eq!(
            preprocess("SELECT * FROM t WHERE a = {$row['id']}"),
            "SELECT * FROM t WHERE a = NULL"
        );
        assert_eq!(
            preprocess("SELECT * FROM t WHERE a = $this->id"),
            "SELECT * FROM t WHERE a = NULL"
        );
        assert_eq!(
            preprocess("SELECT * FROM t WHERE a = '%s'"),
            "SELECT * FROM t WHERE a = 'NULL'"
        );
    }

    #[test]
    fn non_select_is_none() {
        assert!(parse_sql("UPDATE users SET a = 1").is_none());
        assert!(parse_sql("SELECT FROM WHERE").is_none());
    }

    #[test]
    fn db_call_names() {
        assert!(is_db_call("fetchAllAssociative"));
        assert!(is_db_call("\\Doctrine\\DBAL\\Connection::executeQuery"));
        assert!(is_db_call("FETCHONE"));
        assert!(is_db_call("query"));
        assert!(!is_db_call("fetchAllAssoc"));
        assert!(!is_db_call("save"));
    }
}

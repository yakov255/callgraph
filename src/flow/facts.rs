//! Факты о PHP-классах для flow-слоя: имя, пространство имён, `extends`, типы property и литеральные значения по умолчанию.
//!
//! `FileGraph` намеренно не хранит значения property по умолчанию (схема JSONL не меняется), поэтому flow-анализ
//! один раз перечитывает исходник. Плагины строят из этих фактов сопоставление таблица ↔ property.

use mago_span::HasSpan;
use mago_syntax::cst::*;

use super::Value;
use crate::lang::php::analysis::NameResolver;
use crate::lang::php::parser::{MagoAdapter, ParsedFile, PhpParser};

/// Класс для flow-анализа.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ClassFacts {
    pub file: String,
    /// Полное имя (`Common\HotelChain`); без пространства имён — короткое имя.
    pub name: String,
    pub short: String,
    pub extends: Vec<String>,
    pub properties: Vec<PropFacts>,
}

/// Property: имя (без `$`), тип, литеральное значение по умолчанию.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PropFacts {
    pub name: String,
    pub ty: Option<String>,
    pub default: Option<Value>,
    /// Property, продвинутое в конструкторе (`private int $x` в `__construct`).
    pub promoted: bool,
}

/// Классы файла (без интерфейсов / trait-ов: для flow нужны property).
pub fn file_facts(path: &str, source: &[u8]) -> Vec<ClassFacts> {
    MagoAdapter.parse(path, source, |parsed| {
        let mut out = Vec::new();
        let statements = top_statements(parsed.program);
        let (namespace, resolver) = namespace_and_resolver(parsed, &statements);
        for statement in &statements {
            if let Statement::Class(class) = statement {
                out.push(class_facts(
                    parsed,
                    class,
                    path,
                    namespace.as_deref(),
                    &resolver,
                ));
            }
        }
        out
    })
}

/// Верхнеуровневые инструкции — и без блока `namespace`, и вложенные в пространство имён.
fn top_statements<'a, 'arena>(program: &'a Program<'arena>) -> Vec<&'a Statement<'arena>> {
    let mut out = Vec::new();
    for statement in &program.statements {
        match statement {
            Statement::Namespace(ns) => out.extend(ns.statements().iter()),
            other => out.push(other),
        }
    }
    out
}

/// Первое пространство имён и резолвер имён, построенный из `use`-импортов.
fn namespace_and_resolver(
    parsed: &ParsedFile<'_>,
    statements: &[&Statement<'_>],
) -> (Option<String>, NameResolver) {
    let mut namespace = None;
    for statement in parsed.program.statements.iter() {
        if let Statement::Namespace(ns) = statement {
            namespace = ns
                .name
                .as_ref()
                .map(|n| String::from_utf8_lossy(n.value()).into_owned());
            break;
        }
    }
    let mut resolver = NameResolver::new(namespace.clone());
    for statement in statements {
        if let Statement::Use(u) = statement
            && let UseItems::Sequence(items) = &u.items
        {
            for item in items.items.iter() {
                let full = String::from_utf8_lossy(item.name.value()).into_owned();
                let alias = item.alias.as_ref().map_or_else(
                    || full.rsplit('\\').next().unwrap_or(&full).to_owned(),
                    |a| String::from_utf8_lossy(a.identifier.value).into_owned(),
                );
                resolver.add_import(&alias, &full);
            }
        }
    }
    (namespace, resolver)
}

fn class_facts(
    parsed: &ParsedFile<'_>,
    class: &Class<'_>,
    file: &str,
    namespace: Option<&str>,
    resolver: &NameResolver,
) -> ClassFacts {
    let short = String::from_utf8_lossy(class.name.value).into_owned();
    let name = match namespace {
        Some(ns) if !ns.is_empty() => format!("{ns}\\{short}"),
        _ => short.clone(),
    };
    let extends = class
        .extends
        .as_ref()
        .map(|e| {
            e.types
                .iter()
                .map(|t| resolver.resolve_class(&parsed.text(t.span())))
                .collect()
        })
        .unwrap_or_default();
    let mut properties = Vec::new();
    for member in class.members.iter() {
        match member {
            ClassLikeMember::Property(Property::Plain(p)) => {
                let ty = p
                    .hint
                    .as_ref()
                    .map(|h| parsed.text(h.span()).trim().to_owned());
                for item in p.items.iter() {
                    let (variable, default) = match item {
                        PropertyItem::Abstract(a) => (&a.variable, None),
                        PropertyItem::Concrete(c) => {
                            (&c.variable, super::literal::value_of(parsed, c.value))
                        }
                    };
                    properties.push(PropFacts {
                        name: String::from_utf8_lossy(variable.name)
                            .trim_start_matches('$')
                            .to_owned(),
                        ty: ty.clone(),
                        default,
                        promoted: false,
                    });
                }
            }
            ClassLikeMember::Method(m)
                if String::from_utf8_lossy(m.name.value).eq_ignore_ascii_case("__construct") =>
            {
                for p in m.parameter_list.parameters.iter() {
                    if !p.is_promoted_property() {
                        continue;
                    }
                    properties.push(PropFacts {
                        name: String::from_utf8_lossy(p.variable.name)
                            .trim_start_matches('$')
                            .to_owned(),
                        ty: p
                            .hint
                            .as_ref()
                            .map(|h| parsed.text(h.span()).trim().to_owned()),
                        default: p
                            .default_value
                            .as_ref()
                            .and_then(|d| super::literal::value_of(parsed, d.value)),
                        promoted: true,
                    });
                }
            }
            _ => {}
        }
    }
    ClassFacts {
        file: file.to_owned(),
        name,
        short,
        extends,
        properties,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_property_defaults() {
        let src = br#"<?php
namespace Common;
class HotelChain {
    public $nameEn;
    public $tableName = 'cmn_hotel_chain';
    public $fieldsMap = ['nameEn' => 'name_en'];
}
"#;
        let facts = file_facts("a.php", src);
        assert_eq!(facts.len(), 1, "{facts:?}");
        let c = &facts[0];
        assert_eq!(c.name, "Common\\HotelChain");
        let table = c.properties.iter().find(|p| p.name == "tableName").unwrap();
        assert_eq!(
            table.default.as_ref().and_then(|v| v.as_str()),
            Some("cmn_hotel_chain"),
            "{:?}",
            table.default
        );
        let map = c.properties.iter().find(|p| p.name == "fieldsMap").unwrap();
        assert!(
            map.default.as_ref().and_then(|v| v.as_array()).is_some(),
            "{:?}",
            map.default
        );
    }
}

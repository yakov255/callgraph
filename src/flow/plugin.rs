//! Flow-плагины: сопоставление таблица ↔ property из фактов о классах.
//!
//! Статический анализ не везде может вычитать имя таблицы / колонки (массив `$fieldsMap` в legacy ORM,
//! магические методы, сгенерированный код). Для этого и нужны обработчики:
//!
//! - встроенный `legacy-fields-map`: литеральные property `$tableName` + `$fieldsMap` (например, наследники `Billet`);
//! - правила `hydrate` в `.callgraph-flow.json`: декларативное сопоставление для объектов, заполняемых вне AST
//!   (JsonMapper, Symfony Serializer, Doctrine): класс, таблица и `property → column`;
//! - правила `map` в `.callgraph-flow.json`: ручное переопределение, всегда высший приоритет;
//! - собственный скомпилированный обработчик: реализация трейта `FlowHandler`, зарегистрированная в `Registry`.
//!
//! Результат (`Mapping`) — вход для будущего графа потока данных: `(класс, property) → таблица.колонка`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::Deserialize;

use super::Value;
use super::facts::{ClassFacts, PropFacts};

/// Ссылка на колонку.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    pub table: String,
    pub column: String,
    /// Какой обработчик дал (`legacy-fields-map`, `hydrate`, `manual`, …).
    pub handler: String,
}

/// Сопоставление уровня проекта.
#[derive(Debug, Clone, Default)]
pub struct Mapping {
    /// Полное имя класса → таблица.
    pub tables: BTreeMap<String, String>,
    /// (полное имя класса, property) → колонка.
    pub columns: BTreeMap<(String, String), ColumnRef>,
    /// Ошибки запуска плагинов (команда не стартовала, битый JSON, …).
    pub warnings: Vec<String>,
}

impl Mapping {
    /// Добавить, если такого property ещё нет (побеждает более ранний, точный обработчик).
    pub fn insert(
        &mut self,
        class: &str,
        property: &str,
        table: &str,
        column: &str,
        handler: &str,
    ) {
        self.tables
            .entry(class.to_owned())
            .or_insert_with(|| table.to_owned());
        self.columns
            .entry((class.to_owned(), property.to_owned()))
            .or_insert_with(|| ColumnRef {
                table: table.to_owned(),
                column: column.to_owned(),
                handler: handler.to_owned(),
            });
    }

    /// Перезапись (ручное правило `map`).
    pub fn insert_force(
        &mut self,
        class: &str,
        property: &str,
        table: &str,
        column: &str,
        handler: &str,
    ) {
        self.tables.insert(class.to_owned(), table.to_owned());
        self.columns.insert(
            (class.to_owned(), property.to_owned()),
            ColumnRef {
                table: table.to_owned(),
                column: column.to_owned(),
                handler: handler.to_owned(),
            },
        );
    }
}

/// Компилируемый обработчик: дополняет сопоставление по каждому классу.
pub trait FlowHandler: Send + Sync {
    fn id(&self) -> &'static str;
    fn apply(&self, class: &ClassFacts, out: &mut Mapping);
}

/// `$tableName` + `$fieldsMap` (legacy ORM, например `Billet`).
pub struct FieldsMapHandler {
    pub table_property: String,
    pub map_property: String,
    /// Фильтр по `extends` (короткое или полное имя); `None`: любой класс.
    pub extends: Option<String>,
}

impl Default for FieldsMapHandler {
    fn default() -> Self {
        Self {
            table_property: "tableName".into(),
            map_property: "fieldsMap".into(),
            extends: None,
        }
    }
}

impl FlowHandler for FieldsMapHandler {
    fn id(&self) -> &'static str {
        "legacy-fields-map"
    }

    fn apply(&self, class: &ClassFacts, out: &mut Mapping) {
        if let Some(filter) = &self.extends
            && !class
                .extends
                .iter()
                .any(|e| short_name(e).eq_ignore_ascii_case(short_name(filter)))
        {
            return;
        }
        let Some(table) = property_str(class, &self.table_property) else {
            return;
        };
        let Some(map) = property_array(class, &self.map_property) else {
            return;
        };
        let declared: HashSet<&str> = class.properties.iter().map(|p| p.name.as_str()).collect();
        for (key, value) in map {
            let (Some(key), Some(value)) = (key.as_ref().and_then(Value::as_str), value.as_str())
            else {
                continue;
            };
            // `property => column` или `column => property` — решает объявленное property
            let (property, column) = if declared.contains(key) {
                (key, value)
            } else if declared.contains(value) {
                (value, key)
            } else {
                (key, value)
            };
            out.insert(&class.name, property, table, column, self.id());
        }
    }
}

/// Ручное правило из `.callgraph-flow.json`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MapRule {
    pub class: String,
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default)]
    pub property: Option<String>,
    #[serde(default)]
    pub column: Option<String>,
}

/// Декларативное правило для объектов, заполняемых вне AST (JsonMapper, Symfony Serializer, Doctrine).
///
/// Задаёт таблицу класса и явное `property → column`; правило не зависит от наличия класса в AST.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HydrateRule {
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub table: String,
    /// property → колонка.
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

/// Конфигурация (`<root>/.callgraph-flow.json`, JSONC: можно и с комментариями).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub handlers: Vec<HandlerConfig>,
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
    #[serde(default)]
    pub hydrate: Vec<HydrateRule>,
    #[serde(default)]
    pub map: Vec<MapRule>,
}

/// Внешний плагин: команда получает JSON фактов о классах на stdin, пишет JSON сопоставление на stdout.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginConfig {
    pub id: String,
    /// Shell-команда (`sh -c`); это конфигурация проекта, поэтому только из доверенного источника.
    pub command: String,
}

/// Настроенный обработчик.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HandlerConfig {
    pub id: String,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub table_property: Option<String>,
    #[serde(default)]
    pub map_property: Option<String>,
}

/// Обработчики и ручные правила.
pub struct Registry {
    handlers: Vec<Box<dyn FlowHandler>>,
    plugins: Vec<PluginConfig>,
    hydrate: Vec<HydrateRule>,
    manual: Vec<MapRule>,
}

impl Registry {
    /// Конфигурация в корне проекта; без файла работает встроенный `legacy-fields-map`.
    pub fn for_project(root: &Path) -> Self {
        let config = load_config(root).unwrap_or_default();
        Self::from_config(config)
    }

    pub fn from_config(config: Config) -> Self {
        let mut handlers: Vec<Box<dyn FlowHandler>> = Vec::new();
        if config.handlers.is_empty() {
            handlers.push(Box::new(FieldsMapHandler::default()));
        } else {
            for h in &config.handlers {
                // неизвестный id: пропускаем (свой обработчик регистрируется в коде, `with_handler`)
                if h.id == "legacy-fields-map" {
                    handlers.push(Box::new(FieldsMapHandler {
                        table_property: h
                            .table_property
                            .clone()
                            .unwrap_or_else(|| "tableName".into()),
                        map_property: h.map_property.clone().unwrap_or_else(|| "fieldsMap".into()),
                        extends: h.extends.clone(),
                    }));
                }
            }
        }
        Self {
            handlers,
            plugins: config.plugins,
            hydrate: config.hydrate,
            manual: config.map,
        }
    }

    /// Добавить свой (скомпилированный) обработчик — перед ручными правилами.
    pub fn with_handler(mut self, handler: Box<dyn FlowHandler>) -> Self {
        self.handlers.push(handler);
        self
    }

    /// Построить сопоставление.
    ///
    /// Приоритет: встроенные обработчики, внешние плагины, затем `hydrate` (не перекрывает уже найденное)
    /// и в конце ручные правила `map` (перекрывают всё).
    pub fn apply(&self, classes: &[ClassFacts]) -> Mapping {
        let mut mapping = Mapping::default();
        for class in classes {
            for handler in &self.handlers {
                handler.apply(class, &mut mapping);
            }
        }
        for plugin in &self.plugins {
            match run_plugin(plugin, classes) {
                Ok(rows) => {
                    for row in rows {
                        mapping.insert_force(
                            &row.class,
                            &row.property,
                            &row.table,
                            &row.column,
                            &plugin.id,
                        );
                    }
                }
                Err(e) => mapping.warnings.push(format!("plugin {}: {e}", plugin.id)),
            }
        }
        for rule in &self.hydrate {
            let class = rule.class.trim();
            let table = rule.table.trim();
            // неполное правило пропускаем целиком
            if class.is_empty() || table.is_empty() {
                continue;
            }
            // `hydrate` дополняет уже найденное, но не перекрывает его
            mapping
                .tables
                .entry(class.to_owned())
                .or_insert_with(|| table.to_owned());
            for (property, column) in &rule.properties {
                let (property, column) = (property.trim(), column.trim());
                if property.is_empty() || column.is_empty() {
                    continue;
                }
                mapping.insert(class, property, table, column, "hydrate");
            }
        }
        for rule in &self.manual {
            let wanted = short_name(&rule.class).to_ascii_lowercase();
            for class in classes {
                if short_name(&class.name).to_ascii_lowercase() != wanted {
                    continue;
                }
                let Some(table) = &rule.table else { continue };
                let properties: Vec<String> = match &rule.property {
                    Some(p) => vec![p.clone()],
                    None => class.properties.iter().map(|p| p.name.clone()).collect(),
                };
                for property in properties {
                    let column = rule.column.clone().unwrap_or_else(|| property.clone());
                    mapping.insert_force(&class.name, &property, table, &column, "manual");
                }
            }
        }
        mapping
    }
}

/// Ответ внешнего плагина.
#[derive(Debug, Deserialize)]
struct PluginResponse {
    #[serde(default)]
    columns: Vec<PluginColumn>,
}

/// Одна строка из ответа плагина.
#[derive(Debug, Deserialize)]
struct PluginColumn {
    class: String,
    property: String,
    table: String,
    column: String,
}

/// Запуск плагина: JSON фактов о классах на stdin, JSON сопоставление на stdout.
fn run_plugin(plugin: &PluginConfig, classes: &[ClassFacts]) -> Result<Vec<PluginColumn>, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let request = serde_json::json!({ "schema": 1, "classes": classes });
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&plugin.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    {
        let stdin = child.stdin.as_mut().ok_or("no stdin")?;
        serde_json::to_writer(&mut *stdin, &request).map_err(|e| e.to_string())?;
        let _ = stdin.flush();
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("exit {}", output.status));
    }
    let response: PluginResponse =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("bad JSON: {e}"))?;
    Ok(response.columns)
}

fn load_config(root: &Path) -> Option<Config> {
    let path = root.join(".callgraph-flow.json");
    let text = std::fs::read_to_string(path).ok()?;
    let options = jsonc_parser::ParseOptions::default();
    jsonc_parser::parse_to_serde_value(&text, &options)
        .ok()
        .flatten()
}

fn short_name(name: &str) -> &str {
    name.rsplit('\\').next().unwrap_or(name)
}

fn property<'a>(class: &'a ClassFacts, name: &str) -> Option<&'a PropFacts> {
    class
        .properties
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
}

fn property_str<'a>(class: &'a ClassFacts, name: &str) -> Option<&'a str> {
    property(class, name)?.default.as_ref()?.as_str()
}

fn property_array<'a>(class: &'a ClassFacts, name: &str) -> Option<&'a [(Option<Value>, Value)]> {
    property(class, name)?.default.as_ref()?.as_array()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(name: &str, props: Vec<PropFacts>) -> ClassFacts {
        ClassFacts {
            file: "x.php".into(),
            name: name.into(),
            short: short_name(name).into(),
            extends: vec![],
            implements: vec![],
            traits: vec![],
            properties: props,
        }
    }

    fn prop(name: &str, default: Value) -> PropFacts {
        PropFacts {
            name: name.into(),
            ty: None,
            default: Some(default),
            promoted: false,
        }
    }

    #[test]
    fn fields_map_is_read_in_property_to_column_order() {
        let c = class(
            "Common\\HotelChain",
            vec![
                prop("tableName", Value::Str("cmn_hotel_chain".into())),
                prop(
                    "fieldsMap",
                    Value::Array(vec![
                        (
                            Some(Value::Str("nameEn".into())),
                            Value::Str("name_en".into()),
                        ),
                        (
                            Some(Value::Str("name_ru".into())),
                            Value::Str("nameRu".into()),
                        ),
                    ]),
                ),
                PropFacts {
                    name: "nameEn".into(),
                    ty: None,
                    default: None,
                    promoted: false,
                },
                PropFacts {
                    name: "nameRu".into(),
                    ty: None,
                    default: None,
                    promoted: false,
                },
            ],
        );
        let mapping = Registry::from_config(Config::default()).apply(&[c]);
        let get = |p: &str| {
            mapping
                .columns
                .get(&("Common\\HotelChain".to_string(), p.to_string()))
                .map(|c| format!("{}.{}", c.table, c.column))
        };
        assert_eq!(get("nameEn").as_deref(), Some("cmn_hotel_chain.name_en"));
        // обратный порядок (`column => property`) тоже распознаётся
        assert_eq!(get("nameRu").as_deref(), Some("cmn_hotel_chain.name_ru"));
    }

    #[test]
    fn manual_map_overrides_the_handler() {
        let c = class(
            "Common\\HotelChain",
            vec![
                prop("tableName", Value::Str("wrong".into())),
                prop(
                    "fieldsMap",
                    Value::Array(vec![(
                        Some(Value::Str("nameEn".into())),
                        Value::Str("name_en".into()),
                    )]),
                ),
            ],
        );
        let registry = Registry::from_config(Config {
            handlers: vec![],
            plugins: vec![],
            hydrate: vec![],
            map: vec![MapRule {
                class: "HotelChain".into(),
                table: Some("cmn_hotel_chain".into()),
                property: Some("nameEn".into()),
                column: Some("name_en".into()),
            }],
        });
        let mapping = registry.apply(&[c]);
        let col = mapping
            .columns
            .get(&("Common\\HotelChain".to_string(), "nameEn".to_string()))
            .unwrap();
        assert_eq!(col.table, "cmn_hotel_chain");
        assert_eq!(col.handler, "manual");
    }

    #[test]
    fn hydrate_rule_adds_table_and_columns() {
        // класс не обязан быть в AST: объект заполняется JsonMapper / Serializer
        let registry = Registry::from_config(Config {
            hydrate: vec![HydrateRule {
                class: "App\\Dto\\Hotel".into(),
                table: "hotel".into(),
                properties: BTreeMap::from([
                    ("name".into(), "name".into()),
                    ("nameEn".into(), "name_en".into()),
                ]),
            }],
            ..Config::default()
        });
        let mapping = registry.apply(&[]);
        assert_eq!(
            mapping.tables.get("App\\Dto\\Hotel").map(String::as_str),
            Some("hotel")
        );
        let col = mapping
            .columns
            .get(&("App\\Dto\\Hotel".to_string(), "nameEn".to_string()))
            .expect("hydrate column");
        assert_eq!(
            (
                col.table.as_str(),
                col.column.as_str(),
                col.handler.as_str()
            ),
            ("hotel", "name_en", "hydrate")
        );
    }

    #[test]
    fn hydrate_does_not_override_the_handler() {
        let c = class(
            "Common\\HotelChain",
            vec![
                prop("tableName", Value::Str("cmn_hotel_chain".into())),
                prop(
                    "fieldsMap",
                    Value::Array(vec![(
                        Some(Value::Str("nameEn".into())),
                        Value::Str("name_en".into()),
                    )]),
                ),
            ],
        );
        let registry = Registry::from_config(Config {
            hydrate: vec![HydrateRule {
                class: "Common\\HotelChain".into(),
                table: "wrong".into(),
                properties: BTreeMap::from([("nameEn".into(), "wrong".into())]),
            }],
            ..Config::default()
        });
        let mapping = registry.apply(&[c]);
        let col = mapping
            .columns
            .get(&("Common\\HotelChain".to_string(), "nameEn".to_string()))
            .expect("handler column");
        assert_eq!(col.table, "cmn_hotel_chain");
        assert_eq!(col.column, "name_en");
        assert_eq!(col.handler, "legacy-fields-map");
        assert_eq!(
            mapping.tables.get("Common\\HotelChain").map(String::as_str),
            Some("cmn_hotel_chain")
        );
    }

    #[test]
    fn manual_map_overrides_hydrate() {
        let registry = Registry::from_config(Config {
            hydrate: vec![HydrateRule {
                class: "Common\\HotelChain".into(),
                table: "wrong".into(),
                properties: BTreeMap::from([("nameEn".into(), "wrong".into())]),
            }],
            map: vec![MapRule {
                class: "HotelChain".into(),
                table: Some("cmn_hotel_chain".into()),
                property: Some("nameEn".into()),
                column: Some("name_en".into()),
            }],
            ..Config::default()
        });
        let mapping = registry.apply(&[class("Common\\HotelChain", vec![])]);
        let col = mapping
            .columns
            .get(&("Common\\HotelChain".to_string(), "nameEn".to_string()))
            .expect("manual column");
        assert_eq!(
            (col.table.as_str(), col.column.as_str()),
            ("cmn_hotel_chain", "name_en")
        );
        assert_eq!(col.handler, "manual");
    }

    #[test]
    fn hydrate_rule_with_empty_class_or_table_is_skipped() {
        let registry = Registry::from_config(Config {
            hydrate: vec![
                HydrateRule {
                    class: "App\\EmptyTable".into(),
                    table: "  ".into(),
                    properties: BTreeMap::from([("p".into(), "c".into())]),
                },
                HydrateRule {
                    class: "".into(),
                    table: "t".into(),
                    properties: BTreeMap::from([("p".into(), "c".into())]),
                },
            ],
            ..Config::default()
        });
        let mapping = registry.apply(&[]);
        assert!(mapping.tables.is_empty());
        assert!(mapping.columns.is_empty());
    }

    #[test]
    fn external_plugin_can_add_columns() {
        let c = class("Legacy\\Thing", vec![]);
        let command = r#"cat >/dev/null; printf '%s' '{"columns":[{"class":"Legacy\\Thing","property":"p","table":"t","column":"c"}]}'"#;
        let registry = Registry::from_config(Config {
            plugins: vec![PluginConfig {
                id: "legacy".into(),
                command: command.into(),
            }],
            ..Config::default()
        });
        let mapping = registry.apply(&[c]);
        let col = mapping
            .columns
            .get(&("Legacy\\Thing".to_string(), "p".to_string()))
            .expect("plugin column");
        assert_eq!((col.table.as_str(), col.column.as_str()), ("t", "c"));
        assert_eq!(col.handler, "legacy");
    }

    #[test]
    fn extends_filter_skips_other_classes() {
        let mut c = class(
            "Other\\Thing",
            vec![prop("tableName", Value::Str("t".into()))],
        );
        c.extends = vec!["Base".into()];
        let handler = FieldsMapHandler {
            extends: Some("Billet".into()),
            ..FieldsMapHandler::default()
        };
        let mut mapping = Mapping::default();
        handler.apply(&c, &mut mapping);
        assert!(mapping.tables.is_empty());
    }
}

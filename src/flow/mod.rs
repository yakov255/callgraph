//! Flow-слой: сопоставление таблица ↔ property из фактов о PHP-классах, с плагинами.
//!
//! Статический анализ не везде может вычитать имя таблицы / колонки; это дополняют обработчики модуля `plugin`
//! (legacy `$fieldsMap`, ручные правила, собственный скомпилированный обработчик). Результат — вход для будущего
//! графа потока данных.

pub mod facts;
pub mod index;
pub mod literal;
pub mod local;
pub mod model;
pub mod plugin;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rayon::prelude::*;

pub use index::Index;
pub use literal::Value;
pub use model::{ArgEdge, Assign, FlowFile, MethodFlow, Read, Trace};
pub use plugin::{ClassTableHandler, ColumnRef, Config, Mapping, PropertyColumnHandler, Registry};

use crate::error::Result;
use crate::io::FileWalker;
use facts::ClassFacts;
use model::MethodTypes;

/// PHP-классы проекта и построенное по ним сопоставление.
pub fn scan_project(root: &Path) -> Result<Mapping> {
    let walker = FileWalker::for_extensions(vec!["php"]);
    let files = walker.collect(&[root.to_path_buf()], &mut |_| {});
    let classes: Vec<ClassFacts> = files
        .par_iter()
        .flat_map(|path| {
            let Ok(source) = std::fs::read(path) else {
                return Vec::new();
            };
            facts::file_facts(&path.to_string_lossy(), &source)
        })
        .collect();
    let classes = merge_inheritance(classes);
    Ok(Registry::for_project(root).apply(&classes))
}

/// Факты потока данных по всем PHP-файлам проекта (без кэша: перечитывает источник).
///
/// Два прохода: сначала типы возврата методов (нужны для `$x = $repo->find()` и `foreach` по `Foo[]`),
/// затем сам анализ с этой таблицей.
pub fn scan_flow(root: &Path) -> Result<Vec<FlowFile>> {
    let walker = FileWalker::for_extensions(vec!["php"]);
    let files = walker.collect(&[root.to_path_buf()], &mut |_| {});
    let types: MethodTypes = files
        .par_iter()
        .flat_map(|path| {
            let Ok(source) = std::fs::read(path) else {
                return Vec::new();
            };
            local::file_return_types(&path.to_string_lossy(), &source)
        })
        .collect();
    Ok(files
        .par_iter()
        .flat_map(|path| {
            let Ok(source) = std::fs::read(path) else {
                return Vec::new();
            };
            local::file_facts_with_types(&path.to_string_lossy(), &source, &types)
        })
        .collect())
}

/// Карта «класс → прямые родители» из фактов о классах (для наследования методов в `Index`).
///
/// Прямые родители — объединение `extends`, `implements` и `traits` без повторов, полные имена.
/// Транзитивные предки не разворачиваются: `Index` сам обходит цепочку в ширину.
pub fn scan_parents(root: &Path) -> Result<HashMap<String, Vec<String>>> {
    let walker = FileWalker::for_extensions(vec!["php"]);
    let files = walker.collect(&[root.to_path_buf()], &mut |_| {});
    let classes: Vec<facts::ClassFacts> = files
        .par_iter()
        .flat_map(|path| {
            let Ok(source) = std::fs::read(path) else {
                return Vec::new();
            };
            facts::file_facts(&path.to_string_lossy(), &source)
        })
        .collect();
    Ok(classes
        .into_iter()
        .map(|class| {
            let parents = direct_parents(&class);
            (class.name, parents)
        })
        .collect())
}

/// Прямые родители class-like объявления: `extends`, затем `implements`, затем `traits`; повторы убраны.
fn direct_parents(class: &ClassFacts) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in class
        .extends
        .iter()
        .chain(&class.implements)
        .chain(&class.traits)
    {
        if !out.iter().any(|seen| seen.eq_ignore_ascii_case(name)) {
            out.push(name.clone());
        }
    }
    out
}

/// Property родителей, интерфейсов и trait-ов, влитые в класс вместе с цепочкой предков (ближайшее определение
/// побеждает).
///
/// Обход в ширину: сначала прямые родители (`extends`, `implements`, `traits`), затем их родители. Собственные
/// property класса и ближайших предков регистрируются раньше, поэтому дальние копии не перекрывают их.
/// Найденные предки дописываются в `extends` результата.
pub fn merge_inheritance(classes: Vec<ClassFacts>) -> Vec<ClassFacts> {
    let mut by_full: HashMap<String, usize> = HashMap::new();
    let mut by_short: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, class) in classes.iter().enumerate() {
        by_full.insert(class.name.to_ascii_lowercase(), i);
        by_short
            .entry(short_name(&class.name).to_ascii_lowercase())
            .or_default()
            .push(i);
    }
    let resolve = |name: &str| -> Option<usize> {
        let lower = name.trim_start_matches('\\').to_ascii_lowercase();
        by_full.get(&lower).copied().or_else(|| {
            let found = by_short.get(short_name(&lower))?;
            (found.len() == 1).then_some(found[0])
        })
    };
    classes
        .iter()
        .map(|class| {
            let mut merged = class.clone();
            let mut seen: HashSet<String> =
                class.properties.iter().map(|p| p.name.clone()).collect();
            let mut frontier: Vec<String> = direct_parents(class);
            let mut visited: HashSet<usize> = HashSet::new();
            let mut ancestors = Vec::new();
            while !frontier.is_empty() {
                let mut next = Vec::new();
                for name in frontier {
                    let Some(at) = resolve(&name) else { continue };
                    if !visited.insert(at) {
                        continue;
                    }
                    ancestors.push(at);
                    next.extend(direct_parents(&classes[at]));
                }
                frontier = next;
            }
            for at in ancestors {
                let parent = &classes[at];
                merged.extends.push(parent.name.clone());
                for property in &parent.properties {
                    if seen.insert(property.name.clone()) {
                        merged.properties.push(property.clone());
                    }
                }
            }
            merged
        })
        .collect()
}

fn short_name(name: &str) -> &str {
    name.rsplit('\\').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use facts::PropFacts;

    fn klass(name: &str, extends: &[&str], props: &[(&str, Option<Value>)]) -> ClassFacts {
        ClassFacts {
            file: "x.php".into(),
            name: name.into(),
            short: short_name(name).into(),
            extends: extends.iter().map(|s| s.to_string()).collect(),
            implements: vec![],
            traits: vec![],
            properties: props
                .iter()
                .map(|(n, d)| PropFacts {
                    name: (*n).into(),
                    ty: None,
                    default: d.clone(),
                    promoted: false,
                })
                .collect(),
        }
    }

    #[test]
    fn parent_properties_and_table_name_are_inherited() {
        let base = klass(
            "Billet",
            &[],
            &[
                ("tableName", Some(Value::Str("t".into()))),
                ("fieldsMap", Some(Value::Array(vec![]))),
            ],
        );
        let child = klass("Common\\HotelChain", &["Billet"], &[("id", None)]);
        let merged = merge_inheritance(vec![base, child]);
        let chain = merged.iter().find(|c| c.short == "HotelChain").unwrap();
        assert!(chain.properties.iter().any(|p| p.name == "tableName"));
        assert!(chain.extends.iter().any(|e| e == "Billet"));
    }

    /// Разбор исходника через `file_facts`: классы, интерфейсы и trait-ы.
    fn parsed(src: &str) -> Vec<ClassFacts> {
        facts::file_facts("x.php", src.as_bytes())
    }

    /// Временный проект на диске для `scan_parents` (каталог уникален для теста).
    fn project(tag: &str, src: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("callgraph-flow-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.php"), src).unwrap();
        dir
    }

    const ITEM: &str = r#"<?php
namespace App;

interface Src {}

trait HasName {
    private string $name;
}

class Item implements Src {
    use HasName;
    public int $id;
}
"#;

    #[test]
    fn interface_and_trait_are_parents() {
        let classes = parsed(ITEM);
        let item = classes.iter().find(|c| c.short == "Item").unwrap();
        assert_eq!(item.extends, Vec::<String>::new());
        assert_eq!(item.implements, vec!["App\\Src"]);
        assert_eq!(item.traits, vec!["App\\HasName"]);

        let dir = project("parents", ITEM);
        let parents = scan_parents(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        // `scan_parents` отдаёт только прямых родителей: интерфейс и trait, без разворота цепочки.
        assert_eq!(
            parents.get("App\\Item").cloned().unwrap_or_default(),
            vec!["App\\Src".to_string(), "App\\HasName".to_string()]
        );

        let merged = merge_inheritance(classes);
        let item = merged.iter().find(|c| c.short == "Item").unwrap();
        assert!(item.extends.iter().any(|p| p == "App\\Src"), "{item:?}");
        assert!(item.extends.iter().any(|p| p == "App\\HasName"), "{item:?}");
    }

    #[test]
    fn trait_property_is_inherited_by_using_class() {
        let merged = merge_inheritance(parsed(ITEM));
        let item = merged.iter().find(|c| c.short == "Item").unwrap();
        let name = item
            .properties
            .iter()
            .find(|p| p.name == "name")
            .unwrap_or_else(|| panic!("нет property name: {item:?}"));
        assert_eq!(name.ty.as_deref(), Some("string"));
    }

    #[test]
    fn transitive_parents_include_extends_and_implements() {
        let src = r#"<?php
namespace App;

interface Src {}

class Base implements Src {}

trait Sluggable {
    private string $slug;
}

class Child extends Base implements Src {
    use Sluggable;
}
"#;
        let dir = project("transitive", src);
        let parents = scan_parents(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        let child = parents.get("App\\Child").cloned().unwrap_or_default();
        // Прямые родители: Base, Src (повтор из `implements` убран), Sluggable.
        assert!(child.contains(&"App\\Base".to_string()), "{child:?}");
        assert!(child.contains(&"App\\Src".to_string()), "{child:?}");
        assert!(child.contains(&"App\\Sluggable".to_string()), "{child:?}");
        assert_eq!(child.len(), 3, "{child:?}");

        // `merge_inheritance` разворачивает цепочку: в `extends` попадают и предки Base.
        let merged = merge_inheritance(parsed(src));
        let child = merged.iter().find(|c| c.short == "Child").unwrap();
        for parent in ["App\\Base", "App\\Src", "App\\Sluggable"] {
            assert!(
                child.extends.iter().any(|p| p == parent),
                "нет {parent}: {child:?}"
            );
        }
        assert!(child.properties.iter().any(|p| p.name == "slug"));
    }
}

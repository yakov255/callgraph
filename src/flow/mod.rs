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
pub use plugin::{ColumnRef, Config, FlowHandler, Mapping, Registry};

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
        .map(|class| (class.name, class.extends))
        .collect())
}

/// Property родителей и цепочка `extends`, влитые в класс (ближайшее определение побеждает).
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
            let mut queue: Vec<String> = class.extends.clone();
            let mut visited: HashSet<usize> = HashSet::new();
            let mut ancestors = Vec::new();
            while let Some(name) = queue.pop() {
                let Some(at) = resolve(&name) else { continue };
                if !visited.insert(at) {
                    continue;
                }
                ancestors.push(at);
                queue.extend(classes[at].extends.iter().cloned());
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
}

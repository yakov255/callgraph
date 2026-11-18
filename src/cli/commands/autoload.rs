//! `callgraph autoload <class>... [--project <alias|каталог>] [--json]`: резолв класса через
//! composer-автозагрузку проекта (`crate::autoload`).
//!
//! Полезен, когда файл класса не найден графом (vendor, classmap, второй composer-корень):
//! выдаёт путь, корень `composer.json`, каталог vendor и правило (psr-4 / psr-0 / classmap).

use std::io::Write;
use std::path::Path;

use crate::autoload::{AutoloadMap, ClassSource, ResolvedClass};
use crate::cli::Command;
use crate::error::Result;
use crate::skill::SkillHome;

pub struct AutoloadCommand {
    classes: Vec<String>,
    project: Option<String>,
    json: bool,
    home: SkillHome,
}

impl AutoloadCommand {
    pub fn new(classes: Vec<String>, project: Option<String>, json: bool, home: SkillHome) -> Self {
        Self {
            classes,
            project,
            json,
            home,
        }
    }
}

impl Command for AutoloadCommand {
    fn run(&self, out: &mut dyn Write, _err: &mut dyn Write) -> Result<i32> {
        let root = super::project_root(&self.home, self.project.as_deref())?;
        let map = AutoloadMap::discover(&root);
        let mut hits = Vec::new();
        for class in &self.classes {
            hits.push((class, map.resolve(class, None)));
        }
        if self.json {
            writeln!(out, "{}", json(&root, &hits)?)?;
            return Ok(0);
        }
        out.write_all(text(&root, &hits).as_bytes())?;
        Ok(0)
    }
}

fn text(root: &Path, hits: &[(&String, Option<ResolvedClass>)]) -> String {
    let mut out = format!("root: {}\n", root.display());
    for (class, hit) in hits {
        match hit {
            Some(hit) => {
                let relative = hit.path.strip_prefix(root).unwrap_or(&hit.path).display();
                out.push_str(&format!(
                    "{class} → {relative}  [{}, {}]\n",
                    hit.kind.label(),
                    source(hit)
                ));
            }
            None => out.push_str(&format!("{class} → not found\n")),
        }
    }
    out
}

fn json(root: &Path, hits: &[(&String, Option<ResolvedClass>)]) -> Result<String> {
    let items: Vec<serde_json::Value> = hits
        .iter()
        .map(|(class, hit)| match hit {
            Some(hit) => serde_json::json!({
                "class": class,
                "path": hit.path,
                "root": hit.root,
                "vendor": hit.vendor,
                "kind": hit.kind.label(),
                "source": source(hit),
            }),
            None => serde_json::json!({
                "class": class,
                "found": false,
            }),
        })
        .collect();
    Ok(serde_json::json!({
        "root": root,
        "classes": items,
    })
    .to_string())
}

fn source(hit: &ResolvedClass) -> String {
    match &hit.source {
        ClassSource::Own => "own".to_owned(),
        ClassSource::Vendor(name) => name.clone(),
    }
}

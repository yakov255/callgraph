//! `callgraph refs <rN> <k> [--project <alias|каталог>]`: раскрывает сноску из ответа
//! `show --body --refs` — исходник класса и новые сноски на типы в его сигнатурах.

use crate::cli::Command;
use crate::error::CallgraphError;
use crate::refs;
use crate::skill::SkillHome;
use std::io::Write;

pub struct RefsCommand {
    id: String,
    key: usize,
    project: Option<String>,
    home: SkillHome,
}

impl RefsCommand {
    pub fn new(id: String, key: usize, project: Option<String>, home: SkillHome) -> Self {
        Self {
            id,
            key,
            project,
            home,
        }
    }
}

impl Command for RefsCommand {
    fn run(&self, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, CallgraphError> {
        let root = super::project_root(&self.home, self.project.as_deref())?;
        let Some(id) = refs::parse_id(&self.id) else {
            return Err(usage());
        };
        let Some(book) = refs::book(&root, id) else {
            return Err(CallgraphError::Usage(crate::tf!(
                "refs: result r{id} not found (run show --body --refs first)",
                "refs: az r{id} eredmény nem található (futtasd előbb: show --body --refs)"
            )));
        };
        let Some(entry) = book.entries.iter().find(|entry| entry.number == self.key) else {
            return Err(usage());
        };
        let Some(class) = refs::class_source(&root, entry) else {
            match entry.kind.as_str() {
                "internal" => {
                    writeln!(out, "#{}  \\{}  [internal · php]", entry.number, entry.fqcn)?;
                }
                _ => {
                    writeln!(
                        err,
                        "{}",
                        crate::tf!(
                            "refs: #{} has no source ({})",
                            "refs: a #{} forrása nem található ({})",
                            self.key,
                            entry.fqcn
                        )
                    )?;
                }
            }
            return Ok(0);
        };
        writeln!(out, "{}", class.header)?;
        for (number, content) in &class.lines {
            writeln!(out, "{number:>6}  {content}")?;
        }
        out.write_all(refs::render_footnotes(&class.entries, &root).as_bytes())?;
        if let Some(new_id) = refs::save(&root, class.entries.clone()) {
            write!(out, "{}", refs::trailer(new_id, &class.entries))?;
        }
        Ok(0)
    }
}

fn usage() -> CallgraphError {
    CallgraphError::Usage(
        crate::t!(
            "callgraph refs <rN> <k> [--project <alias|dir>]  (expand footnote #k of result rN)",
            "callgraph refs <rN> <k> [--project <alias|mappa>]  (az rN eredmény #k lábjegyzetének kifejtése)"
        )
        .into(),
    )
}

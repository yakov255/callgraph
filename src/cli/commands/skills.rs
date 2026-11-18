//! `callgraph skills get [core] [--full]`: встроенный скилл, совпадающий с версией бинарника.
//!
//! У агента всегда под рукой тот же текст, что лежит в `skill/SKILL.md` репозитория.

use std::io::Write;

use crate::cli::Command;
use crate::error::Result;

/// Скилл вкомпилирован в бинарник: версия не может разойтись.
const SKILL: &str = include_str!("../../../skill/SKILL.md");

pub struct SkillsCommand {
    _full: bool,
}

impl SkillsCommand {
    pub fn new(full: bool) -> Self {
        Self { _full: full }
    }
}

impl Command for SkillsCommand {
    fn run(&self, out: &mut dyn Write, _err: &mut dyn Write) -> Result<i32> {
        out.write_all(SKILL.as_bytes())?;
        Ok(0)
    }
}

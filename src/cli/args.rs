//! Parancssori argumentumok: elemzés (`callgraph [--lang <id,...>] [--cache <fájl>] [--no-cache] <fájl|mappa> [...]`)
//! és cache-takarítás (`callgraph prune [--cache <fájl>] [--dry-run]`).

use std::path::PathBuf;

use crate::error::{CallgraphError, Result};
use crate::lang::LanguageRegistry;
use crate::query::format::BodyMode;

use super::generate_args::{parse_gen, parse_undo};
use super::next_args::parse_next;
use super::profile_args::{LayerArgs, parse_profile};
use super::rename_args::parse_rename;
use super::stats_args::parse_stats;
use super::telemetry_args::{TelemetryAction, parse_telemetry};
use super::uses_args::{parse_deps, parse_uses};

fn usage() -> &'static str {
    crate::t!(
        "callgraph [--lang <id,...>] [--cache <file>] [--no-cache] <file|dir> [...] | callgraph prune [--cache <file>] [--dry-run] | callgraph set-lang [en|hu] | callgraph self-update [--check] | callgraph note add|list|rm … | callgraph refresh [--project <alias|dir>] [--background] [<path>…] | callgraph locate <term>... [--all] [--project <alias|dir>] | callgraph hook install|uninstall|status | callgraph stats [--events] [--days N] | callgraph flow map [--project <alias|dir>] [--json] [--table <name>] [--full] | callgraph trace <term>... [--project <alias|dir>] [--json] [--full] [--must] | callgraph map <term>... | callgraph read <path[::method]> [--body|--api|--flow|--lines N-M] | callgraph find <def|callers|string|const> <value> | callgraph flow <term>... | callgraph skills get [--full] | callgraph autoload <Class>... [--project <alias|dir>] [--json] | callgraph refs <rN> <k> [--project <alias|dir>] | callgraph mcp [--auto-update] | callgraph --version  (any command: --no-gitignore = ignore the .gitignore files)",
        "callgraph [--lang <id,...>] [--cache <fájl>] [--no-cache] <fájl|mappa> [...] | callgraph prune [--cache <fájl>] [--dry-run] | callgraph set-lang [en|hu] | callgraph self-update [--check] | callgraph note add|list|rm … | callgraph refresh [--project <alias|mappa>] [--background] [<út>…] | callgraph locate <kifejezés>... [--all] [--project <alias|mappa>] | callgraph hook install|uninstall|status | callgraph stats [--events] [--days N] | callgraph flow map [--project <alias|mappa>] [--json] [--table <név>] [--full] | callgraph trace <kifejezés>... [--project <alias|mappa>] [--json] [--full] [--must] | callgraph map <kifejezés>... | callgraph read <út[::metódus]> [--body|--api|--flow|--lines N-M] | callgraph find <def|callers|string|const> <érték> | callgraph flow <kifejezés>... | callgraph skills get [--full] | callgraph autoload <Osztály>... [--project <alias|mappa>] [--json] | callgraph refs <rN> <k> [--project <alias|mappa>] | callgraph mcp [--auto-update] | callgraph --version  (bármely parancsnál: --no-gitignore = a .gitignore fájlok nélkül)"
    )
}

fn prune_usage() -> &'static str {
    crate::t!(
        "callgraph prune [<dir|file> ...] [--cache <file>] [--dry-run]",
        "callgraph prune [<mappa|fájl> ...] [--cache <fájl>] [--dry-run]"
    )
}

fn callers_usage() -> &'static str {
    crate::t!(
        "callgraph callers <pattern> [--depth N] [--project <dir>] [--by-layer]",
        "callgraph callers <minta> [--depth N] [--project <mappa>] [--by-layer]"
    )
}

/// Несколько `--lines`: диапазоны склеиваются через запятую.
fn push_range(lines: &mut Option<String>, value: String) {
    match lines {
        Some(existing) => {
            existing.push(',');
            existing.push_str(&value);
        }
        None => *lines = Some(value),
    }
}

fn show_usage() -> &'static str {
    crate::t!(
        "callgraph show [--project <root>] [--label <name>] [--format <f>] [--body [--doc|--raw] [--refs]|--lines <n>-<m>|--strings <pattern>|--const <pattern>] [--grep <pattern>] [--category <cat,...>] [--depth N] [--guess] [--notes <pattern>] [--layer <name,...>] [--layers] [--by-layer] [--full] <path[::m]>...",
        "callgraph show [--project <gyökér>] [--label <név>] [--format <f>] [--body [--doc|--raw] [--refs]|--lines <n>-<m>|--strings <minta>|--const <minta>] [--grep <minta>] [--category <kat,...>] [--depth N] [--guess] [--notes <minta>] [--layer <név,...>] [--layers] [--by-layer] [--full] <út[::m]>..."
    )
}

fn note_usage() -> &'static str {
    crate::t!(
        "callgraph note add <file>[::<method>] \"<text>\" | note list [<path>] | note rm <id>  [--project <alias|root>]",
        "callgraph note add <fájl>[::<metódus>] \"<szöveg>\" | note list [<út>] | note rm <id>  [--project <alias|gyökér>]"
    )
}

fn refresh_usage() -> &'static str {
    crate::t!(
        "callgraph refresh [--project <alias|root>] [--background] [<file|dir> ...]  (no path: every changed / new / deleted file of the project)",
        "callgraph refresh [--project <alias|gyökér>] [--background] [<fájl|mappa> ...]  (út nélkül: a projekt minden változott / új / törölt fájlja)"
    )
}

fn trace_usage() -> &'static str {
    crate::t!(
        "callgraph trace <term>... [--project <alias|dir>] [--json] [--full] [--must]  (data flow: where a DB column / array key / property is read; --must: every path)",
        "callgraph trace <kifejezés>... [--project <alias|mappa>] [--json] [--full] [--must]  (adatfolyam: hol olvassák a DB-oszlopot / tömbkulcsot / property-t; --must: minden úton)"
    )
}

fn flow_usage() -> &'static str {
    crate::t!(
        "callgraph flow map [--project <alias|dir>] [--json] [--table <name>] [--full]  (table <-> property mapping from PHP classes and the .callgraph-flow.json handlers)",
        "callgraph flow map [--project <alias|mappa>] [--json] [--table <név>] [--full]  (tábla <-> property megfeleltetés PHP-osztályokból és a .callgraph-flow.json handlereiből)"
    )
}

fn hook_usage() -> &'static str {
    crate::t!(
        "callgraph hook install|install-session|uninstall|status [--settings <file>]  (Claude Code hooks: PostToolUse background refresh after Edit / Write / MultiEdit; SessionStart recommended settings and update notice; default settings: ~/.claude/settings.json) | callgraph hook post-edit|session-start  (run by the hooks) | callgraph hook never hook|mcp|telemetry  (stop recommending it) | callgraph hook reset [item]  (recommend again)",
        "callgraph hook install|install-session|uninstall|status [--settings <fájl>]  (Claude Code-hookok: PostToolUse háttér-frissítés Edit / Write / MultiEdit után; SessionStart ajánlott beállítások és frissítés-értesítés; alapértelmezett beállítások: ~/.claude/settings.json) | callgraph hook post-edit|session-start  (a hookok futtatják) | callgraph hook never hook|mcp|telemetry  (többé ne ajánlja) | callgraph hook reset [tétel]  (újra ajánlja)"
    )
}

/// Globális kapcsoló minden projektet bejáró parancshoz (`show`, `locate`, `profile`, `refresh`, `callers`, elemzés):
/// a `.gitignore` fájlok nem számítanak (a `.callgraphignore` és a beépített kihagyott mappák igen).
pub const NO_GITIGNORE_FLAG: &str = "--no-gitignore";

/// A globális `--no-gitignore` kapcsoló kivétele az argumentumok közül (bárhol, az első `--` előtt); `true`, ha
/// szerepelt. A belépő ilyenkor `CALLGRAPH_GITIGNORE=0`-t állít be, így a bejáró (`io::FileWalker`) és a
/// gyermek-folyamatok (háttér-`refresh`) is ezt látják.
pub fn take_no_gitignore(args: &mut Vec<String>) -> bool {
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let before = args.len();
    let mut at = 0;
    args.retain(|a| {
        at += 1;
        at > end || a != NO_GITIGNORE_FLAG
    });
    args.len() != before
}

/// A `hook` alparancs művelete (Claude Code-hookok: fájl-írás utáni frissítés, session-indítás; `crate::hook`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAction {
    /// A hook maga: a stdin JSON-jából a szerkesztett fájl háttér-frissítése.
    PostEdit,
    /// A hook felvétele a `settings.json`-ba (idempotens, `.bak` mentéssel).
    Install,
    /// A hook eltávolítása a `settings.json`-ból.
    Uninstall,
    /// Telepítve van-e.
    Status,
    /// A `SessionStart` hook maga: ajánlások és frissítés-értesítés a session elején.
    SessionStart,
    /// A `SessionStart` hook felvétele a `settings.json`-ba.
    InstallSession,
    /// Egy ajánlás végleges elhallgattatása (`hook never hook|mcp|telemetry`).
    Never(String),
    /// Az ajánlások állapotának visszaállítása (egy tételé, vagy tétel nélkül mindegyiké).
    Reset(Option<String>),
}

/// A `note` alparancs művelete (PILOT: jegyzetek fájlhoz / metódushoz).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteAction {
    /// Új jegyzet: `<fájl>[::<metódus>]` és a szöveg.
    Add { target: String, text: String },
    /// Jegyzetek listája, opcionálisan egy fájl / mappa alatt.
    List { path: Option<String> },
    /// Jegyzet törlése azonosító szerint.
    Rm { id: String },
}

/// A feldolgozott parancssori argumentumok — alparancsonként egy változat (Command minta bemenete). Futásonként
/// egyszer jön létre, ezért a nagy `Show`-változat dobozolása nem érné meg.
#[derive(Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum CliArgs {
    /// Elemzendő fájlok és mappák; `cache`: explicit cache-fájl (`None` = alapértelmezett), `no_cache`: kikapcsolva.
    Analyze {
        paths: Vec<PathBuf>,
        lang: Option<Vec<String>>,
        cache: Option<PathBuf>,
        no_cache: bool,
    },
    /// Cache-takarítás, opcionális cache-útvonallal és írás nélküli móddal.
    Prune {
        paths: Vec<PathBuf>,
        cache: Option<PathBuf>,
        dry_run: bool,
    },
    /// Projekt-szintű fordított hívó-keresés.
    Callers {
        pattern: String,
        depth: usize,
        project: Option<PathBuf>,
        /// `--by-layer`: a hívók rétegek szerint rendezve, `layer` mezővel.
        by_layer: bool,
    },
    /// Egy vagy több fájl gráfjának lekérdezése.
    Show {
        paths: Vec<PathBuf>,
        project: Option<PathBuf>,
        label: Option<String>,
        format: String,
        full: bool,
        limit: Option<usize>,
        lines: Option<String>,
        strings: Option<String>,
        const_filter: Option<String>,
        body: bool,
        /// A `--body` nézete: tömör (alap), `--doc` (docblockkal) vagy `--raw` (szűretlen); mindkettő `--body`-t is jelent.
        body_mode: BodyMode,
        /// `--refs`: маркеры типов и сноски в `--body`.
        refs: bool,
        grep: Option<String>,
        category: Option<Vec<String>>,
        depth: usize,
        guess: bool,
        notes: Option<String>,
        /// A réteg-kapcsolók (`--layer`, `--layers`, `--by-layer`; `profile_args.rs`).
        layers: LayerArgs,
    },
    /// `profile [--project <alias|mappa>] [--full] [--with-vendor] [--packages]`: a projekt-profil (architektúra,
    /// rétegek, minták); `with_vendor`: a beágyazott külső kód is számít; `packages`: a közvetlen függőségek listája.
    Profile {
        project: Option<String>,
        full: bool,
        with_vendor: bool,
        packages: bool,
    },
    /// `note add|list|rm` (PILOT); `project`: alias vagy gyökér-út.
    Note {
        action: NoteAction,
        project: Option<String>,
    },
    /// `refresh`: a megadott fájlok / mappák (út nélkül a teljes projekt) naprakészre hozása a projekt-gráfban;
    /// `project`: alias vagy gyökér-út; `background`: leválasztott folyamatban, azonnal visszatér.
    Refresh {
        paths: Vec<PathBuf>,
        project: Option<String>,
        background: bool,
    },
    /// `locate`: kiindulópont-kereső a feladat neveire / fogalmaira; `all`: minden egyező fájl (alapból 5);
    /// `project`: alias vagy gyökér-út.
    Locate {
        terms: Vec<String>,
        all: bool,
        files: bool,
        json: bool,
        exact: bool,
        project: Option<String>,
    },
    /// `uses <csomag> [--calls]`: hol és hogyan használja a projekt egy függőségét; `calls`: fájlonként minden
    /// hívás-sor; `project`: alias vagy gyökér-út.
    Uses {
        package: String,
        calls: bool,
        project: Option<String>,
    },
    /// `deps`: fölösleges, hiányzó és csak dev függőségek; `project`: alias vagy gyökér-út.
    Deps {
        project: Option<String>,
    },
    /// `hook post-edit|install|uninstall|status`; `settings`: a Claude Code `settings.json` helye (alapból
    /// `~/.claude/settings.json`).
    Hook {
        action: HookAction,
        settings: Option<PathBuf>,
    },
    /// `stats [--events] [--days N]`: a helyi használati statisztika az elmúlt N napról (`crate::stats`);
    /// `--events`: a helyi eseménynapló és a leggyakoribb parancs-átmenetek.
    Stats {
        days: u32,
        events: bool,
    },
    /// `next <rN> <k>[,<k>] [--project X]`: egy korábbi eredmény javasolt következő lépése (`crate::next`).
    Next {
        id: String,
        choices: Vec<usize>,
        project: Option<String>,
    },
    /// `gen '<kifejezés>' [--dry|--write|--into|--replace …]`: kód-generálás rövid kifejezésből (`crate::generate`).
    Generate {
        request: crate::generate::GenRequest,
        project: Option<String>,
    },
    /// `rename <route|class|method> <régi> <új> [--dry] [--strings] [--keep-table]`: biztonságos átnevezés (`crate::rename`).
    Rename {
        request: crate::rename::RenameRequest,
        project: Option<String>,
    },
    /// `undo [<fájl>]`: a legutóbbi generátor-módosítás visszavonása a `.callgraph/backups/` mappából.
    Undo {
        file: Option<String>,
        project: Option<String>,
    },
    /// `telemetry on|off|status|preview`: a helyi statisztika opt-in beküldése (`crate::stats::telemetry`).
    Telemetry {
        action: TelemetryAction,
    },
    /// `--version` / `-V`: a verzió és a platform.
    /// `flow map`: сопоставление таблица ↔ property из PHP-классов, с плагинами.
    Flow {
        project: Option<String>,
        json: bool,
        table: Option<String>,
        full: bool,
    },
    /// `autoload`: резолв класса через composer-автозагрузку проекта.
    Autoload {
        classes: Vec<String>,
        project: Option<String>,
        json: bool,
    },
    /// `context <file>:<line> [-n N]`: строки вокруг места с границами метода.
    Context {
        target: String,
        around: u32,
        project: Option<String>,
    },
    /// `find callers --json`: машинный список прямых вызовов.
    FindCallersJson {
        name: String,
        project: Option<String>,
    },
    /// `find path <glob>`: файлы по пути/имени.
    FindPath {
        glob: String,
        json: bool,
        project: Option<String>,
    },
    /// `find impl|sub|trait <Symbol>`: реализации, наследники, пользователи trait.
    FindRelation {
        relation: String,
        symbol: String,
        files: bool,
        json: bool,
        project: Option<String>,
    },
    /// `find text <pattern>`: текстовый поиск по не-AST файлам (neon, yaml, feature, env).
    FindText {
        pattern: String,
        globs: Option<Vec<String>>,
        files: bool,
        json: bool,
        project: Option<String>,
    },
    /// `find refs <Symbol>`: все упоминания символа (use, new, типы, docblock, …).
    FindRefs {
        symbol: String,
        kinds: Option<Vec<String>>,
        files: bool,
        json: bool,
        project: Option<String>,
    },
    /// `skills get [core] [--full]`: встроенный скилл (версия совпадает с бинарником).
    Skills {
        full: bool,
    },
    /// `refs`: раскрывает сноску типа из предыдущего ответа (`refs rN k`).
    Refs {
        id: String,
        key: usize,
        project: Option<String>,
    },
    /// `trace`: цепочки потока данных от источника до чтения.
    Trace {
        terms: Vec<String>,
        project: Option<String>,
        json: bool,
        full: bool,
        must: bool,
    },
    Version,
    /// `--help` / `-h` / `help`: a teljes súgó.
    Help,
    /// `mcp [--auto-update]`: MCP-szerver stdio-n; `--auto-update`: indításkor naponta egyszer frissít.
    Mcp {
        auto_update: bool,
    },
    /// Skill-parancs (`project`, `feedback`, `wrap`, `error`, `set-lang`, `self-update`, `skill-sync`) nyers argumentumokkal.
    Skill {
        command: String,
        args: Vec<String>,
    },
}

impl CliArgs {
    /// A program neve nélküli argumentumlistából állít elő parancsot.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self> {
        let args: Vec<String> = args.into_iter().collect();
        if args.len() == 1 && matches!(args[0].as_str(), "--version" | "-V" | "version") {
            return Ok(Self::Version);
        }
        if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h" | "help") {
            return Ok(Self::Help);
        }
        // Современный surface для агента: map / read / find / flow / skills; старые команды остаются.
        match args.first().map(String::as_str) {
            Some("map") => return Self::parse_locate(&args[1..]),
            Some("read") => return Self::parse_read(&args[1..]),
            Some("find") => return Self::parse_find(&args[1..]),
            Some("skills") => return Self::parse_skills(&args[1..]),
            Some("context") => return Self::parse_context(&args[1..]),
            Some("flow") if args.get(1).map(String::as_str) != Some("map") => {
                return Self::parse_trace(&args[1..]);
            }
            _ => {}
        }
        if args.first().is_some_and(|a| a == "mcp") {
            let auto_update = args
                .get(1)
                .is_some_and(|a| a == crate::mcp::AUTO_UPDATE_FLAG);
            if args.len() > 1 + usize::from(auto_update) {
                return Err(CallgraphError::Usage(
                    crate::t!(
                        "callgraph mcp [--auto-update] (MCP server on stdio; --auto-update: update itself at start, at most once a day)",
                        "callgraph mcp [--auto-update] (MCP-szerver stdio-n; --auto-update: indításkor frissíti magát, naponta legfeljebb egyszer)"
                    )
                    .into(),
                ));
            }
            return Ok(Self::Mcp { auto_update });
        }
        if args.first().is_some_and(|a| a == "prune") {
            return Self::parse_prune(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "callers") {
            return Self::parse_callers(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "note") {
            return Self::parse_note(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "show") {
            return Self::parse_show(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "refresh") {
            return Self::parse_refresh(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "locate") {
            return Self::parse_locate(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "uses") {
            return parse_uses(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "deps") {
            return parse_deps(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "flow") {
            return Self::parse_flow(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "trace") {
            return Self::parse_trace(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "autoload") {
            return Self::parse_autoload(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "refs") {
            return Self::parse_refs(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "hook") {
            return Self::parse_hook(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "profile") {
            return parse_profile(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "stats") {
            return parse_stats(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "next") {
            return parse_next(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "gen") {
            return parse_gen(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "rename") {
            return parse_rename(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "undo") {
            return parse_undo(&args[1..]);
        }
        if args.first().is_some_and(|a| a == "telemetry") {
            return parse_telemetry(&args[1..]);
        }
        // a skill-parancsok maguk értelmezik az argumentumaikat (a hibáik error-id-t kapnak)
        if let Some(command) = args.first().filter(|a| {
            matches!(
                a.as_str(),
                "project"
                    | "feedback"
                    | "wrap"
                    | "error"
                    | "set-lang"
                    | "self-update"
                    | "skill-sync"
                    | "usage-check"
            )
        }) {
            return Ok(Self::Skill {
                command: command.clone(),
                args: args[1..].to_vec(),
            });
        }
        Self::parse_analyze(args)
    }

    /// `read <path[::method]> [--body|--api|--flow|--lines N-M]`: умное чтение.
    /// Файл → outline, `file::method` → тело; у тела сноски `--refs` включены по умолчанию.
    fn parse_read(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(Self::read_usage().into());
        if args.iter().all(|arg| arg.starts_with('-')) {
            return Err(usage());
        }
        let mut out: Vec<String> = Vec::new();
        let mut body = false;
        let mut plain = false;
        for arg in args {
            match arg.as_str() {
                "--api" => {
                    out.extend(["--format".to_owned(), "api".to_owned()]);
                    plain = true;
                }
                "--flow" => {
                    out.extend(["--format".to_owned(), "flow".to_owned()]);
                    plain = true;
                }
                "--lines" => {
                    out.push("--lines".to_owned());
                    plain = true;
                }
                x if x.starts_with("--lines=") => {
                    out.push(x.to_owned());
                    plain = true;
                }
                "--body" => {
                    body = true;
                    out.push("--body".to_owned());
                }
                "--doc" => {
                    body = true;
                    out.push("--doc".to_owned());
                }
                "--raw" => {
                    body = true;
                    out.push("--raw".to_owned());
                }
                x => out.push(x.to_owned()),
            }
        }
        if !plain {
            let method = args
                .iter()
                .any(|arg| !arg.starts_with('-') && arg.contains("::"));
            if body || method {
                if !body {
                    out.push("--body".to_owned());
                }
                if !out.iter().any(|arg| arg == "--refs") {
                    out.push("--refs".to_owned());
                }
            }
        }
        Self::parse_show(&out)
    }

    /// `find <def|callers|string|const> <value>`: поиск по карте проекта.
    fn parse_find(args: &[String]) -> Result<Self> {
        let Some(action) = args.first().map(String::as_str) else {
            return Err(CallgraphError::Usage(Self::find_usage().into()));
        };
        let rest = &args[1..];
        match action {
            "def" => {
                let mut out = vec!["--all".to_owned()];
                out.extend(rest.iter().cloned());
                Self::parse_locate(&out)
            }
            "callers" => Self::parse_find_callers(rest),
            "string" => {
                let mut out = vec![".".to_owned()];
                out.extend_from_slice(&Self::with_flag(rest, "--strings")?);
                Self::parse_show(&out)
            }
            "const" => {
                let mut out = vec![".".to_owned()];
                out.extend_from_slice(&Self::with_flag(rest, "--const")?);
                Self::parse_show(&out)
            }
            "refs" => Self::parse_find_refs(rest),
            "text" => Self::parse_find_text(rest),
            "path" => Self::parse_find_path(rest),
            "impl" | "sub" | "trait" => Self::parse_find_relation(action, rest),
            _ => Err(CallgraphError::Usage(Self::find_usage().into())),
        }
    }

    /// `find callers <name> [--depth N] [--json] [--project X]`.
    fn parse_find_callers(args: &[String]) -> Result<Self> {
        let json = args.iter().any(|arg| arg == "--json");
        let mut depth = 1usize;
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--depth" && i + 1 < args.len() {
                depth = args[i + 1].parse().unwrap_or(1);
                i += 2;
                continue;
            }
            if let Some(value) = args[i].strip_prefix("--depth=") {
                depth = value.parse().unwrap_or(1);
            }
            i += 1;
        }
        if !json {
            let mut out = vec![".".to_owned()];
            out.extend_from_slice(&Self::with_flag(args, "--grep")?);
            return Self::parse_show(&out);
        }
        if depth > 1 {
            return Err(CallgraphError::Usage(
                crate::t!(
                    "find callers --json supports --depth 1 only (use the text output for deeper levels)",
                    "a find callers --json csak --depth 1-et támogat (mélyebb szintekhez a szöveges kimenet)"
                )
                .into(),
            ));
        }
        let usage = || CallgraphError::Usage(Self::find_usage().into());
        let mut name = None;
        let mut project = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--json" => {}
                "--depth" => i += 1,
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                x if x.starts_with('-') && x.len() > 1 => return Err(usage()),
                x if name.is_none() => name = Some(x.to_owned()),
                _ => return Err(usage()),
            }
            i += 1;
        }
        Ok(Self::FindCallersJson {
            name: name.ok_or_else(usage)?,
            project,
        })
    }

    /// `find path <glob> [--json] [--project X]`.
    fn parse_find_path(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(Self::find_usage().into());
        let mut glob = None;
        let mut json = false;
        let mut project = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--json" => json = true,
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                x if x.starts_with('-') && x.len() > 1 => return Err(usage()),
                x if glob.is_none() => glob = Some(x.to_owned()),
                _ => return Err(usage()),
            }
            i += 1;
        }
        Ok(Self::FindPath {
            glob: glob.ok_or_else(usage)?,
            json,
            project,
        })
    }

    /// `find impl|sub|trait <Symbol> [--files] [--json] [--project X]`.
    fn parse_find_relation(action: &str, args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(Self::find_usage().into());
        let mut symbol = None;
        let mut files = false;
        let mut json = false;
        let mut project = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--files" => files = true,
                "--json" => json = true,
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                x if x.starts_with('-') && x.len() > 1 => return Err(usage()),
                x if symbol.is_none() => symbol = Some(x.to_owned()),
                _ => return Err(usage()),
            }
            i += 1;
        }
        Ok(Self::FindRelation {
            relation: action.to_owned(),
            symbol: symbol.ok_or_else(usage)?,
            files,
            json,
            project,
        })
    }

    /// `find text <pattern> [--glob '*.neon,*.yaml'] [--files] [--json] [--project X]`.
    fn parse_find_text(args: &[String]) -> Result<Self> {
        let mut pattern = None;
        let mut globs = None;
        let mut files = false;
        let mut json = false;
        let mut project = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--files" => files = true,
                "--json" => json = true,
                "--glob" if i + 1 < args.len() => {
                    globs = Some(args[i + 1].split(',').map(str::to_owned).collect());
                    i += 1;
                }
                x if x.starts_with("--glob=") => {
                    globs = Some(x[7..].split(',').map(str::to_owned).collect());
                }
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                x if x.starts_with('-') && x.len() > 1 => {
                    return Err(crate::cli::commands::find_text::usage_error());
                }
                x if pattern.is_none() => pattern = Some(x.to_owned()),
                _ => return Err(crate::cli::commands::find_text::usage_error()),
            }
            i += 1;
        }
        let pattern = pattern.ok_or_else(crate::cli::commands::find_text::usage_error)?;
        Ok(Self::FindText {
            pattern,
            globs,
            files,
            json,
            project,
        })
    }

    /// `find refs <Symbol> [--kind use,new,…] [--files] [--json] [--project X]`.
    fn parse_find_refs(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(Self::find_usage().into());
        let mut symbol = None;
        let mut kinds = None;
        let mut files = false;
        let mut json = false;
        let mut project = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--files" => files = true,
                "--json" => json = true,
                "--kind" if i + 1 < args.len() => {
                    kinds = Some(args[i + 1].split(',').map(str::to_owned).collect());
                    i += 1;
                }
                x if x.starts_with("--kind=") => {
                    kinds = Some(x[7..].split(',').map(str::to_owned).collect());
                }
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                x if x.starts_with('-') && x.len() > 1 => return Err(usage()),
                x if symbol.is_none() => symbol = Some(x.to_owned()),
                _ => return Err(usage()),
            }
            i += 1;
        }
        let symbol = symbol.ok_or_else(usage)?;
        Ok(Self::FindRefs {
            symbol,
            kinds,
            files,
            json,
            project,
        })
    }

    /// Значение первого позиционного аргумента → `[flag, value, …флаги]`.
    fn with_flag(args: &[String], flag: &str) -> Result<Vec<String>> {
        let mut value = None;
        let mut pass = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg == "--project" || arg == "--depth" {
                pass.push(arg.clone());
                if i + 1 < args.len() {
                    pass.push(args[i + 1].clone());
                    i += 2;
                    continue;
                }
            } else if !arg.starts_with('-') {
                if value.is_none() {
                    value = Some(arg.clone());
                    i += 1;
                    continue;
                }
            } else {
                pass.push(arg.clone());
            }
            i += 1;
        }
        let value = value.ok_or_else(|| CallgraphError::Usage(Self::find_usage().into()))?;
        let mut out = vec![flag.to_owned(), value];
        out.extend(pass);
        Ok(out)
    }

    /// `skills get [core] [--full]`: встроенный скилл.
    fn parse_skills(args: &[String]) -> Result<Self> {
        let mut full = false;
        let mut action = None;
        for arg in args {
            match arg.as_str() {
                "--full" => full = true,
                x if x.starts_with('-') => {
                    return Err(CallgraphError::Usage(Self::skills_usage().into()));
                }
                x if action.is_none() => action = Some(x.to_owned()),
                _ => {}
            }
        }
        if action.as_deref().is_some_and(|action| action != "get") {
            return Err(CallgraphError::Usage(Self::skills_usage().into()));
        }
        Ok(Self::Skills { full })
    }

    /// `context <file>:<line> [-n N] [--project X]`.
    fn parse_context(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(Self::context_usage().into());
        let mut target = None;
        let mut around = 5;
        let mut project = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-n" | "--context" if i + 1 < args.len() => {
                    around = args[i + 1].parse().map_err(|_| usage())?;
                    i += 1;
                }
                x if x.starts_with("--context=") => {
                    around = x[10..].parse().map_err(|_| usage())?;
                }
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                x if x.starts_with('-') && x.len() > 1 => return Err(usage()),
                x if target.is_none() => target = Some(x.to_owned()),
                _ => return Err(usage()),
            }
            i += 1;
        }
        Ok(Self::Context {
            target: target.ok_or_else(usage)?,
            around,
            project,
        })
    }

    fn context_usage() -> &'static str {
        crate::t!(
            "callgraph context <file>:<line> [-n N] [--project <alias|dir>]",
            "callgraph context <fájl>:<sor> [-n N] [--project <alias|mappa>]"
        )
    }

    fn read_usage() -> &'static str {
        crate::t!(
            "callgraph read <path[::method]> [--body|--api|--flow|--lines N-M] [--project <alias|dir>]  (file: outline, file::method: body with type footnotes)",
            "callgraph read <út[::metódus]> [--body|--api|--flow|--lines N-M] [--project <alias|mappa>]  (fájl: outline, fájl::metódus: törzs típus-lábjegyzetekkel)"
        )
    }

    fn find_usage() -> &'static str {
        crate::t!(
            "callgraph context <file>:<line> [-n N] | callgraph find <def|callers|string|const|refs|text|path|impl|sub|trait> <value> [--depth N] [--kind …] [--files] [--project <alias|dir>]",
            "callgraph context <fájl>:<sor> [-n N] | callgraph find <def|callers|string|const|refs|text|path|impl|sub|trait> <érték> [--depth N] [--kind …] [--files] [--project <alias|mappa>]"
        )
    }

    fn skills_usage() -> &'static str {
        crate::t!(
            "callgraph skills get [core] [--full]  (the built-in, version-matched skill)",
            "callgraph skills get [core] [--full]  (a beépített, verzióhoz illő skill)"
        )
    }

    fn parse_show(args: &[String]) -> Result<Self> {
        let mut paths = Vec::new();
        let mut project = None;
        let mut label = None;
        let mut format = "outline".to_owned();
        let mut full = false;
        let mut limit = None;
        let mut lines = None;
        let mut strings = None;
        let mut const_filter = None;
        let mut body = false;
        let mut body_mode = BodyMode::Compact;
        let mut refs = false;
        let mut grep = None;
        let mut category = None;
        let mut depth = 1;
        let mut guess = false;
        let mut notes = None;
        let mut layers = LayerArgs::default();
        let mut i = 0;
        while i < args.len() {
            if let Some(used) = layers.take(&args[i..])? {
                i += used;
                continue;
            }
            match args[i].as_str() {
                x if x.starts_with("CALLGRAPH_OUT_LIMIT=") => {
                    limit = x.split_once('=').and_then(|(_, v)| v.parse().ok());
                    i += 1;
                }
                "--full" => {
                    full = true;
                    i += 1;
                }
                "--body" => {
                    body = true;
                    i += 1;
                }
                "--doc" => {
                    body = true;
                    if body_mode != BodyMode::Raw {
                        body_mode = BodyMode::Doc;
                    }
                    i += 1;
                }
                "--raw" => {
                    body = true;
                    body_mode = BodyMode::Raw;
                    i += 1;
                }
                "--refs" => {
                    refs = true;
                    i += 1;
                }
                "--guess" => {
                    guess = true;
                    i += 1;
                }
                "--grep" if i + 1 < args.len() => {
                    grep = Some(args[i + 1].clone());
                    i += 2;
                }
                x if x.starts_with("--grep=") => {
                    grep = Some(x[7..].to_owned());
                    i += 1;
                }
                "--notes" if i + 1 < args.len() => {
                    notes = Some(args[i + 1].clone());
                    i += 2;
                }
                x if x.starts_with("--notes=") => {
                    notes = Some(x[8..].to_owned());
                    i += 1;
                }
                "--category" if i + 1 < args.len() => {
                    category = Some(args[i + 1].split(',').map(str::to_owned).collect());
                    i += 2;
                }
                x if x.starts_with("--category=") => {
                    category = Some(x[11..].split(',').map(str::to_owned).collect());
                    i += 1;
                }
                "--depth" if i + 1 < args.len() => {
                    depth = args[i + 1]
                        .parse()
                        .map_err(|_| CallgraphError::Usage(show_usage().into()))?;
                    i += 2;
                }
                x if x.starts_with("--depth=") => {
                    depth = x[8..]
                        .parse()
                        .map_err(|_| CallgraphError::Usage(show_usage().into()))?;
                    i += 1;
                }
                "--lines" if i + 1 < args.len() => {
                    push_range(&mut lines, args[i + 1].clone());
                    i += 2;
                }
                x if x.starts_with("--lines=") => {
                    push_range(&mut lines, x[8..].to_owned());
                    i += 1;
                }
                "--strings" if i + 1 < args.len() => {
                    strings = Some(args[i + 1].clone());
                    i += 2;
                }
                x if x.starts_with("--strings=") => {
                    strings = Some(x[10..].to_owned());
                    i += 1;
                }
                "--const" if i + 1 < args.len() => {
                    const_filter = Some(args[i + 1].clone());
                    i += 2;
                }
                x if x.starts_with("--const=") => {
                    const_filter = Some(x[8..].to_owned());
                    i += 1;
                }
                "--project" if i + 1 < args.len() => {
                    project = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                }
                x if x.starts_with("--project=") => {
                    project = Some(PathBuf::from(&x[10..]));
                    i += 1;
                }
                "--label" if i + 1 < args.len() => {
                    label = Some(args[i + 1].clone());
                    i += 2;
                }
                x if x.starts_with("--label=") => {
                    label = Some(x[8..].to_owned());
                    i += 1;
                }
                "--format" if i + 1 < args.len() => {
                    format = args[i + 1].clone();
                    i += 2;
                }
                x if x.starts_with("--format=") => {
                    format = x[9..].to_owned();
                    i += 1;
                }
                x if !x.starts_with('-') => {
                    paths.push(PathBuf::from(x));
                    i += 1;
                }
                _ => return Err(CallgraphError::Usage(show_usage().into())),
            }
        }
        if paths.is_empty() {
            // réteg-kapcsolóhoz (`--layer`, `--layers`, `--by-layer`) út nélkül a projekt gyökere (mint a `profile`-nál)
            if !layers.any() {
                return Err(CallgraphError::Usage(show_usage().into()));
            }
            paths.push(project.clone().unwrap_or_else(|| PathBuf::from(".")));
        }
        Ok(Self::Show {
            paths,
            project,
            label,
            format,
            full,
            limit,
            lines,
            strings,
            const_filter,
            body,
            body_mode,
            refs,
            grep,
            category,
            depth,
            guess,
            notes,
            layers,
        })
    }

    /// `note <add|list|rm> …`; a `--project` bárhol állhat, `--` után minden szöveg (pl. `-`-sal kezdődő jegyzet).
    fn parse_note(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(note_usage().into());
        let mut project = None;
        let mut positional: Vec<String> = Vec::new();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--" => {
                    positional.extend(args[i + 1..].iter().cloned());
                    break;
                }
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                x if x.starts_with('-') && x.len() > 1 => return Err(usage()),
                x => positional.push(x.to_owned()),
            }
            i += 1;
        }
        let Some((action, rest)) = positional.split_first() else {
            return Err(usage());
        };
        let action = match (action.as_str(), rest) {
            ("add", [target, words @ ..]) if !words.is_empty() => {
                let text = words.join(" ");
                if text.trim().is_empty() {
                    return Err(usage());
                }
                NoteAction::Add {
                    target: target.clone(),
                    text,
                }
            }
            ("list" | "ls", []) => NoteAction::List { path: None },
            ("list" | "ls", [path]) => NoteAction::List {
                path: Some(path.clone()),
            },
            ("rm" | "remove", [id]) => NoteAction::Rm { id: id.clone() },
            _ => return Err(usage()),
        };
        Ok(Self::Note { action, project })
    }

    /// `refresh [--project <alias|út>] [--background] [<út>…]`; `--` után minden út.
    fn parse_refresh(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(refresh_usage().into());
        let mut paths = Vec::new();
        let mut project = None;
        let mut background = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--" => {
                    paths.extend(args[i + 1..].iter().map(PathBuf::from));
                    break;
                }
                "--background" => background = true,
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                x if x.starts_with('-') && x.len() > 1 => return Err(usage()),
                x => paths.push(PathBuf::from(x)),
            }
            i += 1;
        }
        if project.as_deref().is_some_and(str::is_empty) {
            return Err(usage());
        }
        Ok(Self::Refresh {
            paths,
            project,
            background,
        })
    }

    /// `locate <kifejezés>… [--all] [--project <alias|út>]`; `--` után minden kifejezés (a `-`-sel kezdődők is);
    /// egy ismeretlen `--…` argumentum is kifejezés (CSS-változó), az egy kötőjeles ismeretlen kapcsoló hiba.
    /// `flow map [--project <alias|путь>] [--json] [--table <имя>] [--full]`.
    fn parse_flow(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(flow_usage().into());
        let mut project = None;
        let mut json = false;
        let mut table = None;
        let mut full = false;
        let mut i = 0;
        // необязательное слово `map`
        if args.first().is_some_and(|a| a == "map") {
            i = 1;
        }
        while i < args.len() {
            match args[i].as_str() {
                "--json" => json = true,
                "--full" => full = true,
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                "--table" if i + 1 < args.len() => {
                    table = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--table=") => table = Some(x[8..].to_owned()),
                _ => return Err(usage()),
            }
            i += 1;
        }
        if project.as_deref().is_some_and(str::is_empty) {
            return Err(usage());
        }
        Ok(Self::Flow {
            project,
            json,
            table,
            full,
        })
    }

    /// `trace <term>... [--project <alias|путь>] [--json] [--full]`.
    fn autoload_usage() -> &'static str {
        crate::t!(
            "callgraph autoload <Class>... [--project <alias|dir>] [--json]  (resolve a class via composer autoload: psr-4 / psr-0 / classmap, own code and vendor)",
            "callgraph autoload <Osztály>... [--project <alias|mappa>] [--json]  (osztály feloldása composer-autoload-dal: psr-4 / psr-0 / classmap, saját kód és vendor)"
        )
    }

    fn parse_autoload(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(Self::autoload_usage().into());
        let mut classes = Vec::new();
        let mut project = None;
        let mut json = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--" => {
                    classes.extend(args[i + 1..].iter().cloned());
                    break;
                }
                "--json" => json = true,
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                x if x.starts_with('-') && x.len() > 1 => return Err(usage()),
                x => classes.push(x.to_owned()),
            }
            i += 1;
        }
        if classes.is_empty() || project.as_deref().is_some_and(str::is_empty) {
            return Err(usage());
        }
        Ok(Self::Autoload {
            classes,
            project,
            json,
        })
    }

    fn refs_usage() -> &'static str {
        crate::t!(
            "callgraph refs <rN> <k> [--project <alias|dir>]  (expand footnote #k of result rN)",
            "callgraph refs <rN> <k> [--project <alias|mappa>]  (az rN eredmény #k lábjegyzetének kifejtése)"
        )
    }

    fn parse_refs(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(Self::refs_usage().into());
        let mut id = None;
        let mut key = None;
        let mut project = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 2;
                }
                x if x.starts_with("--project=") => {
                    project = Some(x[10..].to_owned());
                    i += 1;
                }
                x if x.starts_with('-') && x.len() > 1 => return Err(usage()),
                x if id.is_none() => {
                    id = Some(x.to_owned());
                    i += 1;
                }
                x if key.is_none() => {
                    key = x.trim_start_matches('#').parse().ok();
                    i += 1;
                }
                _ => return Err(usage()),
            }
        }
        match (id, key) {
            (Some(id), Some(key)) => Ok(Self::Refs { id, key, project }),
            _ => Err(usage()),
        }
    }

    fn parse_trace(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(trace_usage().into());
        let mut terms = Vec::new();
        let mut project = None;
        let mut json = false;
        let mut full = false;
        let mut must = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--" => {
                    terms.extend(args[i + 1..].iter().cloned());
                    break;
                }
                "--json" => json = true,
                "--full" => full = true,
                "--must" => must = true,
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                x if x.starts_with('-') && x.len() > 1 => return Err(usage()),
                x => terms.push(x.to_owned()),
            }
            i += 1;
        }
        if terms.is_empty() || project.as_deref().is_some_and(str::is_empty) {
            return Err(usage());
        }
        Ok(Self::Trace {
            terms,
            project,
            json,
            full,
            must,
        })
    }

    fn parse_locate(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(crate::locate::usage().into());
        let mut terms = Vec::new();
        let mut project = None;
        let mut all = false;
        let mut files = false;
        let mut json = false;
        let mut exact = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--" => {
                    terms.extend(args[i + 1..].iter().cloned());
                    break;
                }
                "--all" => all = true,
                "--files" => files = true,
                "--json" => json = true,
                "--exact" => exact = true,
                "--project" if i + 1 < args.len() => {
                    project = Some(args[i + 1].clone());
                    i += 1;
                }
                "--project" => return Err(usage()),
                x if x.starts_with("--project=") => project = Some(x[10..].to_owned()),
                // egy ismeretlen `--név` kifejezés (CSS custom property: `--orange-500`), nem kapcsoló
                x if x.starts_with("--") && x.len() > 2 => terms.push(x.to_owned()),
                x if x.starts_with('-') && x.len() > 1 => return Err(usage()),
                x => terms.push(x.to_owned()),
            }
            i += 1;
        }
        if terms.iter().all(|t| t.trim().is_empty())
            || project.as_deref().is_some_and(str::is_empty)
        {
            return Err(usage());
        }
        Ok(Self::Locate {
            terms,
            all,
            files,
            json,
            exact,
            project,
        })
    }

    /// `hook post-edit|install|uninstall|status [--settings <fájl>]`.
    fn parse_hook(args: &[String]) -> Result<Self> {
        let usage = || CallgraphError::Usage(hook_usage().into());
        let mut action = None;
        let mut settings = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--settings" if i + 1 < args.len() => {
                    settings = Some(PathBuf::from(&args[i + 1]));
                    i += 1;
                }
                x if x.starts_with("--settings=") => settings = Some(PathBuf::from(&x[11..])),
                "never" if action.is_none() && i + 1 < args.len() => {
                    action = Some(HookAction::Never(args[i + 1].clone()));
                    i += 1;
                }
                "reset" if action.is_none() => {
                    let item = args.get(i + 1).filter(|a| !a.starts_with("--")).cloned();
                    if item.is_some() {
                        i += 1;
                    }
                    action = Some(HookAction::Reset(item));
                }
                x if action.is_none() => {
                    action = Some(match x {
                        "post-edit" => HookAction::PostEdit,
                        "install" => HookAction::Install,
                        "uninstall" | "remove" => HookAction::Uninstall,
                        "status" => HookAction::Status,
                        "session-start" => HookAction::SessionStart,
                        "install-session" => HookAction::InstallSession,
                        _ => return Err(usage()),
                    });
                }
                _ => return Err(usage()),
            }
            i += 1;
        }
        let action = action.ok_or_else(usage)?;
        Ok(Self::Hook { action, settings })
    }

    fn parse_callers(args: &[String]) -> Result<Self> {
        let Some(pattern) = args.first().filter(|value| !value.starts_with('-')) else {
            return Err(CallgraphError::Usage(callers_usage().into()));
        };
        let mut depth = 1;
        let mut project = None;
        let mut by_layer = false;
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--by-layer" => {
                    by_layer = true;
                    i += 1;
                }
                "--depth" if i + 1 < args.len() => {
                    depth = args[i + 1]
                        .parse()
                        .map_err(|_| CallgraphError::Usage(callers_usage().into()))?;
                    i += 2;
                }
                value if value.starts_with("--depth=") => {
                    depth = value[8..]
                        .parse()
                        .map_err(|_| CallgraphError::Usage(callers_usage().into()))?;
                    i += 1;
                }
                "--project" if i + 1 < args.len() => {
                    project = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                }
                value if value.starts_with("--project=") => {
                    project = Some(PathBuf::from(&value[10..]));
                    i += 1;
                }
                _ => return Err(CallgraphError::Usage(callers_usage().into())),
            }
        }
        if depth == 0 {
            return Err(CallgraphError::Usage(callers_usage().into()));
        }
        Ok(Self::Callers {
            pattern: pattern.clone(),
            depth,
            project,
            by_layer,
        })
    }

    fn parse_analyze(args: Vec<String>) -> Result<Self> {
        let mut paths = Vec::new();
        let mut lang = None;
        let mut cache = None;
        let mut no_cache = false;
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            if arg == "--no-cache" {
                no_cache = true;
            } else if arg == "--lang" {
                let value = it.next().ok_or_else(|| {
                    CallgraphError::Usage(crate::tf!(
                        "--lang expects a value; {}",
                        "a --lang értéket vár; {}",
                        usage()
                    ))
                })?;
                lang = Some(parse_lang(&value)?);
            } else if let Some(value) = arg.strip_prefix("--lang=") {
                lang = Some(parse_lang(value)?);
            } else if arg == "--cache" {
                let value = it.next().ok_or_else(|| {
                    CallgraphError::Usage(crate::tf!(
                        "--cache expects a value; {}",
                        "a --cache értéket vár; {}",
                        usage()
                    ))
                })?;
                cache = Some(PathBuf::from(value));
            } else if let Some(value) = arg.strip_prefix("--cache=") {
                cache = Some(PathBuf::from(value));
            } else {
                paths.push(PathBuf::from(arg));
            }
        }
        if paths.is_empty() {
            return Err(CallgraphError::Usage(usage().into()));
        }
        Ok(Self::Analyze {
            paths,
            lang,
            cache,
            no_cache,
        })
    }

    fn parse_prune(args: &[String]) -> Result<Self> {
        let mut paths = Vec::new();
        let mut cache = None;
        let mut dry_run = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--cache" if cache.is_none() && i + 1 < args.len() => {
                    i += 1;
                    cache = Some(PathBuf::from(&args[i]));
                }
                "--dry-run" if !dry_run => dry_run = true,
                value if value.starts_with("--cache=") && cache.is_none() => {
                    cache = Some(PathBuf::from(&value[8..]));
                }
                value if !value.starts_with('-') => paths.push(PathBuf::from(value)),
                _ => return Err(CallgraphError::Usage(prune_usage().into())),
            }
            i += 1;
        }
        Ok(Self::Prune {
            paths,
            cache,
            dry_run,
        })
    }
}

fn parse_lang(value: &str) -> Result<Vec<String>> {
    let available = LanguageRegistry::with_defaults().language_ids();
    let ids: Vec<String> = value.split(',').map(str::trim).map(str::to_owned).collect();
    if ids.iter().any(String::is_empty) {
        return Err(CallgraphError::Usage(crate::tf!(
            "empty --lang id; available ids: {}; {}",
            "üres --lang id; lehetséges id-k: {}; {}",
            available.join(", "),
            usage()
        )));
    }
    if let Some(unknown) = ids
        .iter()
        .find(|id| !available.iter().any(|known| known == id))
    {
        return Err(CallgraphError::Usage(crate::tf!(
            "unknown language id: {unknown}; available ids: {}; {}",
            "ismeretlen nyelv-id: {unknown}; lehetséges id-k: {}; {}",
            available.join(", "),
            usage()
        )));
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(a: &[&str]) -> Result<CliArgs> {
        CliArgs::parse(a.iter().map(|s| s.to_string()))
    }

    #[test]
    fn requires_at_least_one_path_for_analysis() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--no-cache"]).is_err());
        assert_eq!(
            parse(&["a"]).unwrap(),
            CliArgs::Analyze {
                paths: vec![PathBuf::from("a")],
                lang: None,
                cache: None,
                no_cache: false
            }
        );
    }

    #[test]
    fn parses_cache_flags() {
        let a = parse(&["--cache", "c.jsonl", "x", "--no-cache"]).unwrap();
        assert_eq!(
            a,
            CliArgs::Analyze {
                paths: vec![PathBuf::from("x")],
                lang: None,
                cache: Some(PathBuf::from("c.jsonl")),
                no_cache: true
            }
        );
        assert!(parse(&["x", "--cache"]).is_err());
        assert!(matches!(
            parse(&["--cache=d.jsonl", "y"]).unwrap(),
            CliArgs::Analyze { cache: Some(_), .. }
        ));
    }

    #[test]
    fn parses_and_validates_language_filter() {
        assert_eq!(
            parse(&["--lang", "php, css", "src"]).unwrap(),
            CliArgs::Analyze {
                paths: vec![PathBuf::from("src")],
                lang: Some(vec!["php".into(), "css".into()]),
                cache: None,
                no_cache: false,
            }
        );
        assert!(parse(&["--lang=wat", "src"]).is_err());
        assert!(parse(&["--lang", "php,", "src"]).is_err());
    }

    #[test]
    fn parses_prune() {
        assert_eq!(
            parse(&["prune"]).unwrap(),
            CliArgs::Prune {
                paths: vec![],
                cache: None,
                dry_run: false
            }
        );
        assert_eq!(
            parse(&["prune", "--cache", "c.jsonl", "--dry-run"]).unwrap(),
            CliArgs::Prune {
                paths: vec![],
                cache: Some(PathBuf::from("c.jsonl")),
                dry_run: true
            }
        );
        assert!(parse(&["prune", "--bogus"]).is_err());
    }

    #[test]
    fn parses_body_modes() {
        let mode = |a: &[&str]| match parse(a).unwrap() {
            CliArgs::Show {
                body, body_mode, ..
            } => (body, body_mode),
            other => panic!("{other:?}"),
        };
        assert_eq!(mode(&["show", "a.php::m"]), (false, BodyMode::Compact));
        assert_eq!(
            mode(&["show", "--body", "a.php::m"]),
            (true, BodyMode::Compact)
        );
        assert_eq!(
            mode(&["show", "--body", "--doc", "a.php::m"]),
            (true, BodyMode::Doc)
        );
        // a --doc / --raw magában is --body; a --raw erősebb
        assert_eq!(mode(&["show", "--raw", "a.php::m"]), (true, BodyMode::Raw));
        assert_eq!(
            mode(&["show", "--raw", "--doc", "a.php::m"]),
            (true, BodyMode::Raw)
        );
    }

    #[test]
    fn modern_surface_routes_to_existing_commands() {
        let show = |a: &[&str]| match parse(a).unwrap() {
            CliArgs::Show {
                body,
                refs,
                format,
                grep,
                strings,
                paths,
                ..
            } => (body, refs, format, grep, strings, paths),
            other => panic!("{other:?}"),
        };
        // файл → outline, без сносок
        assert_eq!(
            show(&["read", "a.php"]),
            (
                false,
                false,
                "outline".to_owned(),
                None,
                None,
                vec![PathBuf::from("a.php")]
            )
        );
        // file::method → тело и сноски по умолчанию
        assert_eq!(
            show(&["read", "a.php::m"]),
            (
                true,
                true,
                "outline".to_owned(),
                None,
                None,
                vec![PathBuf::from("a.php::m")]
            )
        );
        // --api отключает тело и сноски
        let (body, refs, format, ..) = show(&["read", "--api", "a.php::m"]);
        assert!(!body && !refs && format == "api");
        // find
        let (_, _, _, grep, _, paths) =
            show(&["find", "callers", "--project", "p", "m", "--depth", "2"]);
        assert_eq!(grep.as_deref(), Some("m"));
        assert_eq!(paths, vec![PathBuf::from(".")]);
        assert_eq!(show(&["find", "string", "text"]).4.as_deref(), Some("text"));
        // map / flow
        assert!(matches!(
            parse(&["map", "HotelOnlineService"]).unwrap(),
            CliArgs::Locate { terms, .. } if terms == ["HotelOnlineService"]
        ));
        assert!(matches!(
            parse(&["flow", "hotel_name"]).unwrap(),
            CliArgs::Trace { terms, .. } if terms == ["hotel_name"]
        ));
    }

    #[test]
    fn layer_switches_without_a_path_run_on_the_project_root() {
        let paths = |a: &[&str]| match parse(a).unwrap() {
            CliArgs::Show { paths, .. } => paths,
            other => panic!("{other:?}"),
        };
        let dot = vec![PathBuf::from(".")];
        assert_eq!(paths(&["show", "--layer", "service"]), dot);
        assert_eq!(paths(&["show", "--layers"]), dot);
        assert_eq!(paths(&["show", "--by-layer", "--grep", "x"]), dot);
        assert_eq!(
            paths(&["show", "--project", "/p", "--layer", "service"]),
            vec![PathBuf::from("/p")]
        );
        // útvonallal változatlan
        assert_eq!(
            paths(&["show", "--layers", "src"]),
            vec![PathBuf::from("src")]
        );
        // réteg-kapcsoló nélkül továbbra is használati hiba
        assert!(parse(&["show"]).is_err());
        assert!(parse(&["show", "--project", "/p"]).is_err());
        assert!(parse(&["show", "--grep", "x"]).is_err());
    }

    #[test]
    fn parses_note_subcommands() {
        assert_eq!(
            parse(&[
                "note",
                "add",
                "a.rs::run",
                "slow",
                "path",
                "--project",
                "sp"
            ])
            .unwrap(),
            CliArgs::Note {
                action: NoteAction::Add {
                    target: "a.rs::run".into(),
                    text: "slow path".into()
                },
                project: Some("sp".into())
            }
        );
        assert_eq!(
            parse(&["note", "add", "a.rs", "--", "-1 is a sentinel"]).unwrap(),
            CliArgs::Note {
                action: NoteAction::Add {
                    target: "a.rs".into(),
                    text: "-1 is a sentinel".into()
                },
                project: None
            }
        );
        assert_eq!(
            parse(&["note", "list", "--project=/x", "src"]).unwrap(),
            CliArgs::Note {
                action: NoteAction::List {
                    path: Some("src".into())
                },
                project: Some("/x".into())
            }
        );
        assert_eq!(
            parse(&["note", "rm", "ab12cd34"]).unwrap(),
            CliArgs::Note {
                action: NoteAction::Rm {
                    id: "ab12cd34".into()
                },
                project: None
            }
        );
        assert!(parse(&["note"]).is_err());
        assert!(parse(&["note", "add", "a.rs"]).is_err());
        assert!(parse(&["note", "rm"]).is_err());
        assert!(parse(&["note", "list", "--bogus"]).is_err());
        assert!(matches!(
            parse(&["show", "src", "--notes", "cache"]).unwrap(),
            CliArgs::Show { notes: Some(n), .. } if n == "cache"
        ));
    }

    #[test]
    fn parses_refresh() {
        assert_eq!(
            parse(&["refresh"]).unwrap(),
            CliArgs::Refresh {
                paths: vec![],
                project: None,
                background: false
            }
        );
        assert_eq!(
            parse(&["refresh", "--background", "--project", "sp", "a.php", "src"]).unwrap(),
            CliArgs::Refresh {
                paths: vec![PathBuf::from("a.php"), PathBuf::from("src")],
                project: Some("sp".into()),
                background: true
            }
        );
        assert_eq!(
            parse(&["refresh", "--project=/x", "--", "-odd.php"]).unwrap(),
            CliArgs::Refresh {
                paths: vec![PathBuf::from("-odd.php")],
                project: Some("/x".into()),
                background: false
            }
        );
        assert!(parse(&["refresh", "--bogus"]).is_err());
        assert!(parse(&["refresh", "--project="]).is_err());
    }

    #[test]
    fn parses_locate() {
        assert_eq!(
            parse(&[
                "locate",
                "RefundController",
                "--all",
                "--project",
                "sp",
                "refund"
            ])
            .unwrap(),
            CliArgs::Locate {
                terms: vec!["RefundController".into(), "refund".into()],
                all: true,
                files: false,
                json: false,
                exact: false,
                project: Some("sp".into()),
            }
        );
        assert_eq!(
            parse(&["locate", "--project=/x", "--", "-odd"]).unwrap(),
            CliArgs::Locate {
                terms: vec!["-odd".into()],
                all: false,
                files: false,
                json: false,
                exact: false,
                project: Some("/x".into()),
            }
        );
        assert!(parse(&["locate"]).is_err());
        assert!(parse(&["locate", "--all"]).is_err());
        // egy ismeretlen `--név` kifejezés (CSS custom property), az ismert kapcsolók kapcsolók maradnak
        assert_eq!(
            parse(&["locate", "--orange-500", "--all", "x"]).unwrap(),
            CliArgs::Locate {
                terms: vec!["--orange-500".into(), "x".into()],
                all: true,
                files: false,
                json: false,
                exact: false,
                project: None,
            }
        );
        assert!(parse(&["locate", "x", "-b"]).is_err());
        assert!(parse(&["locate", "x", "--project"]).is_err());
        assert!(parse(&["locate", "x", "--project="]).is_err());
    }

    #[test]
    fn no_gitignore_is_taken_anywhere_before_the_separator() {
        let mut args: Vec<String> = ["show", "--no-gitignore", "src", "--no-gitignore"]
            .map(String::from)
            .to_vec();
        assert!(take_no_gitignore(&mut args));
        assert_eq!(args, ["show", "src"]);
        let mut args: Vec<String> = ["locate", "--", "--no-gitignore"]
            .map(String::from)
            .to_vec();
        assert!(!take_no_gitignore(&mut args));
        assert_eq!(args.len(), 3);
        let mut args: Vec<String> = ["--no-gitignore", "locate", "x", "--", "--no-gitignore"]
            .map(String::from)
            .to_vec();
        assert!(take_no_gitignore(&mut args));
        assert_eq!(args, ["locate", "x", "--", "--no-gitignore"]);
    }

    #[test]
    fn parses_hook() {
        assert_eq!(
            parse(&["hook", "post-edit"]).unwrap(),
            CliArgs::Hook {
                action: HookAction::PostEdit,
                settings: None
            }
        );
        assert_eq!(
            parse(&["hook", "install", "--settings", "/s.json"]).unwrap(),
            CliArgs::Hook {
                action: HookAction::Install,
                settings: Some(PathBuf::from("/s.json"))
            }
        );
        assert_eq!(
            parse(&["hook", "--settings=/s.json", "uninstall"]).unwrap(),
            CliArgs::Hook {
                action: HookAction::Uninstall,
                settings: Some(PathBuf::from("/s.json"))
            }
        );
        assert!(parse(&["hook"]).is_err());
        assert!(parse(&["hook", "bogus"]).is_err());
        assert!(parse(&["hook", "status", "install"]).is_err());
    }

    #[test]
    fn parses_version_and_self_update() {
        assert_eq!(parse(&["--version"]).unwrap(), CliArgs::Version);
        assert_eq!(parse(&["-V"]).unwrap(), CliArgs::Version);
        assert_eq!(parse(&["version"]).unwrap(), CliArgs::Version);
        assert_eq!(parse(&["--help"]).unwrap(), CliArgs::Help);
        assert_eq!(parse(&["-h"]).unwrap(), CliArgs::Help);
        assert_eq!(parse(&["help"]).unwrap(), CliArgs::Help);
        // több argumentummal a `version` egy elemzendő út (mint eddig)
        assert!(matches!(
            parse(&["version", "x"]).unwrap(),
            CliArgs::Analyze { .. }
        ));
        assert_eq!(
            parse(&["mcp"]).unwrap(),
            CliArgs::Mcp { auto_update: false }
        );
        assert_eq!(
            parse(&["mcp", "--auto-update"]).unwrap(),
            CliArgs::Mcp { auto_update: true }
        );
        assert!(parse(&["mcp", "x"]).is_err());
        assert!(parse(&["mcp", "--auto-update", "x"]).is_err());
        assert!(parse(&["mcp", "x", "--auto-update"]).is_err());
        assert_eq!(
            parse(&["self-update", "--check"]).unwrap(),
            CliArgs::Skill {
                command: "self-update".into(),
                args: vec!["--check".into()]
            }
        );
    }
}

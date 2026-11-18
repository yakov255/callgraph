//! Резолв Composer-автозагрузки: namespace → файл, без конфигурации.
//!
//! В проекте может быть несколько `composer.json` (монорепа, подпроекты). Модуль читает все
//! (кроме каталогов `vendor`, `node_modules`, `.git`, `.callgraph`, `target`) и к собственным
//! правилам `autoload` / `autoload-dev` psr-4 / psr-0 добавляет правила vendor-пакетов:
//!
//! - `vendor/composer/installed.json` (Composer 2; `install-path` + `autoload`), или
//! - если его нет — `composer.json` пакетов из `require` / `require-dev` в `vendor/<имя>`;
//! - `vendor/composer/autoload_classmap.php` для классов из classmap (так устроен core).
//!
//! Каталог `vendor` берётся из `config.vendor-dir` соответствующего `composer.json` (по умолчанию
//! `vendor`), поэтому у разных корней могут быть разные vendor. При резолве побеждает самый
//! длинный совпавший префикс; при равенстве — ближайший к вызывающему файлу корень, затем свой
//! код перед vendor.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Каталоги, в которых не ищем `composer.json`.
const SKIP_DIRS: &[&str] = &[
    "vendor",
    "node_modules",
    ".git",
    ".callgraph",
    ".idea",
    ".vscode",
    "target",
];

/// Предохранители обхода дерева.
const MAX_COMPOSERS: usize = 4096;
const MAX_DEPTH: usize = 16;

/// Какое правило автозагрузки разрешило имя.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoloadKind {
    /// `autoload.psr-4` (или psr-4 vendor-пакета).
    Psr4,
    /// `autoload.psr-0`.
    Psr0,
    /// `vendor/composer/autoload_classmap.php`.
    Classmap,
}

impl AutoloadKind {
    /// Короткая метка для вывода.
    pub fn label(self) -> &'static str {
        match self {
            Self::Psr4 => "psr-4",
            Self::Psr0 => "psr-0",
            Self::Classmap => "classmap",
        }
    }
}

/// Источник правила: свой код проекта или vendor-пакет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassSource {
    /// Собственный namespace из `autoload` / `autoload-dev`.
    Own,
    /// Vendor-пакет (`acme/lib`).
    Vendor(String),
}

/// Корень `composer.json` и соответствующий ему каталог vendor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Root {
    dir: PathBuf,
    vendor: PathBuf,
}

/// Правило psr-4 / psr-0.
#[derive(Debug, Clone)]
struct Rule {
    prefix: String,
    dirs: Vec<PathBuf>,
    root: usize,
    kind: AutoloadKind,
    dev: bool,
    source: ClassSource,
}

/// Разрешённый класс.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedClass {
    /// Абсолютный путь к файлу исходника.
    pub path: PathBuf,
    /// Каталог `composer.json` (корень проекта).
    pub root: PathBuf,
    /// Каталог vendor этого корня.
    pub vendor: PathBuf,
    /// Какое правило разрешило имя.
    pub kind: AutoloadKind,
    /// Свой код или vendor-пакет.
    pub source: ClassSource,
}

/// Все правила composer-автозагрузки проекта.
#[derive(Debug, Default)]
pub struct AutoloadMap {
    roots: Vec<Root>,
    rules: Vec<Rule>,
    classmap: HashMap<String, Vec<(PathBuf, usize)>>,
}

impl AutoloadMap {
    /// Читает все `composer.json` проекта (и autoload соответствующих vendor).
    pub fn discover(project: &Path) -> Self {
        let mut map = Self::default();
        for composer in composer_files(project) {
            map.add_composer(&composer, project);
        }
        map
    }

    /// Прочитанные composer-корни (корень проекта первым).
    pub fn roots(&self) -> impl Iterator<Item = (&Path, &Path)> {
        self.roots
            .iter()
            .map(|root| (root.dir.as_path(), root.vendor.as_path()))
    }

    /// Разрешает полное имя класса (`Leader\Class`, `\Leader\Class`).
    ///
    /// `from`: ссылающийся файл (необязательно) — при одинаковом префиксе побеждает ближайший
    /// к нему composer-корень (монорепа, где два подпроекта используют один namespace).
    pub fn resolve(&self, class: &str, from: Option<&Path>) -> Option<ResolvedClass> {
        let class = class.trim().trim_start_matches('\\');
        if class.is_empty() {
            return None;
        }
        if let Some(hit) = self.resolve_classmap(class, from) {
            return Some(hit);
        }
        if let Some(hit) = self.resolve_rules(class, from, AutoloadKind::Psr4) {
            return Some(hit);
        }
        self.resolve_rules(class, from, AutoloadKind::Psr0)
    }

    /// Точное совпадение из classmap (`vendor/composer/autoload_classmap.php`).
    fn resolve_classmap(&self, class: &str, from: Option<&Path>) -> Option<ResolvedClass> {
        let candidates = self.classmap.get(class)?;
        let mut best: Option<(&PathBuf, usize)> = None;
        for (path, root) in candidates {
            if !path.is_file() {
                continue;
            }
            if best.is_none_or(|(_, current)| {
                proximity(*root, from, self) > proximity(current, from, self)
            }) {
                best = Some((path, *root));
            }
        }
        let (path, root) = best?;
        let root = &self.roots[root];
        Some(ResolvedClass {
            path: path.clone(),
            root: root.dir.clone(),
            vendor: root.vendor.clone(),
            kind: AutoloadKind::Classmap,
            source: source_of(path, &root.vendor),
        })
    }

    /// Правило psr-4 / psr-0 с самым длинным совпавшим префиксом.
    fn resolve_rules(
        &self,
        class: &str,
        from: Option<&Path>,
        kind: AutoloadKind,
    ) -> Option<ResolvedClass> {
        let mut candidates: Vec<(&Rule, PathBuf)> = Vec::new();
        for rule in self.rules.iter().filter(|rule| rule.kind == kind) {
            let Some(suffix) = class.strip_prefix(rule.prefix.as_str()) else {
                continue;
            };
            let relative = match kind {
                AutoloadKind::Psr4 => format!("{}.php", suffix.replace('\\', "/")),
                _ => format!("{}.php", psr0_path(suffix)),
            };
            for dir in &rule.dirs {
                let path = dir.join(&relative);
                if path.is_file() {
                    candidates.push((rule, path));
                    break;
                }
            }
        }
        let (rule, path) = candidates
            .into_iter()
            .max_by(|(a, _), (b, _)| compare(a, b, from, self))?;
        let root = &self.roots[rule.root];
        Some(ResolvedClass {
            path,
            root: root.dir.clone(),
            vendor: root.vendor.clone(),
            kind,
            source: rule.source.clone(),
        })
    }

    /// Обработка одного корня: свой psr-4 / psr-0, vendor-пакеты и classmap.
    fn add_composer(&mut self, composer: &Path, project: &Path) {
        if self.roots.len() >= MAX_COMPOSERS {
            return;
        }
        let Ok(text) = std::fs::read_to_string(composer) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if !value.is_object() {
            return;
        }
        let dir = composer.parent().unwrap_or(project).to_path_buf();
        let vendor = vendor_dir(&dir, &value);
        let index = self.roots.len();
        self.roots.push(Root {
            dir: dir.clone(),
            vendor: vendor.clone(),
        });

        self.add_own(&value, index, &dir);
        self.add_vendor(&value, index, &vendor);
        self.add_classmap(&vendor);
    }

    /// Секции `autoload` / `autoload-dev` из `composer.json`.
    fn add_own(&mut self, value: &Value, root: usize, dir: &Path) {
        for (section, dev) in [("autoload", false), ("autoload-dev", true)] {
            let Some(autoload) = value.get(section) else {
                continue;
            };
            for (kind, key) in [(AutoloadKind::Psr4, "psr-4"), (AutoloadKind::Psr0, "psr-0")] {
                let Some(map) = autoload.get(key).and_then(Value::as_object) else {
                    continue;
                };
                for (prefix, paths) in map {
                    let dirs = prefixed_paths(dir, paths);
                    if !dirs.is_empty() {
                        self.rules.push(Rule {
                            prefix: prefix.clone(),
                            dirs,
                            root,
                            kind,
                            dev,
                            source: ClassSource::Own,
                        });
                    }
                }
            }
        }
    }

    /// Правила vendor-пакетов: `installed.json`, при его отсутствии — манифесты пакетов.
    fn add_vendor(&mut self, value: &Value, root: usize, vendor: &Path) {
        let mut seen = Vec::new();
        if let Some(packages) = installed_packages(&vendor.join("composer").join("installed.json"))
        {
            for package in packages {
                let Some(name) = package.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let Some(base) = package
                    .get("install-path")
                    .and_then(Value::as_str)
                    .map(|path| normalize(&vendor.join("composer").join(path)))
                    .filter(|path| path.is_dir())
                else {
                    continue;
                };
                self.add_package(&package, name, root, &base);
                seen.push(name.to_owned());
            }
        }
        for key in ["require", "require-dev"] {
            let Some(deps) = value.get(key).and_then(Value::as_object) else {
                continue;
            };
            for name in deps.keys() {
                if !name.contains('/') || seen.iter().any(|seen| seen == name) {
                    continue;
                }
                let base = vendor.join(name);
                let manifest = base.join("composer.json");
                let Ok(text) = std::fs::read_to_string(&manifest) else {
                    continue;
                };
                let Ok(package) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                self.add_package(&package, name, root, &base);
            }
        }
    }

    /// `autoload` одного vendor-пакета.
    fn add_package(&mut self, package: &Value, name: &str, root: usize, base: &Path) {
        let Some(autoload) = package.get("autoload") else {
            return;
        };
        for (kind, key) in [(AutoloadKind::Psr4, "psr-4"), (AutoloadKind::Psr0, "psr-0")] {
            let Some(map) = autoload.get(key).and_then(Value::as_object) else {
                continue;
            };
            for (prefix, paths) in map {
                let dirs = prefixed_paths(base, paths);
                if !dirs.is_empty() {
                    self.rules.push(Rule {
                        prefix: prefix.clone(),
                        dirs,
                        root,
                        kind,
                        dev: false,
                        source: ClassSource::Vendor(name.to_owned()),
                    });
                }
            }
        }
    }

    /// Чтение `vendor/composer/autoload_classmap.php`.
    fn add_classmap(&mut self, vendor: &Path) {
        let path = vendor.join("composer").join("autoload_classmap.php");
        // classmap может содержать не-UTF-8 имена файлов, поэтому читаем байты с потерей.
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        let text = String::from_utf8_lossy(&bytes);
        let root = self.roots.len().saturating_sub(1);
        let base = vendor.parent().unwrap_or(vendor);
        for (class, file) in classmap_entries(&text, vendor, base) {
            let entry = self.classmap.entry(class).or_default();
            if !entry.iter().any(|(have, _)| *have == file) {
                entry.push((file, root));
            }
        }
    }
}

/// Ближайший к вызывающему файлу корень: чем глубже корень (и он предок файла), тем лучше.
fn proximity(root: usize, from: Option<&Path>, map: &AutoloadMap) -> usize {
    let Some(from) = from else {
        return 0;
    };
    let dir = &map.roots[root].dir;
    if from.starts_with(dir) {
        dir.components().count()
    } else {
        0
    }
}

/// Порядок правил: длиннее префикс, свой код, не dev, ближе корень, мельче корень.
fn compare(a: &Rule, b: &Rule, from: Option<&Path>, map: &AutoloadMap) -> Ordering {
    a.prefix
        .len()
        .cmp(&b.prefix.len())
        .then_with(|| own(a).cmp(&own(b)))
        .then_with(|| b.dev.cmp(&a.dev))
        .then_with(|| proximity(a.root, from, map).cmp(&proximity(b.root, from, map)))
        .then_with(|| b.root.cmp(&a.root))
        .then_with(|| a.root.cmp(&b.root).reverse())
        .then_with(|| a.prefix.cmp(&b.prefix))
}

/// Имя пакета для файла внутри vendor (первые два сегмента после `vendor`).
fn source_of(path: &Path, vendor: &Path) -> ClassSource {
    let Ok(relative) = path.strip_prefix(vendor) else {
        return ClassSource::Own;
    };
    let mut parts = relative
        .components()
        .filter_map(|part| part.as_os_str().to_str());
    match (parts.next(), parts.next()) {
        (Some(vendor), Some(package)) => ClassSource::Vendor(format!("{vendor}/{package}")),
        _ => ClassSource::Own,
    }
}

fn own(rule: &Rule) -> bool {
    matches!(rule.source, ClassSource::Own)
}

/// Путь psr-0: разделители namespace и подчёркивания в имени класса становятся каталогами.
fn psr0_path(suffix: &str) -> String {
    suffix.replace(['\\', '_'], "/")
}

/// Абсолютные каталоги из `paths` (строка или список); только существующие.
fn prefixed_paths(base: &Path, paths: &Value) -> Vec<PathBuf> {
    let list: Vec<&Value> = match paths {
        Value::String(_) => vec![paths],
        Value::Array(items) => items.iter().collect(),
        _ => return Vec::new(),
    };
    list.into_iter()
        .filter_map(Value::as_str)
        .map(|path| normalize(&base.join(path)))
        .filter(|path| path.is_dir())
        .collect()
}

/// `config.vendor-dir` (абсолютный или относительно корня composer); по умолчанию `vendor`.
fn vendor_dir(dir: &Path, value: &Value) -> PathBuf {
    let configured = value
        .get("config")
        .and_then(|config| config.get("vendor-dir"))
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    match configured {
        Some(path) if Path::new(path).is_absolute() => normalize(Path::new(path)),
        Some(path) => normalize(&dir.join(path)),
        None => dir.join("vendor"),
    }
}

/// Список пакетов из `<vendor>/composer/installed.json` (Composer 2 — объект, Composer 1 — массив).
fn installed_packages(path: &Path) -> Option<Vec<Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    match value {
        Value::Array(items) => Some(items),
        Value::Object(map) => map
            .get("packages")
            .and_then(Value::as_array)
            .cloned()
            .or_else(|| Some(Vec::new())),
        _ => None,
    }
}

/// Записи сгенерированного classmap: `'Name' => $vendorDir . '/…'` или `$baseDir . '/…'`.
fn classmap_entries(text: &str, vendor: &Path, base: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('\'') else {
            continue;
        };
        let Some((class, right)) = rest.split_once("' => ") else {
            continue;
        };
        let Some(file) = relative_literal(right) else {
            continue;
        };
        let dir = if right.starts_with("$vendorDir") {
            vendor
        } else if right.starts_with("$baseDir") {
            base
        } else {
            continue;
        };
        let file = file.trim_start_matches('/');
        out.push((
            class.replace("\\\\", "\\").replace("\\'", "'"),
            dir.join(file),
        ));
    }
    out
}

/// Последний литерал `'/…'` справа (без кавычек).
fn relative_literal(right: &str) -> Option<&str> {
    let start = right.find('\'')? + 1;
    let end = right.rfind('\'')?;
    (end > start).then(|| &right[start..end])
}

/// Абсолютный путь без `..` / `.` (символические ссылки не разворачивает).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Файлы `composer.json` проекта (детерминированный порядок, корень первым).
fn composer_files(root: &Path) -> Vec<PathBuf> {
    // Обход в ширину: при достижении лимита остаются более близкие к корню (реальные) проекты,
    // а не глубокие фикстуры.
    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), 0usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        if out.len() >= MAX_COMPOSERS || depth > MAX_DEPTH {
            continue;
        }
        let composer = dir.join("composer.json");
        if composer.is_file() {
            out.push(composer);
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.is_dir()
                    && !path.is_symlink()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| !SKIP_DIRS.contains(&name) && !name.starts_with('.'))
            })
            .collect();
        children.sort();
        queue.extend(children.into_iter().map(|child| (child, depth + 1)));
    }
    out
}

#[cfg(test)]
mod tests;

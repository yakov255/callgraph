//! Тесты автозагрузки composer: несколько `composer.json`, `vendor-dir`, classmap, psr-0.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{AutoloadKind, AutoloadMap, ClassSource};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Временный проект: файлы пишутся на диск, каталог удаляется в `Drop`.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "callgraph-autoload-{name}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn file(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }

    fn dir(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn composer(psr4: &[(&str, &str)]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = psr4
        .iter()
        .map(|(prefix, dir)| ((*prefix).to_owned(), serde_json::Value::from(*dir)))
        .collect();
    serde_json::json!({"autoload": {"psr-4": map}}).to_string()
}

fn resolved(map: &AutoloadMap, class: &str, from: Option<&Path>) -> PathBuf {
    map.resolve(class, from)
        .unwrap_or_else(|| panic!("class {class} not resolved"))
        .path
}

#[test]
fn psr4_resolves_own_class() {
    let project = Fixture::new("psr4");
    project.file("composer.json", &composer(&[("App\\", "src/")]));
    project.file("src/Foo/Bar.php", "<?php");

    let map = AutoloadMap::discover(&project.root);
    let hit = map.resolve("App\\Foo\\Bar", None).unwrap();

    assert_eq!(hit.path, project.path("src/Foo/Bar.php"));
    assert_eq!(hit.root, project.root);
    assert_eq!(hit.kind, AutoloadKind::Psr4);
    assert_eq!(hit.source, ClassSource::Own);
    // ведущий `\` не мешает
    assert_eq!(
        resolved(&map, "\\App\\Foo\\Bar", None),
        project.path("src/Foo/Bar.php")
    );
}

#[test]
fn longest_prefix_wins() {
    let project = Fixture::new("prefix");
    project.file(
        "composer.json",
        &composer(&[("App\\", "src/"), ("App\\Admin\\", "admin/")]),
    );
    project.file("src/Admin/Panel.php", "<?php");
    project.file("admin/Panel.php", "<?php");

    let map = AutoloadMap::discover(&project.root);
    assert_eq!(
        resolved(&map, "App\\Admin\\Panel", None),
        project.path("admin/Panel.php")
    );
}

#[test]
fn nearest_composer_root_wins_for_same_prefix() {
    let project = Fixture::new("nearest");
    project.file("composer.json", &composer(&[("App\\", "src/")]));
    project.file("src/Foo.php", "<?php");
    project.file("app/composer.json", &composer(&[("App\\", "src/")]));
    project.file("app/src/Foo.php", "<?php");
    let from = project.file("app/src/Deep/UseFoo.php", "<?php");

    let map = AutoloadMap::discover(&project.root);
    let hit = map.resolve("App\\Foo", Some(&from)).unwrap();

    assert_eq!(hit.path, project.path("app/src/Foo.php"));
    assert_eq!(hit.root, project.path("app"));
}

#[test]
fn custom_vendor_dir_and_installed_json() {
    let project = Fixture::new("vendor");
    project.file(
        "composer.json",
        "{\"config\": {\"vendor-dir\": \"vendor-alt\"}, \"autoload\": {\"psr-4\": {\"App\\\\\": \"src/\"}}}",
    );
    project.dir("src");
    project.file(
        "vendor-alt/composer/installed.json",
        r#"{
            "packages": [
                {
                    "name": "acme/lib",
                    "install-path": "../acme/lib",
                    "autoload": {"psr-4": {"Acme\\": "src/"}}
                }
            ]
        }"#,
    );
    project.file("vendor-alt/acme/lib/src/Tool.php", "<?php");

    let map = AutoloadMap::discover(&project.root);
    let hit = map.resolve("Acme\\Tool", None).unwrap();

    assert_eq!(hit.path, project.path("vendor-alt/acme/lib/src/Tool.php"));
    assert_eq!(hit.vendor, project.path("vendor-alt"));
    assert_eq!(hit.source, ClassSource::Vendor("acme/lib".to_owned()));
}

#[test]
fn vendor_manifest_is_a_fallback() {
    let project = Fixture::new("manifest");
    project.file(
        "composer.json",
        "{\"require\": {\"acme/lib\": \"^1.0\"}, \"autoload\": {\"psr-4\": {\"App\\\\\": \"src/\"}}}",
    );
    project.dir("src");
    project.file(
        "vendor/acme/lib/composer.json",
        "{\"name\": \"acme/lib\", \"autoload\": {\"psr-4\": {\"Acme\\\\\": \"src/\"}}}",
    );
    project.file("vendor/acme/lib/src/Tool.php", "<?php");

    let map = AutoloadMap::discover(&project.root);
    let hit = map.resolve("Acme\\Tool", None).unwrap();

    assert_eq!(hit.path, project.path("vendor/acme/lib/src/Tool.php"));
    assert_eq!(hit.source, ClassSource::Vendor("acme/lib".to_owned()));
}

#[test]
fn psr0_maps_underscores_to_directories() {
    let project = Fixture::new("psr0");
    project.file(
        "composer.json",
        "{\"autoload\": {\"psr-0\": {\"Legacy_\": \"lib/\"}}}",
    );
    project.file("lib/Foo/Bar.php", "<?php");

    let map = AutoloadMap::discover(&project.root);
    let hit = map.resolve("Legacy_Foo_Bar", None).unwrap();

    assert_eq!(hit.path, project.path("lib/Foo/Bar.php"));
    assert_eq!(hit.kind, AutoloadKind::Psr0);
}

#[test]
fn classmap_covers_own_and_vendor_classes() {
    let project = Fixture::new("classmap");
    project.file("composer.json", "{\"autoload\": {\"classmap\": [\"src\"]}}");
    project.file("src/Legacy/Thing.php", "<?php");
    project.file("vendor/acme/lib/src/Tool.php", "<?php");
    project.file(
        "vendor/composer/autoload_classmap.php",
        "<?php\n\
         $vendorDir = dirname(__DIR__);\n\
         $baseDir = dirname($vendorDir);\n\
         return array(\n\
             'App\\\\Legacy\\\\Thing' => $baseDir . '/src/Legacy/Thing.php',\n\
             'Acme\\\\Lib\\\\Tool' => $vendorDir . '/acme/lib/src/Tool.php',\n\
         );\n",
    );

    let map = AutoloadMap::discover(&project.root);
    let own = map.resolve("App\\Legacy\\Thing", None).unwrap();
    let vendor = map.resolve("Acme\\Lib\\Tool", None).unwrap();

    assert_eq!(own.path, project.path("src/Legacy/Thing.php"));
    assert_eq!(own.kind, AutoloadKind::Classmap);
    assert_eq!(own.source, ClassSource::Own);
    assert_eq!(vendor.path, project.path("vendor/acme/lib/src/Tool.php"));
    assert_eq!(vendor.source, ClassSource::Vendor("acme/lib".to_owned()));
}

#[test]
fn missing_file_is_not_resolved() {
    let project = Fixture::new("missing");
    project.file("composer.json", &composer(&[("App\\", "src/")]));
    project.dir("src");

    let map = AutoloadMap::discover(&project.root);
    assert!(map.resolve("App\\Nope", None).is_none());
}

#[test]
fn vendor_composer_json_does_not_become_a_root() {
    let project = Fixture::new("skip-vendor");
    project.file("composer.json", &composer(&[("App\\", "src/")]));
    project.file("src/Foo.php", "<?php");
    project.file(
        "vendor/acme/lib/composer.json",
        "{\"autoload\": {\"psr-4\": {\"Acme\\\\\": \"src/\"}}}",
    );

    let map = AutoloadMap::discover(&project.root);
    assert_eq!(map.roots().count(), 1);
}

#[test]
fn empty_project_has_no_rules() {
    let project = Fixture::new("empty");
    let map = AutoloadMap::discover(&project.root);

    assert_eq!(map.roots().count(), 0);
    assert!(map.resolve("App\\Foo", None).is_none());
}

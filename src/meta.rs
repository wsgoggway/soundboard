//! Category tab metadata: optional `.meta.json` files inside the clips
//! directory.
//!
//! Every category directory (and the clips directory itself, for the
//! top-level files) MAY contain a `.meta.json` file configuring how the GUI
//! presents the category tab:
//!
//! ```json
//! { "title": "Приветствия", "order": 10, "visible": true }
//! ```
//!
//! Rules (the file is hand-written by the user — be lenient):
//! - all fields are optional; missing file is NOT an error (pure defaults);
//! - unknown fields are IGNORED;
//! - known fields are STRICT about their types when present (`title`:
//!   string, `order`: integer, `visible`: boolean) — a type mismatch or
//!   broken JSON makes the whole file fall back to defaults and produces a
//!   warning string (surfaced on the «Дебаг» page by the GUI);
//! - `title` — the tab caption; default is the directory name (the root
//!   category defaults to [`ROOT_TITLE`]);
//! - `order` — sort key; default 0; tabs sort by (order, directory name);
//! - `visible` — default true; false hides the tab entirely (its clips are
//!   also excluded from the GUI search: hidden means hidden).
//!
//! The meta title is GUI-only cosmetics: hotkeys, the CLI (`list` output
//! `category/name`) and clip resolution keep using the directory name.

use std::path::Path;

use serde::Deserialize;

use crate::clips::UNCATEGORIZED;

/// Name of the per-category metadata file.
pub const META_FILE: &str = ".meta.json";

/// Default tab caption for the root category (top-level clips).
pub const ROOT_TITLE: &str = "Общий";

/// Parsed contents of one `.meta.json` (all fields optional).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CategoryMeta {
    /// Tab caption override.
    pub title: Option<String>,
    /// Tab sort key override.
    pub order: Option<i64>,
    /// Tab visibility override.
    pub visible: Option<bool>,
}

/// The raw file shape as deserialized. Option fields default to `None`
/// when absent; unknown fields are ignored (no `deny_unknown_fields`);
/// wrong value types are a hard error (strict when present).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawMeta {
    title: Option<String>,
    order: Option<i64>,
    visible: Option<bool>,
}

impl CategoryMeta {
    /// Loads `<dir>/.meta.json`. A missing file yields pure defaults with
    /// no error; anything unreadable or unparseable yields an error message
    /// (the caller turns it into a warning and uses defaults).
    pub fn load(dir: &Path) -> Result<CategoryMeta, String> {
        let path = dir.join(META_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<RawMeta>(&text)
                .map(|raw| CategoryMeta {
                    title: raw.title,
                    order: raw.order,
                    visible: raw.visible,
                })
                .map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CategoryMeta::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Tab caption: the configured title or `fallback` (directory name).
    pub fn effective_title<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.title.as_deref().unwrap_or(fallback)
    }

    /// Tab sort key: the configured order or 0.
    pub fn effective_order(&self) -> i64 {
        self.order.unwrap_or(0)
    }

    /// Tab visibility: the configured flag or true.
    pub fn effective_visible(&self) -> bool {
        self.visible.unwrap_or(true)
    }
}

/// One GUI category tab: the clip category (directory name — the stable
/// identity used for tab-matching across rescans) plus its presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryTab {
    /// Directory name (or [`UNCATEGORIZED`] for the root category). This is
    /// the key clips are grouped by and the stable tab identity.
    pub dir_name: String,
    /// Tab caption (meta title or the fallback).
    pub title: String,
    /// Sort key (meta order or 0).
    pub order: i64,
    /// Whether the tab is shown at all.
    pub visible: bool,
}

/// Builds the GUI tab list for `clips_dir`: the root category first-class
/// (meta from `clips_dir/.meta.json`, default title [`ROOT_TITLE`]) plus one
/// entry per non-hidden one-level subdirectory (meta from
/// `<subdir>/.meta.json`, default title = directory name). Sorted by
/// (order, directory name). Meta problems become warning strings; the
/// affected tab falls back to defaults.
pub fn category_tabs(clips_dir: &Path) -> (Vec<CategoryTab>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let mut tabs: Vec<CategoryTab> = Vec::new();

    let root_meta = match CategoryMeta::load(clips_dir) {
        Ok(meta) => meta,
        Err(msg) => {
            warnings.push(format!("{META_FILE} (корневая вкладка): {msg}"));
            CategoryMeta::default()
        }
    };
    tabs.push(CategoryTab {
        dir_name: UNCATEGORIZED.to_string(),
        title: root_meta.effective_title(ROOT_TITLE).to_string(),
        order: root_meta.effective_order(),
        visible: root_meta.effective_visible(),
    });

    // The subdirectory set mirrors Clips::scan: non-hidden directories one
    // level deep, sorted by name for a deterministic input order.
    let mut subdirs: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(clips_dir) {
        for entry in entries.flatten() {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            if let Some(name) = entry.file_name().to_str()
                && !name.starts_with('.')
            {
                subdirs.push(name.to_string());
            }
        }
    }
    subdirs.sort();
    for name in subdirs {
        let meta = match CategoryMeta::load(&clips_dir.join(&name)) {
            Ok(meta) => meta,
            Err(msg) => {
                warnings.push(format!("{META_FILE} в папке «{name}»: {msg}"));
                CategoryMeta::default()
            }
        };
        tabs.push(CategoryTab {
            title: meta.effective_title(&name).to_string(),
            dir_name: name.clone(),
            order: meta.effective_order(),
            visible: meta.effective_visible(),
        });
    }
    tabs.sort_by(|a, b| {
        a.order
            .cmp(&b.order)
            .then_with(|| a.dir_name.cmp(&b.dir_name))
    });
    (tabs, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::File;
    use std::path::PathBuf;

    fn write_meta(dir: &Path, text: &str) {
        std::fs::write(dir.join(META_FILE), text).expect("write meta");
    }

    fn tab_by_dir<'a>(tabs: &'a [CategoryTab], dir: &str) -> &'a CategoryTab {
        tabs.iter()
            .find(|t| t.dir_name == dir)
            .unwrap_or_else(|| panic!("tab {dir} missing"))
    }

    fn subdir(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir(&path).expect("mkdir");
        path
    }

    // --- CategoryMeta::load ------------------------------------------------

    #[test]
    fn missing_file_is_pure_defaults_without_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let meta = CategoryMeta::load(dir.path()).expect("load");
        assert_eq!(meta, CategoryMeta::default());
        assert_eq!(meta.effective_title("Fallback"), "Fallback");
        assert_eq!(meta.effective_order(), 0);
        assert!(meta.effective_visible());
    }

    #[test]
    fn full_meta_parses_all_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_meta(
            dir.path(),
            r#"{"title":"Приветствия","order":10,"visible":true}"#,
        );
        let meta = CategoryMeta::load(dir.path()).expect("load");
        assert_eq!(meta.title.as_deref(), Some("Приветствия"));
        assert_eq!(meta.order, Some(10));
        assert_eq!(meta.visible, Some(true));
    }

    #[test]
    fn partial_meta_fills_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_meta(dir.path(), r#"{"order":-5}"#);
        let meta = CategoryMeta::load(dir.path()).expect("load");
        assert_eq!(meta.title, None);
        assert_eq!(meta.order, Some(-5));
        assert_eq!(meta.visible, None);
        assert_eq!(meta.effective_order(), -5);
    }

    #[test]
    fn empty_object_meta_is_valid() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_meta(dir.path(), "{}");
        let meta = CategoryMeta::load(dir.path()).expect("load");
        assert_eq!(meta, CategoryMeta::default());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_meta(dir.path(), r#"{"title":"X","color":"red","extra":[1,2]}"#);
        let meta = CategoryMeta::load(dir.path()).expect("load");
        assert_eq!(meta.title.as_deref(), Some("X"));
        assert_eq!(meta.order, None);
        assert_eq!(meta.visible, None);
    }

    #[test]
    fn malformed_json_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_meta(dir.path(), "{not json");
        let err = CategoryMeta::load(dir.path()).expect_err("malformed");
        assert!(err.contains(META_FILE), "{err}");
    }

    #[test]
    fn empty_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_meta(dir.path(), "");
        assert!(CategoryMeta::load(dir.path()).is_err());
    }

    #[test]
    fn wrong_field_types_are_strict_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_meta(dir.path(), r#"{"title":42}"#);
        assert!(CategoryMeta::load(dir.path()).is_err());

        let dir = tempfile::tempdir().expect("tempdir");
        write_meta(dir.path(), r#"{"order":"ten"}"#);
        assert!(CategoryMeta::load(dir.path()).is_err());

        let dir = tempfile::tempdir().expect("tempdir");
        write_meta(dir.path(), r#"{"visible":"yes"}"#);
        assert!(CategoryMeta::load(dir.path()).is_err());
    }

    #[test]
    fn unreadable_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(META_FILE);
        File::create(&path).expect("create");
        let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        std::fs::set_permissions(&path, perms).expect("chmod");
        let result = CategoryMeta::load(dir.path());
        // Running as root would read it anyway; only assert the error case
        // when the read actually failed.
        if std::fs::read_to_string(&path).is_err() {
            assert!(result.is_err());
        }
    }

    // --- category_tabs ------------------------------------------------------

    #[test]
    fn tabs_cover_root_and_subdirs_sorted_by_order_then_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        subdir(dir.path(), "Alpha");
        subdir(dir.path(), "Beta");
        write_meta(&dir.path().join("Alpha"), r#"{"order":5}"#);
        let (tabs, warnings) = category_tabs(dir.path());
        assert!(warnings.is_empty());
        let names: Vec<&str> = tabs.iter().map(|t| t.dir_name.as_str()).collect();
        // order: root 0, Beta 0, Alpha 5 — the 0 tie sorts by directory
        // name, and "Beta" (ASCII 'B') precedes «Без категории» (Cyrillic
        // bytes) in byte order.
        assert_eq!(names, vec!["Beta", UNCATEGORIZED, "Alpha"]);
        assert_eq!(tab_by_dir(&tabs, UNCATEGORIZED).title, ROOT_TITLE);
        assert_eq!(tab_by_dir(&tabs, "Alpha").title, "Alpha");
    }

    #[test]
    fn titles_and_order_come_from_meta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let greet = subdir(dir.path(), "hello_dir");
        write_meta(&greet, r#"{"title":"Приветствия","order":-5}"#);
        let (tabs, warnings) = category_tabs(dir.path());
        assert!(warnings.is_empty());
        assert_eq!(tab_by_dir(&tabs, "hello_dir").title, "Приветствия");
        // Negative order puts the subdir before the root tab.
        assert_eq!(tabs[0].dir_name, "hello_dir");
        assert_eq!(tabs[1].dir_name, UNCATEGORIZED);
    }

    #[test]
    fn invisible_tabs_are_flagged_hidden() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = subdir(dir.path(), "Secret");
        write_meta(&gone, r#"{"visible":false}"#);
        let (tabs, warnings) = category_tabs(dir.path());
        assert!(warnings.is_empty());
        // The tab is returned with the flag set; the GUI filters it out.
        assert!(!tab_by_dir(&tabs, "Secret").visible);
    }

    #[test]
    fn invisible_root_is_flagged_hidden() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_meta(dir.path(), r#"{"visible":false,"title":"Общий"}"#);
        let (tabs, _) = category_tabs(dir.path());
        assert!(!tab_by_dir(&tabs, UNCATEGORIZED).visible);
    }

    #[test]
    fn root_meta_sets_title_and_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_meta(dir.path(), r#"{"title":"Все подряд","order":100}"#);
        subdir(dir.path(), "Zed");
        let (tabs, _) = category_tabs(dir.path());
        assert_eq!(tab_by_dir(&tabs, UNCATEGORIZED).title, "Все подряд");
        assert_eq!(tab_by_dir(&tabs, UNCATEGORIZED).order, 100);
        assert_eq!(
            tabs.last().map(|t| t.dir_name.as_str()),
            Some(UNCATEGORIZED)
        );
    }

    #[test]
    fn malformed_meta_warns_and_falls_back_to_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let broken = subdir(dir.path(), "Broken");
        write_meta(&broken, "{oops");
        let hidden = subdir(dir.path(), "Hidden");
        write_meta(&hidden, r#"{"visible":false}"#);
        let (tabs, warnings) = category_tabs(dir.path());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Broken"), "{}", warnings[0]);
        // Broken meta falls back to defaults: visible, order 0, dir name.
        let tab = tab_by_dir(&tabs, "Broken");
        assert_eq!(tab.title, "Broken");
        assert!(tab.visible);
        assert_eq!(tab.order, 0);
        // The well-formed sibling still applies.
        assert!(!tab_by_dir(&tabs, "Hidden").visible);
    }

    #[test]
    fn missing_clips_dir_yields_root_tab_without_warnings() {
        let (tabs, warnings) = category_tabs(Path::new("/nonexistent/soundboard/clips"));
        assert!(warnings.is_empty());
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].dir_name, UNCATEGORIZED);
        assert_eq!(tabs[0].title, ROOT_TITLE);
    }

    #[test]
    fn hidden_subdirectories_are_not_tabs() {
        let dir = tempfile::tempdir().expect("tempdir");
        subdir(dir.path(), ".stash");
        let (tabs, warnings) = category_tabs(dir.path());
        assert!(warnings.is_empty());
        assert_eq!(tabs.len(), 1);
    }
}

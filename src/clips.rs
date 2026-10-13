//! Clip registry: scans the clips directory for playable audio files.
//!
//! Rules:
//! - the scan is recursive ONE level deep: `clips_dir/*.ext` plus
//!   `clips_dir/<Subdir>/*.ext`; deeper nesting is ignored on purpose
//!   (categories stay flat and predictable);
//! - files in the top level of `clips_dir` get the category
//!   [`UNCATEGORIZED`] («Без категории»); files in a subdirectory get the
//!   subdirectory name as the category;
//! - recognized extensions: `wav`, `mp3`, `ogg`, `oga`, `flac`, `opus`,
//!   `m4a` — matched case-insensitively;
//! - hidden entries (names starting with `.` — files AND subdirectories)
//!   are ignored;
//! - the clip name is the file stem (e.g. `airhorn.wav` -> `airhorn`) and
//!   stays unique across ALL categories: on a duplicate the file whose full
//!   path sorts first alphabetically wins, later ones are skipped with a
//!   warning. Hotkeys, the CLI and [`Clips::resolve`] keep working on bare
//!   names, so the first-wins policy is the only sane global rule;
//! - a missing or unreadable directory yields an empty registry plus a
//!   warning, never an error (an empty list is a usable state). An
//!   unreadable subdirectory becomes a warning too, never a failure.
//!
//! [`Clips::resolve`] matches a user-typed name: exact match first, then a
//! unique case-insensitive/prefix match. Ambiguous prefixes are an error
//! listing the candidates.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Recognized audio file extensions (lowercase; matching is case-insensitive).
pub const EXTENSIONS: &[&str] = &["wav", "mp3", "ogg", "oga", "flac", "opus", "m4a"];

/// Category of top-level clips (files directly in `clips_dir`).
pub const UNCATEGORIZED: &str = "Без категории";

/// Why a clip name could not be resolved. User-facing: messages are Russian
/// (they surface in the GUI error toast and the CLI stderr).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResolveError {
    /// No clip matches the query exactly or as a unique prefix.
    #[error("клип «{query}» не найден (доступны: {available})")]
    NotFound {
        /// The query as typed.
        query: String,
        /// All registered clip names, sorted.
        available: String,
    },
    /// The query is a prefix of several clips.
    #[error("«{query}» неоднозначно, кандидаты: {candidates}")]
    Ambiguous {
        /// The query as typed.
        query: String,
        /// Matching clip names, sorted, comma-separated.
        candidates: String,
    },
}

/// One resolved clip: its registry name, category and file path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipRef<'a> {
    /// Registry name (file stem).
    pub name: &'a str,
    /// Category (subdirectory name or [`UNCATEGORIZED`]).
    pub category: &'a str,
    /// Absolute-ish path of the audio file.
    pub path: &'a Path,
}

/// One registry entry as returned by [`Clips::entries`] (sorted view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipEntry<'a> {
    /// Registry name (file stem).
    pub name: &'a str,
    /// Category (subdirectory name or [`UNCATEGORIZED`]).
    pub category: &'a str,
    /// Path of the audio file.
    pub path: &'a Path,
}

/// One scanned file: its path and the category it landed in.
struct Scanned {
    path: PathBuf,
    category: String,
}

/// The set of clips found in `clips_dir`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Clips {
    by_name: BTreeMap<String, (PathBuf, String)>,
    warnings: Vec<String>,
}

impl Clips {
    /// Scans `dir` for audio files (see the module docs for the rules).
    /// Never fails: problems become warnings and/or an empty registry.
    pub fn scan(dir: &Path) -> Self {
        let mut clips = Clips::default();
        let mut scanned = Vec::new();
        collect_level(dir, UNCATEGORIZED, &mut scanned, &mut clips.warnings);
        // One level of subdirectories becomes categories; deeper levels are
        // never visited (documented behavior).
        let mut subdirs: Vec<PathBuf> = Vec::new();
        // The top-level read already produced the warning on failure;
        // subdir listing errors are swallowed on purpose.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if !is_dir {
                    continue;
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if name.starts_with('.') {
                    continue; // hidden subdirectories are never categories
                }
                subdirs.push(entry.path());
            }
        }
        subdirs.sort();
        for subdir in subdirs {
            let category = subdir
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default();
            collect_level(&subdir, category, &mut scanned, &mut clips.warnings);
        }
        // Deterministic duplicate handling across ALL categories: sort by
        // full path, the first file wins.
        scanned.sort_by(|a, b| a.path.cmp(&b.path));
        for file in scanned {
            let Some(stem) = file.path.file_stem().and_then(OsStr::to_str) else {
                clips.warnings.push(format!(
                    "пропускаю файл с нечитаемым именем: {}",
                    file.path.display()
                ));
                continue;
            };
            if stem.starts_with('.') {
                continue; // hidden files are never clips
            }
            if clips.by_name.contains_key(stem) {
                let kept = clips.by_name[stem].0.display();
                clips.warnings.push(format!(
                    "дубликат имени клипа «{stem}»: {} игнорируется, остаётся {}",
                    file.path.display(),
                    kept
                ));
                continue;
            }
            clips
                .by_name
                .insert(stem.to_string(), (file.path, file.category));
        }
        clips
    }

    /// Sorted clip names (ordered by category, then name — the same order
    /// as [`Clips::entries`]). This is the input hotkeys and the CLI keep
    /// consuming.
    #[must_use]
    pub fn list(&self) -> Vec<&str> {
        self.entries().into_iter().map(|e| e.name).collect()
    }

    /// All clips as (name, category, path) entries, sorted by
    /// (category, name) — the GUI grid grouping order.
    #[must_use]
    pub fn entries(&self) -> Vec<ClipEntry<'_>> {
        let mut entries: Vec<ClipEntry<'_>> = self
            .by_name
            .iter()
            .map(|(name, (path, category))| ClipEntry {
                name: name.as_str(),
                category: category.as_str(),
                path: path.as_path(),
            })
            .collect();
        entries.sort_by(|a, b| a.category.cmp(b.category).then_with(|| a.name.cmp(b.name)));
        entries
    }

    /// Path and category of a clip by exact registry name.
    // Registry API: exercised by unit tests; the GUI walks `entries()`.
    #[allow(dead_code)]
    #[must_use]
    pub fn get(&self, name: &str) -> Option<ClipRef<'_>> {
        self.by_name
            .get_key_value(name)
            .map(|(name, (path, category))| ClipRef {
                name: name.as_str(),
                category: category.as_str(),
                path: path.as_path(),
            })
    }

    /// Resolves a user-typed name: exact match first, then a unique
    /// case-insensitive or prefix match.
    pub fn resolve(&self, query: &str) -> Result<ClipRef<'_>, ResolveError> {
        if let Some(clip) = self.get(query) {
            return Ok(clip);
        }
        let lower = query.to_lowercase();
        let matches: Vec<ClipRef<'_>> = self
            .by_name
            .iter()
            .filter(|(name, _)| {
                let name_lower = name.to_lowercase();
                name_lower.starts_with(&lower)
            })
            .map(|(name, (path, category))| ClipRef {
                name: name.as_str(),
                category: category.as_str(),
                path: path.as_path(),
            })
            .collect();
        match matches.as_slice() {
            [clip] => Ok(clip.clone()),
            [] => Err(ResolveError::NotFound {
                query: query.to_string(),
                available: self.list().join(", "),
            }),
            many => Err(ResolveError::Ambiguous {
                query: query.to_string(),
                candidates: many
                    .iter()
                    .map(|clip| clip.name)
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
        }
    }

    /// Non-fatal problems found during the scan.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Number of registered clips.
    // Registry API: exercised by unit tests.
    #[allow(dead_code)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// True when no clips were found.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Collects audio files of one directory level into `scanned`, assigning
/// every file `category`. A missing/unreadable directory becomes a warning.
fn collect_level(
    dir: &Path,
    category: &str,
    scanned: &mut Vec<Scanned>,
    warnings: &mut Vec<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warnings.push(format!(
                "не удалось прочитать папку клипов {}: {e}",
                dir.display()
            ));
            return;
        }
    };
    for entry in entries {
        match entry {
            Ok(entry) => {
                let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
                if !is_file {
                    continue; // subdirectories and specials are not clips here
                }
                let recognized = entry
                    .path()
                    .extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|ext| EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
                if !recognized {
                    continue;
                }
                scanned.push(Scanned {
                    path: entry.path(),
                    category: category.to_string(),
                });
            }
            Err(e) => warnings.push(format!("не удалось получить содержимое {dir:?}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::File;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        File::create(&path).expect("create test file");
        path
    }

    fn mkdir(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir(&path).expect("mkdir");
        path
    }

    #[test]
    fn scan_keeps_audio_files_by_stem() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(dir.path(), "airhorn.wav");
        touch(dir.path(), "applause.mp3");
        touch(dir.path(), "README.md");
        touch(dir.path(), "notes.txt");
        let clips = Clips::scan(dir.path());
        assert_eq!(clips.list(), vec!["airhorn", "applause"]);
        assert!(clips.warnings().is_empty());
        let clip = clips.get("airhorn").expect("airhorn");
        assert_eq!(clip.path, dir.path().join("airhorn.wav"));
        assert_eq!(clip.category, UNCATEGORIZED);
    }

    #[test]
    fn scan_extension_match_is_case_insensitive() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(dir.path(), "loud.WAV");
        touch(dir.path(), "soft.Mp3");
        touch(dir.path(), "jazz.FlAc");
        let clips = Clips::scan(dir.path());
        assert_eq!(clips.list(), vec!["jazz", "loud", "soft"]);
    }

    #[test]
    fn duplicate_stem_keeps_first_alphabetically_with_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(dir.path(), "airhorn.wav");
        touch(dir.path(), "airhorn.mp3");
        let clips = Clips::scan(dir.path());
        assert_eq!(clips.list(), vec!["airhorn"]);
        assert_eq!(clips.len(), 1);
        // "airhorn.mp3" < "airhorn.wav" alphabetically, so the mp3 wins.
        assert_eq!(
            clips.get("airhorn").expect("airhorn").path,
            dir.path().join("airhorn.mp3")
        );
        assert_eq!(clips.warnings().len(), 1);
        assert!(clips.warnings()[0].contains("дубликат имени клипа «airhorn»"));
    }

    #[test]
    fn resolve_exact_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(dir.path(), "airhorn.wav");
        touch(dir.path(), "airhorn_extra.wav");
        let clips = Clips::scan(dir.path());
        let r = clips.resolve("airhorn").expect("resolve");
        assert_eq!(r.name, "airhorn");
        assert_eq!(r.path, dir.path().join("airhorn.wav"));
    }

    #[test]
    fn resolve_unique_case_insensitive_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(dir.path(), "AirHorn.wav");
        let clips = Clips::scan(dir.path());
        let r = clips.resolve("airhorn").expect("resolve");
        assert_eq!(r.name, "AirHorn");
    }

    #[test]
    fn resolve_unique_prefix_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(dir.path(), "airhorn.wav");
        touch(dir.path(), "applause.wav");
        let clips = Clips::scan(dir.path());
        let r = clips.resolve("air").expect("resolve");
        assert_eq!(r.name, "airhorn");
    }

    #[test]
    fn resolve_ambiguous_prefix_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(dir.path(), "airhorn.wav");
        touch(dir.path(), "airhorn_extra.wav");
        let clips = Clips::scan(dir.path());
        let err = clips.resolve("air").expect_err("ambiguous");
        assert!(matches!(err, ResolveError::Ambiguous { .. }));
        assert!(err.to_string().contains("неоднозначно"), "{err}");
        assert!(err.to_string().contains("airhorn, airhorn_extra"));
    }

    #[test]
    fn resolve_unknown_name_is_not_found_with_available_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(dir.path(), "airhorn.wav");
        let clips = Clips::scan(dir.path());
        let err = clips.resolve("nope").expect_err("not found");
        assert!(matches!(err, ResolveError::NotFound { .. }));
        assert!(err.to_string().contains("не найден"), "{err}");
        assert!(err.to_string().contains("airhorn"));
    }

    #[test]
    fn empty_dir_yields_empty_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clips = Clips::scan(dir.path());
        assert!(clips.is_empty());
        assert!(clips.list().is_empty());
        assert!(clips.warnings().is_empty());
    }

    #[test]
    fn missing_dir_yields_empty_registry_with_warning() {
        let clips = Clips::scan(Path::new("/nonexistent/soundboard/clips"));
        assert!(clips.is_empty());
        assert_eq!(clips.warnings().len(), 1);
        assert!(clips.warnings()[0].contains("не удалось прочитать папку клипов"));
    }
    #[test]
    fn directories_and_dotfiles_are_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        mkdir(dir.path(), "not_a_clip_dir");
        touch(dir.path(), ".hidden.wav");
        touch(dir.path(), "real.wav");
        let clips = Clips::scan(dir.path());
        assert_eq!(clips.list(), vec!["real"]);
        assert!(clips.warnings().is_empty());
    }

    // --- categories (recursive scan, one level) ---------------------------

    #[test]
    fn subdirectory_files_get_the_subdir_as_category() {
        let dir = tempfile::tempdir().expect("tempdir");
        let memes = mkdir(dir.path(), "Мемы");
        touch(dir.path(), "airhorn.wav");
        touch(&memes, "bruh.mp3");
        let clips = Clips::scan(dir.path());
        // «Без категории» sorts before «Мемы» (byte order of the first
        // Cyrillic letters), so the uncategorized clip comes first.
        assert_eq!(clips.list(), vec!["airhorn", "bruh"]);
        let bruh = clips.get("bruh").expect("bruh");
        assert_eq!(bruh.category, "Мемы");
        assert_eq!(bruh.path, memes.join("bruh.mp3"));
        assert_eq!(
            clips.get("airhorn").expect("airhorn").category,
            UNCATEGORIZED
        );
    }

    #[test]
    fn deeper_than_one_level_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = mkdir(dir.path(), "Sub");
        let deep = mkdir(&sub, "Deep");
        touch(dir.path(), "top.wav");
        touch(&sub, "mid.wav");
        touch(&deep, "bottom.wav");
        let clips = Clips::scan(dir.path());
        // bottom.wav is two levels down: ignored by design.
        assert_eq!(clips.entries()[0].category, "Sub");
        assert_eq!(clips.list(), vec!["mid", "top"]);
        assert!(clips.resolve("bottom").is_err());
    }

    #[test]
    fn duplicate_stem_across_categories_first_path_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = mkdir(dir.path(), "Zed");
        touch(dir.path(), "same.wav");
        touch(&sub, "same.mp3");
        let clips = Clips::scan(dir.path());
        // Names stay unique: "Zed/same.mp3" sorts before "same.wav".
        assert_eq!(clips.len(), 1);
        let kept = clips.get("same").expect("same");
        assert_eq!(kept.path, sub.join("same.mp3"));
        assert_eq!(kept.category, "Zed");
        assert!(clips.warnings()[0].contains("дубликат имени клипа «same»"));
    }

    #[test]
    fn entries_are_sorted_by_category_then_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = mkdir(dir.path(), "Alpha");
        let b = mkdir(dir.path(), "Beta");
        touch(dir.path(), "zeta.wav");
        touch(dir.path(), "alpha_root.wav");
        touch(&a, "m.wav");
        touch(&a, "k.wav");
        touch(&b, "x.wav");
        let clips = Clips::scan(dir.path());
        let order: Vec<(&str, &str)> = clips
            .entries()
            .into_iter()
            .map(|e| (e.category, e.name))
            .collect();
        assert_eq!(
            order,
            vec![
                ("Alpha", "k"),
                ("Alpha", "m"),
                ("Beta", "x"),
                (UNCATEGORIZED, "alpha_root"),
                (UNCATEGORIZED, "zeta"),
            ]
        );
    }

    #[test]
    fn hidden_subdirectories_are_not_categories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hidden = mkdir(dir.path(), ".stash");
        touch(&hidden, "secret.wav");
        touch(dir.path(), "real.wav");
        let clips = Clips::scan(dir.path());
        assert_eq!(clips.list(), vec!["real"]);
        assert!(clips.warnings().is_empty());
    }
}

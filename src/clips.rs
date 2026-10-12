//! Clip registry: scans the clips directory for playable audio files.
//!
//! Rules:
//! - the scan is non-recursive (top level of `clips_dir` only);
//! - recognized extensions: `wav`, `mp3`, `ogg`, `oga`, `flac`, `opus`,
//!   `m4a` — matched case-insensitively;
//! - hidden files (file name starting with `.`) are ignored;
//! - the clip name is the file stem (e.g. `airhorn.wav` -> `airhorn`);
//! - stems must be unique: on a duplicate the file that sorts first
//!   alphabetically wins, later ones are skipped with a warning;
//! - a missing or unreadable directory yields an empty registry plus a
//!   warning, never an error (an empty list is a usable state).
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

/// One resolved clip: its registry name and file path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipRef<'a> {
    /// Registry name (file stem).
    pub name: &'a str,
    /// Absolute-ish path of the audio file.
    pub path: &'a Path,
}

/// The set of clips found in `clips_dir`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Clips {
    by_name: BTreeMap<String, PathBuf>,
    warnings: Vec<String>,
}

impl Clips {
    /// Scans `dir` for audio files (see the module docs for the rules).
    /// Never fails: problems become warnings and/or an empty registry.
    pub fn scan(dir: &Path) -> Self {
        let mut clips = Clips::default();
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                clips.warnings.push(format!(
                    "не удалось прочитать папку клипов {}: {e}",
                    dir.display()
                ));
                return clips;
            }
        };
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
                    if !is_file {
                        continue; // subdirectories and specials are ignored silently
                    }
                    files.push(entry.path());
                }
                Err(e) => clips.warnings.push(format!("не удалось получить содержимое {dir:?}: {e}")),
            }
        }
        // Deterministic duplicate handling: sort by file name, first wins.
        files.sort();
        for path in files {
            let Some(ext) = path.extension().and_then(OsStr::to_str) else {
                continue; // no extension (e.g. dotfiles) -> not an audio file
            };
            if !EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
                clips.warnings.push(format!(
                    "пропускаю файл с нечитаемым именем: {}",
                    path.display()
                ));
                continue;
            };
            if stem.starts_with('.') {
                continue; // hidden files are never clips
            }
            if clips.by_name.contains_key(stem) {
                clips.warnings.push(format!(
                    "дубликат имени клипа «{stem}»: {} игнорируется, остаётся {}",
                    path.display(),
                    clips.by_name[stem].display()
                ));
                continue;
            }
            clips.by_name.insert(stem.to_string(), path);
        }
        clips
    }

    /// Sorted clip names.
    #[must_use]
    pub fn list(&self) -> Vec<&str> {
        self.by_name.keys().map(String::as_str).collect()
    }

    /// Path of a clip by exact registry name.
    // Registry API for the M3 GUI; exercised by unit tests for now.
    #[allow(dead_code)]
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Path> {
        self.by_name.get(name).map(PathBuf::as_path)
    }

    /// Resolves a user-typed name: exact match first, then a unique
    /// case-insensitive or prefix match.
    pub fn resolve(&self, query: &str) -> Result<ClipRef<'_>, ResolveError> {
        if let Some((name, path)) = self.by_name.get_key_value(query) {
            return Ok(ClipRef {
                name,
                path: path.as_path(),
            });
        }
        let lower = query.to_lowercase();
        let matches: Vec<&str> = self
            .by_name
            .keys()
            .map(String::as_str)
            .filter(|name| {
                let name_lower = name.to_lowercase();
                name_lower == lower || name_lower.starts_with(&lower)
            })
            .collect();
        match matches.as_slice() {
            [name] => Ok(ClipRef {
                name,
                path: self.by_name[*name].as_path(),
            }),
            [] => Err(ResolveError::NotFound {
                query: query.to_string(),
                available: self.list().join(", "),
            }),
            many => Err(ResolveError::Ambiguous {
                query: query.to_string(),
                candidates: many.join(", "),
            }),
        }
    }

    /// Non-fatal problems found during the scan.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Number of registered clips.
    // Registry API for the M3 GUI; exercised by unit tests for now.
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::File;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        File::create(&path).expect("create test file");
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
        assert_eq!(
            clips.get("airhorn"),
            Some(dir.path().join("airhorn.wav").as_path())
        );
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
            clips.get("airhorn"),
            Some(dir.path().join("airhorn.mp3").as_path())
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
        std::fs::create_dir(dir.path().join("subdir.wav")).expect("mkdir");
        touch(dir.path(), ".hidden.wav");
        touch(dir.path(), "real.wav");
        let clips = Clips::scan(dir.path());
        assert_eq!(clips.list(), vec!["real"]);
        assert!(clips.warnings().is_empty());
    }
}

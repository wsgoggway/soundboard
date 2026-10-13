//! Soundboard configuration.
//!
//! Stored as TOML at `~/.config/soundboard/config.toml` (XDG-respected via
//! `dirs::config_dir`).
//!
//! Loading rules:
//! - missing file -> built-in defaults;
//! - malformed file (invalid TOML, wrong value types, unknown keys,
//!   non-finite volume) -> error;
//! - `volume` outside `0.0..=1.0` is CLAMPED into range (deliberate choice:
//!   a stale hand-edited value must not make the whole app refuse to start;
//!   the clamped value is what gets re-saved on the next `Config::save`).
//! - `clips_dir` supports a leading `~` (expanded against the user's home
//!   directory); the default is `$XDG_DATA_HOME/soundboard-clips`
//!   (falling back to `~/.local/share/soundboard-clips`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Reserved `volume` value meaning "not a number" is rejected at load time.
pub const VOLUME_MAX: f32 = 1.0;
/// Minimum allowed `volume`.
pub const VOLUME_MIN: f32 = 0.0;

/// Application configuration (see module docs for loading rules).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Directory holding the meme audio clips.
    #[serde(default = "default_clips_dir")]
    pub clips_dir: PathBuf,
    /// Playback volume, `0.0..=1.0` (clamped on load).
    #[serde(default = "default_volume")]
    pub volume: f32,
    /// A new clip stops the currently playing one.
    #[serde(default = "default_true")]
    pub interrupt_mode: bool,
    /// Pass the real microphone through into the virtual mic.
    #[serde(default = "default_true")]
    pub mic_mix: bool,
    /// Also route clip audio to the real speakers (local monitoring).
    #[serde(default)]
    pub monitoring: bool,
}

fn default_clips_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| expand_tilde(Path::new("~/.local/share")))
        .join("soundboard-clips")
}

fn default_volume() -> f32 {
    0.9
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Config {
            clips_dir: default_clips_dir(),
            volume: default_volume(),
            interrupt_mode: true,
            mic_mix: true,
            monitoring: false,
        }
    }
}

/// Expands a single leading `~` or `~/...` to the user's home directory.
/// Other paths are returned unchanged.
fn expand_tilde(path: &Path) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return path.to_path_buf();
    };
    if path == Path::new("~") {
        return home;
    }
    match path.strip_prefix("~/") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Default config file location: `<config dir>/soundboard/config.toml`.
///
/// # Errors
/// Fails when the XDG config directory cannot be determined (neither
/// `XDG_CONFIG_HOME` nor `HOME` usable). It used to fall back silently to the
/// current working directory, which could create stray `./soundboard` folders
/// — an explicit error is the honest behavior.
pub fn config_path() -> Result<PathBuf> {
    config_path_in(dirs::config_dir().as_deref())
}

/// Pure core of [`config_path`], unit-testable without touching the
/// environment.
fn config_path_in(config_dir: Option<&Path>) -> Result<PathBuf> {
    let dir = config_dir.ok_or_else(|| {
        anyhow::anyhow!(
            "не удалось определить каталог конфигурации (задайте XDG_CONFIG_HOME или HOME)"
        )
    })?;
    Ok(dir.join("soundboard").join("config.toml"))
}

impl Config {
    /// Loads config from the default location; defaults if the file is
    /// missing, error on malformed content or on an undeterminable config
    /// directory.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    /// Loads config from an explicit path (unit-testable variant of [`Config::load`]).
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(e).with_context(|| format!("не удалось прочитать {}", path.display()));
            }
        };
        let mut cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("некорректный файл конфигурации {}", path.display()))?;
        if !cfg.volume.is_finite() {
            anyhow::bail!(
                "некорректный файл конфигурации {}: volume должен быть конечным числом, получено {}",
                path.display(),
                cfg.volume
            );
        }
        cfg.volume = cfg.volume.clamp(VOLUME_MIN, VOLUME_MAX);
        cfg.clips_dir = expand_tilde(&cfg.clips_dir);
        Ok(cfg)
    }

    /// Saves config to the default location, creating parent directories.
    /// Used by the GUI (M3); not exercised by the CLI paths yet.
    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path()?)
    }

    /// Saves config to an explicit path (unit-testable variant of [`Config::save`]).
    #[allow(dead_code)]
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("не удалось создать {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("не удалось сериализовать конфигурацию")?;
        std::fs::write(path, text)
            .with_context(|| format!("не удалось записать {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_documented_values() {
        let cfg = Config::default();
        let expected = dirs::data_dir()
            .unwrap_or_else(|| expand_tilde(Path::new("~/.local/share")))
            .join("soundboard-clips");
        assert_eq!(cfg.clips_dir, expected);
        assert_eq!(cfg.volume, 0.9);
        assert!(cfg.interrupt_mode);
        assert!(cfg.mic_mix);
        assert!(!cfg.monitoring);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = Config::load_from(&dir.path().join("absent.toml")).expect("load");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn roundtrip_save_load_preserves_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roundtrip.toml");
        let original = Config {
            clips_dir: PathBuf::from("/tmp/clips"),
            volume: 0.42,
            interrupt_mode: false,
            mic_mix: false,
            monitoring: true,
        };
        original.save_to(&path).expect("save");
        let loaded = Config::load_from(&path).expect("load");
        assert_eq!(loaded, original);
    }

    #[test]
    fn partial_file_fills_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("partial.toml");
        std::fs::write(&path, "volume = 0.5\n").expect("write");
        let cfg = Config::load_from(&path).expect("load");
        assert_eq!(cfg.volume, 0.5);
        assert_eq!(cfg.clips_dir, Config::default().clips_dir);
        assert!(cfg.interrupt_mode);
    }

    #[test]
    fn malformed_toml_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broken.toml");
        std::fs::write(&path, "volume = = = nope\n").expect("write");
        assert!(Config::load_from(&path).is_err());
    }

    #[test]
    fn wrong_value_type_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wrongtype.toml");
        std::fs::write(&path, "volume = \"loud\"\n").expect("write");
        assert!(Config::load_from(&path).is_err());
    }

    #[test]
    fn unknown_key_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("unknown.toml");
        std::fs::write(&path, "volum = 0.5\n").expect("write");
        assert!(Config::load_from(&path).is_err());
    }

    #[test]
    fn out_of_range_volume_is_clamped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clamp.toml");
        std::fs::write(&path, "volume = 2.5\n").expect("write");
        let cfg = Config::load_from(&path).expect("load");
        assert_eq!(cfg.volume, VOLUME_MAX);

        std::fs::write(&path, "volume = -1.0\n").expect("write");
        let cfg = Config::load_from(&path).expect("load");
        assert_eq!(cfg.volume, VOLUME_MIN);
    }

    #[test]
    fn non_finite_volume_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nan.toml");
        std::fs::write(&path, "volume = nan\n").expect("write");
        assert!(Config::load_from(&path).is_err());
    }

    #[test]
    fn tilde_in_clips_dir_is_expanded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tilde.toml");
        std::fs::write(&path, "clips_dir = \"~/my-clips\"\n").expect("write");
        let cfg = Config::load_from(&path).expect("load");
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(cfg.clips_dir, home.join("my-clips"));
    }

    #[test]
    fn config_path_uses_given_base_dir() {
        let path = config_path_in(Some(Path::new("/cfg"))).expect("path");
        assert_eq!(path, Path::new("/cfg/soundboard/config.toml"));
    }

    #[test]
    fn config_path_errors_without_base_dir() {
        let err = config_path_in(None).expect_err("must not fall back to CWD");
        assert!(err.to_string().contains("каталог конфигурации"), "{err}");
    }
}

//! Live refresh of the Plasma global-shortcut daemon after our hotkey
//! desktop files change.
//!
//! Why this exists (verified on Plasma 6.7.5, 2026-10-12): `kglobalacceld`
//! reads each service component's `Exec` line exactly once — at daemon
//! startup (`loadSettings`) and when it first notices the component. On
//! ksycoca `databaseChanged` its `detectAppsWithShortcuts` explicitly skips
//! components that are "already there", so REWRITING an existing desktop
//! file (e.g. after the binary moved) leaves the daemon holding the stale
//! `Exec`: the shortcut fires, but the launch fails with
//! «Невозможно найти программу …» (see `docs/kde-hotkeys.md`).
//!
//! The daemon only re-creates a component when its file reappears after
//! being gone (`refreshServices` drops components whose storage id no
//! longer resolves). A refresh is therefore a hide/restore dance:
//!
//! 1. rename our files away and run `kbuildsycoca6`;
//! 2. WAIT until the daemon actually dropped the components (a refresh pass
//!    that still sees the files would keep the stale service object — the
//!    wait is required for correctness, not cosmetic);
//! 3. rename the files back and run `kbuildsycoca6` again;
//! 4. wait until the components are registered again (now with fresh
//!    `Exec`).
//!
//! The daemon is queried over D-Bus with the `gdbus` CLI (no D-Bus crate —
//! same subprocess precedent as `kbuildsycoca6` in `hotkeys.rs`). Without
//! the daemon (non-Plasma session) the dance degrades to a no-op success.
//!
//! Blocking: every step spawns processes and sleeps — never call this on
//! the UI thread (the GUI runs it on `gio::spawn_blocking`). Used by both
//! the GUI «Включить/Обновить хоткеи» flow and the CLI `hotkeys --write`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

/// Result of [`refresh_services`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshOutcome {
    /// The daemon saw the components dropped and re-registered.
    Refreshed,
    /// `org.kde.kglobalaccel` is not on the session bus (non-Plasma
    /// session): there is nothing to refresh; the caller should still
    /// refresh the ksycoca cache itself.
    NoDaemon,
}

/// How long we wait for the daemon to drop / re-register our components.
const PROPAGATE_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval while waiting for the daemon.
const POLL_STEP: Duration = Duration::from_millis(100);

/// Suffix of the temporary names used to hide our files during the dance.
/// Does NOT end in `.desktop`, so neither ksycoca nor
/// `hotkeys::read_managed_files` ever see the hidden state.
const HIDDEN_SUFFIX: &str = "sb-hidden";

/// Runs the hide/restore dance for the desktop files named `file_names` in
/// `dir` so a running Plasma re-reads their `Exec` lines. Idempotent: with
/// no daemon present it returns [`RefreshOutcome::NoDaemon`] and changes
/// nothing (hidden leftovers from a crashed run are restored first).
///
/// # Errors
/// When the files cannot be renamed/restored, `kbuildsycoca6` fails, or the
/// daemon does not follow the drop/re-register within the timeout — in
/// every error case the files are restored to their real names first.
pub(crate) fn refresh_services(dir: &Path, file_names: &[String]) -> Result<RefreshOutcome> {
    // Probe the daemon before touching anything: no kglobalaccel on the
    // session bus means there is nothing to refresh (hotkeys are a Plasma
    // mechanism anyway).
    let probe = match all_components() {
        Ok(output) => output,
        Err(_) => return Ok(RefreshOutcome::NoDaemon),
    };
    restore_hidden_leftovers(dir, file_names);
    if !any_registered(&probe, file_names) {
        // Nothing registered yet (first registration): only the re-add wait
        // matters — and it starts from a refresh, no hide phase needed.
        return finish_readd(file_names).map(|()| RefreshOutcome::Refreshed);
    }

    // Hide phase: files gone from disk, then from the daemon.
    let mut hidden: Vec<PathBuf> = Vec::new();
    for name in file_names {
        let path = dir.join(name);
        let hidden_path = hidden_path(&path);
        let _ = std::fs::remove_file(&hidden_path); // stale leftover
        std::fs::rename(&path, &hidden_path)
            .with_context(|| format!("не удалось временно спрятать {name}"))?;
        hidden.push(hidden_path);
    }
    let restore = |hidden: &[PathBuf]| {
        for path in hidden {
            let _ = std::fs::rename(path, restored_path(path));
        }
    };
    if let Err(e) = crate::hotkeys::refresh_ksycoca().and_then(|()| {
        wait_registered(file_names, false)
            .then_some(())
            .context("kglobalaccel не выгрузил старые компоненты")
    }) {
        restore(&hidden);
        return Err(e);
    }

    // Restore phase: files back, daemon re-creates them with fresh Exec.
    restore(&hidden);
    finish_readd(file_names).map(|()| RefreshOutcome::Refreshed)
}

/// Final `kbuildsycoca6` plus the wait until the daemon re-registered every
/// component. Files must already be at their real names.
///
/// # Errors
/// When `kbuildsycoca6` fails or the components do not come back in time.
fn finish_readd(file_names: &[String]) -> Result<()> {
    crate::hotkeys::refresh_ksycoca()?;
    if wait_registered(file_names, true) {
        Ok(())
    } else {
        bail!(
            "kglobalaccel не перерегистрировал компоненты — перелогиньтесь, чтобы применить хоткеи"
        );
    }
}

/// Hides a desktop file under its temporary name.
fn hidden_path(path: &Path) -> PathBuf {
    path.with_extension(HIDDEN_SUFFIX)
}

/// Inverse of [`hidden_path`].
fn restored_path(hidden: &Path) -> PathBuf {
    hidden.with_extension("desktop")
}

/// Restores hidden leftovers from a previous crashed run (best effort).
fn restore_hidden_leftovers(dir: &Path, file_names: &[String]) {
    for name in file_names {
        let hidden = hidden_path(&dir.join(name));
        if hidden.exists() {
            let _ = std::fs::rename(&hidden, restored_path(&hidden));
        }
    }
}

/// Polls the daemon until every one of `file_names` is registered
/// (`want = true`) or none of them is (`want = false`).
fn wait_registered(file_names: &[String], want: bool) -> bool {
    let deadline = Instant::now() + PROPAGATE_TIMEOUT;
    loop {
        let ok = all_components()
            .map(|output| {
                if want {
                    all_registered(&output, file_names)
                } else {
                    !any_registered(&output, file_names)
                }
            })
            .unwrap_or(false);
        if ok {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_STEP);
    }
}

/// Runs `gdbus call … org.kde.KGlobalAccel.allComponents` and returns its
/// stdout. Fails when `gdbus` is missing, the session bus is unreachable,
/// or the name `org.kde.kglobalaccel` has no owner (non-Plasma session).
fn all_components() -> anyhow::Result<String> {
    let output = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.kde.kglobalaccel",
            "--object-path",
            "/kglobalaccel",
            "--method",
            "org.kde.KGlobalAccel.allComponents",
        ])
        .output()
        .context("не удалось запустить gdbus")?;
    if !output.status.success() {
        bail!("gdbus org.kde.KGlobalAccel.allComponents завершился с ошибкой");
    }
    String::from_utf8(output.stdout)
        .context("gdbus вернул не-UTF8 вывод")
        .map(|s| s.to_string())
}

/// Storage id → D-Bus object path token: the component object paths use
/// underscores (`/component/net_local_soundboard_bruh_desktop`).
fn path_token(storage_id: &str) -> String {
    storage_id.replace('.', "_")
}

/// True when at least one of `file_names` appears among the components in
/// `gdbus` output.
fn any_registered(output: &str, file_names: &[String]) -> bool {
    file_names
        .iter()
        .any(|name| output.contains(&path_token(name)))
}

/// True when every one of `file_names` appears among the components.
fn all_registered(output: &str, file_names: &[String]) -> bool {
    file_names
        .iter()
        .all(|name| output.contains(&path_token(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec![
            "net.local.soundboard.bruh.desktop".to_string(),
            "net.local.soundboard.faaah.desktop".to_string(),
        ]
    }

    fn components_output() -> String {
        // Shape of a real `gdbus call … allComponents` reply.
        "(['org.kde.krunner', '/component/plasmashell', \
          '/component/net_local_soundboard_bruh_desktop']"
            .to_string()
    }

    #[test]
    fn storage_id_maps_to_underscored_path_token() {
        assert_eq!(
            path_token("net.local.soundboard.bruh.desktop"),
            "net_local_soundboard_bruh_desktop"
        );
    }

    #[test]
    fn registered_checks_match_gdbus_output_shape() {
        let out = components_output();
        let all = names();
        assert!(any_registered(&out, &all));
        assert!(!all_registered(&out, &all)); // faaah is missing there
        assert!(all_registered(&out, &[all[0].clone()]));
        assert!(!any_registered(
            &out,
            &["net.local.soundboard.other.desktop".to_string()]
        ));
        assert!(!any_registered("", &all));
        assert!(!any_registered(&out, &[])); // nothing asked → nothing found
    }

    #[test]
    fn hidden_and_restored_names_round_trip() {
        let dir = Path::new("/home/u/.local/share/applications");
        let real = dir.join("net.local.soundboard.bruh.desktop");
        let hidden = hidden_path(&real);
        assert_eq!(hidden, dir.join("net.local.soundboard.bruh.sb-hidden"));
        assert_eq!(restored_path(&hidden), real);
        // The hidden name must not look like a desktop file.
        assert!(!hidden.to_string_lossy().ends_with(".desktop"));
    }
}

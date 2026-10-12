//! System tray presence: a StatusNotifierItem via the `ksni` crate — the
//! native Plasma tray protocol (works on Wayland/KWin; Plasma provides
//! `org.kde.StatusNotifierWatcher`, no libappindicator needed).
//!
//! Threading: `ksni::blocking::spawn` runs the whole SNI service (D-Bus
//! object + menu) on its own background thread. That thread must NEVER
//! touch GTK — menu actions only send [`TrayCmd`]s over an async channel
//! that the GTK main loop drains (same pattern as the worker channels), and
//! the main loop side updates the tray read-only state via
//! `Handle::update` (which marshals the closure onto the ksni thread).
//!
//! The menu mirrors the window: toggle visibility, stop all playbacks
//! (`WorkerCmd::Stop { name: None }` — the worker handle is thread-safe and
//! owns all audio), a passive graph-status line fed from
//! `GuiMsg::Status`, and a full quit that goes through the SAME graceful
//! shutdown path as SIGINT/SIGTERM (worker `Shutdown` →
//! `GuiMsg::ShutdownComplete` → window destroyed → process exit 0; the
//! audio graph stays up for the conferencing app).

use anyhow::{Context as _, Result};
use async_channel::Sender;
use gtk4::IconTheme;
use ksni::blocking::Handle;
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Category, ToolTip, Tray};

use super::worker::{Worker, WorkerCmd};

/// Convenience alias for the ksni service handle.
pub(crate) type TrayHandle = Handle<SoundboardTray>;

/// Commands from the ksni thread (menu / activate) to the GTK main loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrayCmd {
    /// Show the window if hidden, hide it (to the tray) if visible.
    ToggleWindow,
    /// Full graceful shutdown, then app exit.
    Quit,
}

/// The soundboard tray item state. Shared between the ksni thread (renders
/// `menu()`/`title()`/…) and the main loop (`Handle::update` closures).
pub(crate) struct SoundboardTray {
    icon_name: String,
    /// Last doctor status, mirrored into the passive menu line (set from
    /// the main loop via `Handle::update`).
    pub(crate) graph_ok: bool,
    cmd_tx: Sender<TrayCmd>,
    worker: Worker,
}

impl Tray for SoundboardTray {
    fn id(&self) -> String {
        "net.local.soundboard".to_string()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn title(&self) -> String {
        "Soundboard".to_string()
    }

    fn icon_name(&self) -> String {
        self.icon_name.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: self.icon_name.clone(),
            icon_pixmap: Vec::new(),
            title: "Soundboard".to_string(),
            description: "Виртуальный микрофон для звонков: клипы, громкость, хоткеи".to_string(),
        }
    }

    /// Left click (Plasma default): toggle the window.
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.cmd_tx.send_blocking(TrayCmd::ToggleWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            menu_item("Показать/Скрыть окно", {
                let cmd_tx = self.cmd_tx.clone();
                move |_| {
                    let _ = cmd_tx.send_blocking(TrayCmd::ToggleWindow);
                }
            }),
            MenuItem::Separator,
            menu_item("Стоп всё", {
                let worker = self.worker.clone();
                move |_| worker.send(WorkerCmd::Stop { name: None })
            }),
            StandardItem {
                label: graph_status_line(self.graph_ok),
                enabled: false,
                ..StandardItem::default()
            }
            .into(),
            MenuItem::Separator,
            menu_item("Выход", {
                let cmd_tx = self.cmd_tx.clone();
                move |_| {
                    let _ = cmd_tx.send_blocking(TrayCmd::Quit);
                }
            }),
        ]
    }
}

/// A standard enabled menu item with a label and an activate callback.
fn menu_item(
    label: &str,
    activate: impl Fn(&mut SoundboardTray) + Send + 'static,
) -> MenuItem<SoundboardTray> {
    StandardItem {
        label: label.to_string(),
        activate: Box::new(activate),
        ..StandardItem::default()
    }
    .into()
}

/// Passive menu line mirroring the window's doctor status.
fn graph_status_line(graph_ok: bool) -> String {
    if graph_ok {
        "Граф: OK".to_string()
    } else {
        "Граф: проблемы".to_string()
    }
}

/// First tray icon present in the current theme, else the primary name as
/// the sane fallback (the theme renders a generic audio glyph for missing
/// names, so the item never ends up blank).
#[must_use]
pub(crate) fn pick_icon_name(theme: &IconTheme) -> String {
    const CANDIDATES: [&str; 3] = ["audio-input-microphone", "audio-card", "audio-x-generic"];
    CANDIDATES
        .iter()
        .find(|name| theme.has_icon(name))
        .map_or_else(|| CANDIDATES[0].to_string(), |name| (*name).to_string())
}

/// Spawns the tray service on its own thread and returns the handle for
/// main-loop-side updates ([`Handle::update`]) and shutdown
/// ([`Handle::shutdown`]).
///
/// # Errors
/// When the D-Bus registration fails (e.g. no StatusNotifierWatcher —
/// without a tray shell the app simply runs windowed).
pub(crate) fn spawn(
    icon_name: String,
    cmd_tx: Sender<TrayCmd>,
    worker: Worker,
) -> Result<TrayHandle> {
    let tray = SoundboardTray {
        icon_name,
        graph_ok: true,
        cmd_tx,
        worker,
    };
    use ksni::blocking::TrayMethods as _;
    tray.spawn().context("не удалось создать значок в трее")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_status_line_matches_window_summary() {
        assert_eq!(graph_status_line(true), "Граф: OK");
        assert_eq!(graph_status_line(false), "Граф: проблемы");
    }
}

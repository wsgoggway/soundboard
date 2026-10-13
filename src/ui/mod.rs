//! GTK4/libadwaita user interface (M3).
//!
//! [`run`] boots the audio worker ([`worker`], the only owner of blocking
//! audio APIs), the system tray item ([`tray`], StatusNotifierItem via
//! ksni) and the libadwaita window ([`window`]); they talk via async
//! channels so the GTK main thread never blocks, and share one
//! `worker::SharedConfig` (UI mutates, worker snapshots — see the worker
//! module docs). [`kglobalaccel`] is session glue shared with the CLI: it
//! makes a running Plasma re-read the hotkey desktop files after they are
//! rewritten.

pub mod kglobalaccel;
pub mod tray;
pub mod window;
pub mod worker;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use gtk4::gio::prelude::{ApplicationExt, ApplicationExtManual};
use gtk4::glib;
use gtk4::prelude::GtkWindowExt as _;
use libadwaita as adw;

use worker::{SharedConfig, Worker};

/// Freedesktop icon name of the app (installed by `just install-desktop`
/// into hicolor as `org.soundboard.App.svg|png`; the GTK app id and the
/// desktop-file `StartupWMClass` use the same name).
pub(crate) const APP_ICON: &str = "org.soundboard.App";

/// Runs the GUI until the tray «Выход» (or a signal) performs the graceful
/// shutdown and the last window is destroyed. Closing the window only
/// hides it to the tray while the app keeps running.
///
/// # Errors
/// Fails on config load errors or when the app cannot spawn its worker.
pub fn run() -> Result<()> {
    let cfg = crate::config::Config::load().context("не удалось загрузить конфигурацию")?;
    let shared: SharedConfig = Arc::new(Mutex::new(cfg));
    let (ui_tx, ui_rx) = async_channel::unbounded::<worker::GuiMsg>();
    let (meter_tx, meter_rx) = async_channel::unbounded::<f32>();
    let worker = Worker::spawn(shared.clone(), ui_tx, meter_tx)?;

    let app = adw::Application::builder()
        .application_id("org.soundboard.App")
        .build();

    // Tray: spawned once in `startup` (before the first `activate`, display
    // already open so the theme icon can be resolved). Failure degrades to
    // windowed mode — then closing the window really quits again.
    let (tray_tx, tray_rx) = async_channel::unbounded::<tray::TrayCmd>();
    let tray: Rc<RefCell<Option<tray::TrayHandle>>> = Rc::new(RefCell::new(None));
    {
        let worker = worker.clone();
        let tray_tx = tray_tx.clone();
        let tray_slot = tray.clone();
        app.connect_startup(move |_app| {
            if tray_slot.borrow().is_some() {
                return; // startup runs once per process, but stay idempotent
            }
            let icon = gtk4::gdk::Display::default()
                .map(|display| tray::pick_icon_name(&gtk4::IconTheme::for_display(&display)))
                .unwrap_or_else(|| "audio-input-microphone".to_string());
            match tray::spawn(icon, tray_tx.clone(), worker.clone()) {
                Ok(handle) => *tray_slot.borrow_mut() = Some(handle),
                Err(e) => eprintln!("предупреждение: {e:#}"),
            }
        });
    }

    // One window per process: a second `soundboard` launch (gio already
    // makes the app single-instance via its D-Bus id) just presents — and
    // thereby un-hides — the existing window instead of building another.
    let window_slot: Rc<RefCell<Option<adw::ApplicationWindow>>> = Rc::new(RefCell::new(None));
    {
        let ui_rx = ui_rx.clone();
        let meter_rx = meter_rx.clone();
        let worker = worker.clone();
        let shared = shared.clone();
        let tray = tray.clone();
        let tray_rx = tray_rx.clone();
        let window_slot = window_slot.clone();
        app.connect_activate(move |app| {
            let mut slot = window_slot.borrow_mut();
            if let Some(window) = slot.as_ref() {
                window.present();
            } else {
                let window = window::build(
                    app,
                    ui_rx.clone(),
                    meter_rx.clone(),
                    &worker,
                    &shared,
                    tray.clone(),
                    tray_rx.clone(),
                );
                *slot = Some(window);
            }
        });
    }
    if app.run() == glib::ExitCode::SUCCESS {
        Ok(())
    } else {
        anyhow::bail!("приложение завершилось с ошибкой")
    }
}

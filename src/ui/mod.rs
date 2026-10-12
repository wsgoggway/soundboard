//! GTK4/libadwaita user interface (M3).
//!
//! [`run`] boots the audio worker ([`worker`], the only owner of blocking
//! audio APIs) and the libadwaita window ([`window`]); the two talk via
//! async channels so the GTK main thread never blocks, and share one
//! `worker::SharedConfig` (UI mutates, worker snapshots — see the worker
//! module docs).

pub mod kglobalaccel;
pub mod window;
pub mod worker;

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use gtk4::gio::prelude::{ApplicationExt, ApplicationExtManual};
use gtk4::glib;
use libadwaita as adw;

use worker::{SharedConfig, Worker};

/// Runs the GUI until the window closes (the audio graph is left running).
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
    app.connect_activate(move |app| {
        // Receivers are Clone; connect_activate may be called more than once
        // per gio contract, each window gets its own receiving ends.
        window::build(app, ui_rx.clone(), meter_rx.clone(), &worker, &shared);
    });
    if app.run() == glib::ExitCode::SUCCESS {
        Ok(())
    } else {
        anyhow::bail!("приложение завершилось с ошибкой")
    }
}

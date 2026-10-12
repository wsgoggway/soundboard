//! Dedicated audio worker thread for the GUI.
//!
//! ALL audio/graph APIs are blocking (a `play()` probe sleeps up to 300 ms,
//! `stop()` up to 2 s, every `PulseSession` call pumps the libpulse
//! mainloop) — none of them may run on the GTK main thread. This module owns
//! the solution: one worker thread owns the `PulseSession` + `Graph` +
//! `PwCatPlayer` (and starts the `MeterHandle`, which spawns its own capture
//! thread). The UI talks to it via a `std::sync::mpsc` command channel and
//! listens back over async channels (`async_channel`, sent with
//! `send_blocking`, drained on the main loop with `spawn_future_local` — the
//! current gtk4-rs book pattern; `glib::MainContext::channel` was removed in
//! glib 0.22).
//!
//! Startup auto-setup (core requirement): right after spawn the worker runs
//! `EnsureGraph` and then `Doctor`, so the graph comes up without user
//! action and the status rows get their first report.
//!
//! Shutdown protocol: `WorkerCmd::Shutdown` → the worker stops the meter,
//! stops all playbacks and exits its loop; the graph is deliberately NOT
//! torn down (it must persist for Zoom; only `soundboard down` removes it).
//! A final `GuiMsg::ShutdownComplete` tells the window it may destroy
//! itself.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

use crate::config::Config;
use crate::graph::{DoctorReport, Graph, PulseSession, Report};
use crate::meter::MeterHandle;
use crate::player::{Player as _, PwCatPlayer};

/// Command from the UI thread to the audio worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerCmd {
    /// Reconcile the audio graph (startup auto-setup, «Починить граф»).
    EnsureGraph,
    /// Run the diagnostics and send the report back.
    Doctor,
    /// Play a 2 s 440 Hz test sine into the `soundboard` sink.
    PlayTest,
    /// Stop the meter and all playbacks, then exit (graph stays up).
    Shutdown,
}

/// Message from the worker to the UI thread. (Meter peaks do NOT travel
/// here: they stream at ~30 Hz over the dedicated `f32` channel handed to
/// the worker at spawn, so slow blocking worker operations can never delay
/// the VU bar.)
#[derive(Debug)]
pub enum GuiMsg {
    /// Fresh diagnostics: one status row per check.
    Status(DoctorReport),
    /// Graph reconcile finished; `is_noop()`/`warnings` drive the summary.
    GraphUp(Report),
    /// A blocking audio operation failed (shown once, UI stays usable).
    Error(String),
    /// The worker finished its shutdown; the window may destroy itself.
    ShutdownComplete,
}

/// 440 Hz test sine parameters (recipe-verified 48 kHz mono 16-bit wav).
const TEST_RATE: u32 = 48_000;
const TEST_FREQ: f32 = 440.0;
const TEST_SECS: u32 = 2;
const TEST_AMPLITUDE: f32 = 0.5;

/// PCM samples of the test sine (16-bit little-endian mono).
fn test_sine_samples() -> Vec<i16> {
    let count = (TEST_RATE * TEST_SECS) as usize;
    (0..count)
        .map(|i| {
            let s = TEST_AMPLITUDE
                * (std::f32::consts::TAU * TEST_FREQ * i as f32 / TEST_RATE as f32).sin();
            (s * i16::MAX as f32) as i16
        })
        .collect()
}

/// Wraps `samples` into a canonical 44-byte-header PCM wav file.
fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16_u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1_u16.to_le_bytes()); // mono
    out.extend_from_slice(&TEST_RATE.to_le_bytes());
    out.extend_from_slice(&(TEST_RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2_u16.to_le_bytes()); // block align
    out.extend_from_slice(&16_u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Path of the generated test file (rewritten on every test playback).
fn test_sine_path() -> PathBuf {
    std::env::temp_dir().join("soundboard-test-440hz.wav")
}

fn write_test_sine() -> Result<PathBuf> {
    let path = test_sine_path();
    std::fs::write(&path, wav_bytes(&test_sine_samples()))
        .with_context(|| format!("не удалось записать {}", path.display()))?;
    Ok(path)
}

/// Owner of everything audio; one instance per app, one thread.
/// Cheap to clone (a channel sender inside).
#[derive(Clone)]
pub struct Worker {
    cmd_tx: std::sync::mpsc::Sender<WorkerCmd>,
}

impl Worker {
    /// Spawns the worker thread. It immediately reconciles the graph, runs
    /// the doctor and starts the meter (auto-setup at launch), then waits
    /// for commands.
    ///
    /// # Errors
    /// Only fails when the thread cannot be spawned at all; audio/graph
    /// problems are reported asynchronously as [`GuiMsg::Error`].
    pub fn spawn(
        cfg: Config,
        ui_tx: async_channel::Sender<GuiMsg>,
        meter_tx: async_channel::Sender<f32>,
    ) -> Result<Worker> {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<WorkerCmd>();
        let ui = move |msg: GuiMsg| {
            let _ = ui_tx.send_blocking(msg); // UI gone: nothing to report to
        };
        let start_up = std::thread::Builder::new()
            .name("soundboard-worker".to_string())
            .spawn(move || run_worker(cfg, cmd_rx, ui, meter_tx))
            .context("не удалось запустить рабочий поток")?;
        // Dropping the handle detaches the thread; it exits on Shutdown or
        // when this (the app's) side drops the command sender.
        drop(start_up);
        Ok(Worker { cmd_tx })
    }

    /// Sends a command; a dead worker (app shutting down) is not an error.
    pub fn send(&self, cmd: WorkerCmd) {
        let _ = self.cmd_tx.send(cmd);
    }
}

/// Worker thread body: owns the session, graph, player and meter.
fn run_worker(
    cfg: Config,
    cmd_rx: std::sync::mpsc::Receiver<WorkerCmd>,
    ui: impl Fn(GuiMsg),
    meter_tx: async_channel::Sender<f32>,
) {
    let ui_err = |e: anyhow::Error| ui(GuiMsg::Error(format!("{e:#}")));

    // Startup auto-setup: reconcile, then report, then meter.
    let mut graph = match open_graph() {
        Ok(graph) => graph,
        Err(e) => {
            ui_err(e);
            // Nothing else can work without a session; still serve Doctor
            // retries below by re-opening on each command.
            wait_for_commands_without_session(cfg, cmd_rx, ui);
            return;
        }
    };
    match graph.reconcile(&cfg) {
        Ok(report) => ui(GuiMsg::GraphUp(report)),
        Err(e) => ui_err(e),
    }
    ui(GuiMsg::Status(graph.doctor(&cfg)));
    let mut meter = start_meter(&ui, &meter_tx);

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            WorkerCmd::EnsureGraph => {
                match graph.reconcile(&cfg) {
                    Ok(report) => ui(GuiMsg::GraphUp(report)),
                    Err(e) => ui_err(e),
                }
                ui(GuiMsg::Status(graph.doctor(&cfg)));
                if meter.is_none() {
                    meter = start_meter(&ui, &meter_tx);
                }
            }
            WorkerCmd::Doctor => {
                ui(GuiMsg::Status(graph.doctor(&cfg)));
            }
            WorkerCmd::PlayTest => {
                let mut player = PwCatPlayer::new();
                match write_test_sine()
                    .and_then(|path| player.play(&path, "Тест 440 Гц", cfg.volume))
                {
                    Ok(_handle) => {}
                    Err(e) => ui_err(e),
                }
            }
            WorkerCmd::Shutdown => break,
        }
    }
    meter_stop(&mut meter);
    ui(GuiMsg::ShutdownComplete);
}

fn meter_stop(meter: &mut Option<MeterHandle>) {
    if let Some(m) = meter.as_mut() {
        m.stop();
    }
}

/// Connects to the pulse server and builds the graph facade (same recipe as
/// the CLI entry point).
fn open_graph() -> Result<Graph<PulseSession>> {
    let session = PulseSession::connect("soundboard")?;
    let state_path = crate::graph::default_state_path()?;
    Ok(Graph::new(session, state_path))
}

/// Starts the meter against the virtual mic monitor; failures are surfaced
/// once and the meter stays stopped (returns `None`).
fn start_meter(ui: &impl Fn(GuiMsg), meter_tx: &async_channel::Sender<f32>) -> Option<MeterHandle> {
    match MeterHandle::start(crate::graph::SOURCE_VIRTMIC_MONITOR, meter_tx.clone()) {
        Ok(handle) => Some(handle),
        Err(e) => {
            ui(GuiMsg::Error(format!("{e:#}")));
            None
        }
    }
}

/// Degraded mode when the pulse session could not be opened at startup:
/// `EnsureGraph`/`Doctor` re-open a session per command, `PlayTest` is
/// impossible without a server.
fn wait_for_commands_without_session(
    cfg: Config,
    cmd_rx: std::sync::mpsc::Receiver<WorkerCmd>,
    ui: impl Fn(GuiMsg),
) {
    let ui_err = |e: anyhow::Error| ui(GuiMsg::Error(format!("{e:#}")));
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            WorkerCmd::EnsureGraph | WorkerCmd::Doctor => match open_graph() {
                Ok(mut graph) => {
                    if cmd == WorkerCmd::EnsureGraph {
                        match graph.reconcile(&cfg) {
                            Ok(report) => ui(GuiMsg::GraphUp(report)),
                            Err(e) => ui_err(e),
                        }
                    }
                    ui(GuiMsg::Status(graph.doctor(&cfg)));
                }
                Err(e) => ui_err(e),
            },
            WorkerCmd::PlayTest => {
                ui_err(anyhow::anyhow!(
                    "нет связи с pulse-сервером — тест невозможен"
                ));
            }
            WorkerCmd::Shutdown => break,
        }
    }
    ui(GuiMsg::ShutdownComplete);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sine_has_two_seconds_of_440hz_mono() {
        let samples = test_sine_samples();
        assert_eq!(samples.len(), (TEST_RATE * TEST_SECS) as usize);
        // Peak amplitude matches the configured level (float rounding ok).
        let peak = samples
            .iter()
            .fold(0_i32, |m, s| m.max(i16::abs(*s) as i32));
        assert!(peak > (0.49 * i16::MAX as f32) as i32);
        assert!(peak <= i16::MAX as i32);
        // Zero crossings: a 440 Hz sine over 2 s has ~1760 of them.
        let crossings = samples
            .windows(2)
            .filter(|w| (w[0] < 0 && w[1] >= 0) || (w[0] >= 0 && w[1] < 0))
            .count();
        assert!(
            (1750..=1770).contains(&crossings),
            "unexpected zero crossings: {crossings}"
        );
    }

    #[test]
    fn wav_header_is_canonical_pcm() {
        let bytes = wav_bytes(&[0_i16, 100, -100]);
        assert_eq!(bytes.len(), 44 + 6);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        // PCM, mono, 48 kHz, 16-bit.
        assert_eq!(&bytes[20..22], &1_u16.to_le_bytes());
        assert_eq!(&bytes[22..24], &1_u16.to_le_bytes());
        assert_eq!(&bytes[24..28], &48_000_u32.to_le_bytes());
        assert_eq!(&bytes[34..36], &16_u16.to_le_bytes());
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(&bytes[40..44], &6_u32.to_le_bytes());
        assert_eq!(&bytes[44..46], &0_i16.to_le_bytes());
    }

    #[test]
    fn test_sine_path_is_in_temp_dir() {
        assert!(test_sine_path().starts_with(std::env::temp_dir()));
    }
}

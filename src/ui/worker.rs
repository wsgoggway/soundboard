//! Dedicated audio worker thread for the GUI.
//!
//! ALL audio/graph APIs are blocking (a `play()` probe sleeps up to 300 ms,
//! `stop()` up to 2 s, every `PulseSession` call pumps the libpulse
//! mainloop) — none of them may run on the GTK main thread. This module owns
//! the solution: one worker thread owns the `PulseSession` + `Graph` +
//! `PwCatPlayer` + clip `Scheduler` (and starts the `MeterHandle`, which
//! spawns its own capture thread). The UI talks to it via a
//! `std::sync::mpsc` command channel and listens back over async channels
//! (`async_channel`, sent with `send_blocking`, drained on the main loop
//! with `spawn_future_local` — the current gtk4-rs book pattern;
//! `glib::MainContext::channel` was removed in glib 0.22).
//!
//! Config sharing: the UI and the worker hold one [`SharedConfig`]. The UI
//! mutates fields under the lock (cheap, in-memory); the worker takes a
//! `clone()` snapshot before every blocking operation, so the UI thread can
//! never block on audio. Persistence (`Config::save`, a small file write)
//! happens only on the worker thread (`WorkerCmd::SaveConfig` or shutdown).
//!
//! Playback engine (M3b): the worker owns a persistent [`PwCatPlayer`] and a
//! [`Scheduler`]. `Play { name }` resolves the clip against a fresh
//! [`Clips::scan`] (a cheap readdir — keeps the registry current without a
//! rescan command; new files are playable immediately), asks the scheduler
//! for a decision (per-clip interrupt semantics come from
//! `config.interrupt_mode`), kills the interrupted handles, starts the new
//! playback with the current `config.volume` and registers it. Finished
//! playbacks are reaped after every command AND on a [`REAP_INTERVAL`]
//! timeout while idle (so a clip that ends on its own un-highlights without
//! user action); every change publishes [`GuiMsg::PlayingSet`] — the full
//! set of playing clip names (scheduler-tracked clips only; CLI-spawned
//! `soundboard play` processes and the 440 Hz test tone are not in the
//! registry and are ignored by the UI grid, but «Стоп всё» still stops the
//! scheduler-tracked ones). «Стоп всё» (`Stop { name: None }`) stops only
//! playbacks this GUI spawned — cross-process stopping stays a CLI feature
//! (`soundboard stop`) to keep parallel CLI+GUI use predictable.
//!
//! Startup auto-setup (core requirement): right after spawn the worker runs
//! `EnsureGraph` and then `Doctor`, so the graph comes up without user
//! action and the status rows get their first report.
//!
//! Meter lifecycle: the meter starts once at startup and is RESTARTED on
//! every successful `EnsureGraph` (unconditionally, not only when it was
//! never started) — this revives both a meter that failed at startup and one
//! whose source vanished mid-run (the pump thread exits silently then; see
//! the `meter` module docs for the frozen-value failure model).
//!
//! Shutdown protocol: `WorkerCmd::Shutdown` → the worker persists the
//! config (the volume slider may have moved), stops the meter and all
//! playbacks and exits its loop; the graph is deliberately NOT torn down
//! (it must persist for Zoom; only `soundboard down` removes it). A final
//! `GuiMsg::ShutdownComplete` tells the window it may destroy itself.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::clips::Clips;
use crate::config::Config;
use crate::graph::{DoctorReport, Graph, PulseSession, PwTooling, Report};
use crate::meter::MeterHandle;
use crate::player::{Player as _, PwCatPlayer};
use crate::scheduler::{Decision, Handle, PlayRequest, Request, Scheduler, StopRequest};

/// Config shared between the UI thread and the audio worker (see the module
/// docs for the locking/snapshotting discipline).
pub type SharedConfig = Arc<Mutex<Config>>;

/// Locks the shared config, healing a poisoned lock: a panicked worker must
/// not take the whole UI down, and the config is a plain data struct whose
/// mid-update state is harmless.
pub fn lock_config(cfg: &SharedConfig) -> MutexGuard<'_, Config> {
    cfg.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How often the idle worker reaps finished playbacks to keep the UI
/// playing-set current.
const REAP_INTERVAL: Duration = Duration::from_millis(250);

/// Registry name under which the test tone is tracked in the scheduler (not
/// a real clip; the UI grid ignores it, «Стоп всё» stops it).
const TEST_NAME: &str = "Тест 440 Гц";

/// Command from the UI thread to the audio worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerCmd {
    /// Reconcile the audio graph (startup auto-setup, «Починить граф»,
    /// mic-mix/monitoring toggles). On success the meter is restarted too.
    EnsureGraph,
    /// Run the diagnostics and send the report back.
    Doctor,
    /// Play a 2 s 440 Hz test sine into the `soundboard` sink.
    PlayTest,
    /// Play the clip by registry name (file stem); interrupt semantics and
    /// volume come from the current shared config.
    Play {
        /// Registry name of the clip.
        name: String,
    },
    /// Stop one clip (`name: Some`) or every GUI-spawned playback
    /// (`name: None` = «Стоп всё»).
    Stop {
        /// Clip to stop; `None` = stop all.
        name: Option<String>,
    },
    /// Persist the current shared config to disk (worker thread does the
    /// blocking write; used by the toggles and the volume slider).
    SaveConfig,
    /// Stop the meter and all playbacks, persist the config, then exit (the
    /// graph stays up).
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
    /// Names of the clips currently playing (the UI highlights them in the
    /// grid). Published whenever the set changes.
    PlayingSet(Vec<String>),
    /// A blocking audio operation failed (toast + summary; UI stays usable).
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

/// How many names `create_unique_wav` tries before giving up.
const UNIQUE_CREATE_ATTEMPTS: u32 = 3;

/// Base directory for the generated test wav: the user's runtime directory
/// (`XDG_RUNTIME_DIR` — per-user, 0700 on systemd) when set and non-empty,
/// else the shared temp dir as the fallback.
fn test_runtime_dir() -> PathBuf {
    test_runtime_dir_from(std::env::var_os("XDG_RUNTIME_DIR").as_deref())
}

/// [`test_runtime_dir`] with an injectable env value (unit-testable).
fn test_runtime_dir_from(xdg_runtime_dir: Option<&OsStr>) -> PathBuf {
    match xdg_runtime_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir(),
    }
}

/// Writes the test wav under a unique, freshly created file name and returns
/// its path. The caller must remove the file once the playback attempt is
/// over (see `test_files` in `run_worker`).
fn write_test_sine() -> Result<PathBuf> {
    create_unique_wav(test_runtime_dir(), wav_bytes(&test_sine_samples()))
        .context("не удалось записать тестовый wav-файл")
}

/// Creates `dir/soundboard-test-440hz-<pid>-<nanos>.wav` exclusively
/// (`create_new`) and fills it with `bytes`, mirroring the
/// `player::open_stderr_log` pattern: the name is unique per process and
/// attempt, so two GUI instances never race and a pre-existing symlink at
/// the path is never followed — the call fails instead and a bumped
/// nanosecond suffix is tried. The bytes go through the very handle the
/// call created, never through a second open of the path.
fn create_unique_wav(dir: PathBuf, bytes: Vec<u8>) -> std::io::Result<PathBuf> {
    for attempt in 0..UNIQUE_CREATE_ATTEMPTS {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() + attempt)
            .unwrap_or(attempt);
        let path = dir.join(format!(
            "soundboard-test-440hz-{}-{nanos}.wav",
            std::process::id()
        ));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        file.write_all(&bytes)?;
        return Ok(path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no unique test-wav name after retries",
    ))
}

/// Drops the wav of a finished/stopped test playback (best effort: a leftover
/// in the user's runtime dir is cosmetic, not a failure).
fn remove_test_file(test_files: &mut HashMap<Handle, PathBuf>, handle: Handle) {
    if let Some(path) = test_files.remove(&handle) {
        let _ = std::fs::remove_file(path);
    }
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
        cfg: SharedConfig,
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

/// Worker thread body: owns the session, graph, player, scheduler and meter.
fn run_worker(
    cfg: SharedConfig,
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
            // Nothing else can work without a session; still serve retries
            // below by re-opening on each command.
            wait_for_commands_without_session(cfg, cmd_rx, ui, meter_tx);
            return;
        }
    };
    let snapshot = lock_config(&cfg).clone();
    match graph.reconcile(&snapshot) {
        Ok(report) => ui(GuiMsg::GraphUp(report)),
        Err(e) => ui_err(e),
    }
    ui(GuiMsg::Status(graph.doctor(&snapshot)));
    let mut meter = start_meter(&ui, &meter_tx);

    let mut player = PwCatPlayer::new();
    let mut scheduler = Scheduler::new();
    let mut published: Vec<String> = Vec::new();
    // Wav files of live test playbacks (handle -> path); removed as soon as
    // the corresponding playback is reaped, stopped or the worker exits —
    // the file must outlive the pw-cat process reading it.
    let mut test_files: HashMap<Handle, PathBuf> = HashMap::new();

    loop {
        match cmd_rx.recv_timeout(REAP_INTERVAL) {
            Ok(cmd) => match cmd {
                WorkerCmd::EnsureGraph => {
                    let snapshot = lock_config(&cfg).clone();
                    let outcome = graph.reconcile(&snapshot);
                    let reconcile_ok = outcome.is_ok();
                    match outcome {
                        Ok(report) => ui(GuiMsg::GraphUp(report)),
                        Err(e) => ui_err(e),
                    }
                    ui(GuiMsg::Status(graph.doctor(&snapshot)));
                    // Restart the meter on every SUCCESSFUL repair, even when
                    // it was already running or had failed earlier: a fresh
                    // capture stream heals both a source that vanished
                    // mid-run (the old pump thread exits silently, the bar
                    // would freeze) and a meter that never started.
                    if reconcile_ok {
                        meter_stop(&mut meter);
                        meter = start_meter(&ui, &meter_tx);
                    }
                }
                WorkerCmd::Doctor => {
                    let snapshot = lock_config(&cfg).clone();
                    ui(GuiMsg::Status(graph.doctor(&snapshot)));
                }
                WorkerCmd::PlayTest => {
                    let (volume, interrupt) = play_params(&cfg);
                    match write_test_sine() {
                        Ok(path) => match play_clip(
                            &mut player,
                            &mut scheduler,
                            TEST_NAME,
                            &path,
                            volume,
                            interrupt,
                            &ui_err,
                        ) {
                            // Cleanup happens when the playback is reaped or
                            // stopped; a failed spawn never started reading
                            // the file, so remove it right away.
                            Some(handle) => {
                                test_files.insert(handle, path);
                            }
                            None => {
                                let _ = std::fs::remove_file(&path);
                            }
                        },
                        Err(e) => ui_err(e),
                    }
                    publish_playing(
                        &mut player,
                        &mut scheduler,
                        &mut published,
                        &mut test_files,
                        &ui,
                    );
                }
                WorkerCmd::Play { name } => {
                    let (volume, interrupt) = play_params(&cfg);
                    // Fresh scan per play: a cheap readdir keeps the registry
                    // current, so clips added on disk are playable at once.
                    let clips = Clips::scan(&lock_config(&cfg).clips_dir);
                    match clips.resolve(&name) {
                        Ok(clip) => {
                            play_clip(
                                &mut player,
                                &mut scheduler,
                                clip.name,
                                clip.path,
                                volume,
                                interrupt,
                                &ui_err,
                            );
                        }
                        Err(e) => ui_err(anyhow::anyhow!("{e}")),
                    }
                    publish_playing(
                        &mut player,
                        &mut scheduler,
                        &mut published,
                        &mut test_files,
                        &ui,
                    );
                }
                WorkerCmd::Stop { name } => {
                    let decision = scheduler.decide(Request::Stop(StopRequest { name }));
                    if let Decision::Stop(handles) = decision {
                        for handle in handles {
                            if let Err(e) = player.stop(handle) {
                                ui_err(e);
                            }
                            remove_test_file(&mut test_files, handle);
                        }
                    }
                    publish_playing(
                        &mut player,
                        &mut scheduler,
                        &mut published,
                        &mut test_files,
                        &ui,
                    );
                }
                WorkerCmd::SaveConfig => {
                    if let Err(e) = lock_config(&cfg).save() {
                        ui_err(e);
                    }
                }
                WorkerCmd::Shutdown => break,
            },
            // Idle: keep the playing set honest (clips end on their own).
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                publish_playing(
                    &mut player,
                    &mut scheduler,
                    &mut published,
                    &mut test_files,
                    &ui,
                );
            }
            // The app dropped the command sender: shut down the same way.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Persist the config (the volume slider may have moved since the last
    // explicit save), then stop all audio. The graph stays up for Zoom.
    if let Err(e) = lock_config(&cfg).save() {
        ui_err(e);
    }
    meter_stop(&mut meter);
    if let Err(e) = player.stop_all() {
        ui_err(e);
    }
    // Nothing we spawned is alive any more: no test wav may outlive the app.
    for (_, path) in test_files.drain() {
        let _ = std::fs::remove_file(path);
    }
    ui(GuiMsg::ShutdownComplete);
}

/// Snapshot of the config fields a playback needs.
fn play_params(cfg: &SharedConfig) -> (f32, bool) {
    let snapshot = lock_config(cfg);
    (snapshot.volume, snapshot.interrupt_mode)
}

/// Runs one play request through the scheduler: kills the interrupted
/// handles, starts the new playback at `volume` and registers it. A failed
/// stop of an old handle is surfaced but does not cancel the fresh play.
/// Returns the registered handle, or `None` when the spawn failed (nothing
/// was registered).
fn play_clip(
    player: &mut PwCatPlayer,
    scheduler: &mut Scheduler,
    name: &str,
    path: &Path,
    volume: f32,
    interrupt_mode: bool,
    ui_err: &impl Fn(anyhow::Error),
) -> Option<Handle> {
    let Decision::Start { stop } = scheduler.decide(Request::Play(PlayRequest {
        name: name.to_string(),
        interrupt_mode,
    })) else {
        return None; // play decisions are always Start
    };
    for handle in stop {
        if let Err(e) = player.stop(handle) {
            ui_err(e);
        }
    }
    match player.play(path, name, volume) {
        // On spawn failure there is nothing to register: `decide` already
        // dropped the interrupted handles, so the scheduler stays honest.
        Ok(handle) => {
            scheduler.started(name, handle);
            Some(handle)
        }
        Err(e) => {
            ui_err(e);
            None
        }
    }
}

/// Reaps finished playbacks, updates the scheduler, removes the wav of a
/// finished test playback and publishes the playing set when it changed.
fn publish_playing(
    player: &mut PwCatPlayer,
    scheduler: &mut Scheduler,
    published: &mut Vec<String>,
    test_files: &mut HashMap<Handle, PathBuf>,
    ui: &impl Fn(GuiMsg),
) {
    for handle in player.reap() {
        scheduler.finished(handle);
        remove_test_file(test_files, handle);
    }
    let playing: Vec<String> = scheduler
        .active()
        .into_iter()
        .map(|(_, name)| name.to_string())
        .collect();
    if playing != *published {
        *published = playing.clone();
        ui(GuiMsg::PlayingSet(playing));
    }
}

fn meter_stop(meter: &mut Option<MeterHandle>) {
    if let Some(m) = meter.as_mut() {
        m.stop();
    }
}

/// Connects to the pulse server, builds the graph facade (pulse + native
/// pw-cli/pw-link tooling; same recipe as the CLI entry point).
fn open_graph() -> Result<Graph<PulseSession, PwTooling>> {
    let session = PulseSession::connect("soundboard")?;
    let state_path = crate::graph::default_state_path()?;
    Ok(Graph::new(session, PwTooling::system(), state_path))
}

/// Starts the meter against the virtual mic source (the native
/// Audio/Source/Virtual node, pulse alias `virtmic`); failures are surfaced
/// once and the meter stays stopped (returns `None`).
fn start_meter(ui: &impl Fn(GuiMsg), meter_tx: &async_channel::Sender<f32>) -> Option<MeterHandle> {
    match MeterHandle::start(crate::graph::SOURCE_VIRTMIC, meter_tx.clone()) {
        Ok(handle) => Some(handle),
        Err(e) => {
            ui(GuiMsg::Error(format!("{e:#}")));
            None
        }
    }
}

/// Degraded mode when the pulse session could not be opened at startup:
/// `EnsureGraph`/`Doctor` re-open a session per command, playback is
/// impossible without a server.
fn wait_for_commands_without_session(
    cfg: SharedConfig,
    cmd_rx: std::sync::mpsc::Receiver<WorkerCmd>,
    ui: impl Fn(GuiMsg),
    meter_tx: async_channel::Sender<f32>,
) {
    let ui_err = |e: anyhow::Error| ui(GuiMsg::Error(format!("{e:#}")));
    let mut meter: Option<MeterHandle> = None;
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            WorkerCmd::EnsureGraph | WorkerCmd::Doctor => match open_graph() {
                Ok(mut graph) => {
                    let snapshot = lock_config(&cfg).clone();
                    let outcome = if cmd == WorkerCmd::EnsureGraph {
                        match graph.reconcile(&snapshot) {
                            Ok(report) => {
                                ui(GuiMsg::GraphUp(report));
                                true
                            }
                            Err(e) => {
                                ui_err(e);
                                false
                            }
                        }
                    } else {
                        false
                    };
                    ui(GuiMsg::Status(graph.doctor(&snapshot)));
                    if outcome {
                        // Same restart-on-repair policy as the healthy loop.
                        meter_stop(&mut meter);
                        meter = start_meter(&ui, &meter_tx);
                    }
                }
                Err(e) => ui_err(e),
            },
            WorkerCmd::PlayTest => {
                ui_err(anyhow::anyhow!(
                    "нет связи с pulse-сервером — тест невозможен"
                ));
            }
            WorkerCmd::Play { name } => {
                ui_err(anyhow::anyhow!(
                    "нет связи с pulse-сервером — «{name}» не воспроизведён"
                ));
            }
            // Degraded mode never spawned anything: nothing to stop.
            WorkerCmd::Stop { .. } => {}
            WorkerCmd::SaveConfig => {
                if let Err(e) = lock_config(&cfg).save() {
                    ui_err(e);
                }
            }
            WorkerCmd::Shutdown => break,
        }
    }
    if let Err(e) = lock_config(&cfg).save() {
        ui_err(e);
    }
    meter_stop(&mut meter);
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
    fn test_runtime_dir_prefers_xdg_and_falls_back_to_temp() {
        let xdg = OsStr::new("/run/user/1000");
        assert_eq!(
            test_runtime_dir_from(Some(xdg)),
            PathBuf::from("/run/user/1000")
        );
        // Empty value is treated as unset.
        assert_eq!(
            test_runtime_dir_from(Some(OsStr::new(""))),
            std::env::temp_dir()
        );
        assert_eq!(test_runtime_dir_from(None), std::env::temp_dir());
    }

    #[test]
    fn unique_wav_names_never_collide_or_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes = wav_bytes(&[1_i16, 2, 3]);
        let first =
            create_unique_wav(dir.path().to_path_buf(), bytes.clone()).expect("first write");
        let second = create_unique_wav(dir.path().to_path_buf(), bytes).expect("second write");
        // Distinct names, both exist, neither clobbered the other.
        assert_ne!(first, second, "two writes must not share a file name");
        assert!(first.is_file());
        assert!(second.is_file());
        let expected = wav_bytes(&[1_i16, 2, 3]);
        assert_eq!(std::fs::read(&first).expect("read first"), expected);
        assert_eq!(std::fs::read(&second).expect("read second"), expected);
    }

    #[test]
    fn write_test_sine_lands_in_runtime_dir_with_wav_content() {
        let base = test_runtime_dir();
        let a = write_test_sine().expect("write a");
        let b = write_test_sine().expect("write b");
        assert_ne!(a, b);
        assert!(a.starts_with(&base));
        assert!(b.starts_with(&base));
        let data = std::fs::read(&a).expect("read wav");
        assert_eq!(data, wav_bytes(&test_sine_samples()));
        // Leave no test artifacts behind.
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }
}

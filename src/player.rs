//! Clip playback via the `pw-cat` subprocess.
//!
//! Playback shape (contract `docs/pipewire-recipe.md`, target constant
//! [`crate::graph::SINK_MEMES`]):
//!
//! ```text
//! pw-cat --playback --target soundboard --volume <v> <file>
//! ```
//!
//! Flag choice verified against `pw-cat --help` on this host (PipeWire 1.6.9):
//! `--volume` IS documented ("Stream volume 0-1.0"), so it is passed
//! explicitly, clamped to `0.0..=1.0`.
//!
//! Process management:
//! - children start in their own process group (`process_group(0)`);
//! - [`Player::stop`] sends SIGTERM to the group, waits a grace period, then
//!   SIGKILLs and reaps (std's `Child::kill` is SIGKILL-only, hence the libc
//!   dependency); stopping an unknown handle is a no-op;
//! - every API call first reaps exited children with `try_wait` (no zombies);
//! - a child's stderr goes to a log file in the temp dir — never a pipe, so a
//!   long-playing child can neither block on a full pipe nor die of SIGPIPE;
//! - to surface decode errors without blocking, [`Player::play`] waits a
//!   short probe window (default 300 ms; decode failures exit within
//!   milliseconds): a child that already exited non-zero is reported as an
//!   error with the first stderr line; a still-running child is success, and
//!   its (healthy, silent) stderr log is deleted right away.
//!
//! Transcode fallback ([`Transcoder`]): real-world clips are sometimes
//! ogg-opus files libsndfile (the `pw-cat` decoder) refuses as malformed —
//! e.g. files muxed by old ffmpeg whose first audio page granule position is
//! `48000 - pre-skip`, less than the samples completing on that page (see
//! `docs/pipewire-recipe.md`). When the FIRST spawn dies within the probe
//! window and `ffmpeg` is available, the clip is transcoded once to a
//! canonical 48 kHz stereo wav under `~/.cache/soundboard/transcoded/`
//! (`<stem>-<mtime+size hash>.wav`, argv array, no shell) and the cached wav
//! is played instead. The cache key is the source mtime + size, so an
//! edited clip is re-transcoded; a failed transcode (or a missing ffmpeg)
//! surfaces the ORIGINAL decode error — never a crash, never a silent
//! success. The decision logic (cache key, needs-transcode, candidates) is
//! pure and unit-tested with an injected fake ffmpeg.
//!
//! Cross-process stop (CLI): `soundboard play` exits right after spawning, so
//! a later `soundboard stop` finds the orphaned `pw-cat`s by scanning
//! `/proc/*/cmdline` for our exact argument shape (`--playback`, `--target
//! soundboard`), optionally filtered by clip path, and signals them. A
//! manually started `pw-cat --playback --target soundboard` matches too — by
//! design: anything playing into our virtual mic is ours to stop.
//!
//! The v1 CLI consumes only [`Player::play`] and the cross-process helpers;
//! the rest of the [`Player`] surface (stop/stop_all/active/reap, test
//! constructors) is exercised by the unit/integration tests today and by the
//! M3 GUI afterwards, hence the module-level `allow(dead_code)` until then.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

use crate::graph::SINK_MEMES;
use crate::scheduler::Handle;

/// Player binary resolved from `PATH`.
const PW_CAT: &str = "pw-cat";
/// ffmpeg binary resolved from `PATH` for the transcode fallback.
const FFMPEG: &str = "ffmpeg";
/// How long `play` watches for an immediate (decode) failure.
const PROBE_WINDOW: Duration = Duration::from_millis(300);
/// Grace period for SIGTERM before escalating to SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(2);
/// Poll step for process-state checks.
const POLL_STEP: Duration = Duration::from_millis(10);

/// Outcome of the probe window: `Ok(handle)` while playing, `Err(error)`
/// when the child died immediately (the decode-failure signal).
type ProbeOutcome = std::result::Result<Handle, anyhow::Error>;

/// One clip playing in a process we did not spawn in this lifetime
/// (e.g. spawned by an earlier `soundboard play` invocation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalPlayback {
    /// OS process id.
    pub pid: u32,
    /// Clip file argument, when the command line carries one.
    pub clip: Option<PathBuf>,
}

/// Playback backend trait (see [`PwCatPlayer`] for the v1 implementation).
pub trait Player {
    /// Starts playing `clip` under registry `name` at `volume` (0.0..=1.0).
    /// Returns a [`Handle`] once the player accepted the sound; immediate
    /// failures (binary missing, decode error within the probe window) are
    /// errors.
    fn play(&mut self, clip: &Path, name: &str, volume: f32) -> Result<Handle>;
    /// Stops one playback. Stopping an unknown/finished handle is a no-op.
    fn stop(&mut self, handle: Handle) -> Result<()>;
    /// Stops every playback this player spawned.
    fn stop_all(&mut self) -> Result<()>;
    /// (handle, clip name) pairs of playbacks not yet known to have exited.
    fn active(&self) -> Vec<(Handle, String)>;
    /// Reaps children that exited on their own; returns their handles.
    fn reap(&mut self) -> Vec<Handle>;
}

#[derive(Debug)]
struct RunningChild {
    name: String,
    child: Child,
    stderr_log: PathBuf,
}

/// v1 player: spawns `pw-cat` per clip (see module docs) with an ffmpeg
/// transcode fallback for clips `pw-cat` cannot decode.
#[derive(Debug)]
pub struct PwCatPlayer {
    binary: PathBuf,
    target: String,
    probe_window: Duration,
    transcoder: Transcoder,
    children: BTreeMap<Handle, RunningChild>,
    next_id: u64,
}

impl PwCatPlayer {
    /// Player targeting [`SINK_MEMES`] with `pw-cat` from `PATH`; the
    /// transcoder auto-detects `ffmpeg` once here (a PATH scan, no spawn).
    #[must_use]
    pub fn new() -> Self {
        PwCatPlayer {
            binary: PathBuf::from(PW_CAT),
            target: SINK_MEMES.to_string(),
            probe_window: PROBE_WINDOW,
            transcoder: Transcoder::new(),
            children: BTreeMap::new(),
            next_id: 1,
        }
    }

    /// Overrides the player binary (tests inject a fake script).
    #[must_use]
    pub fn with_binary(mut self, binary: PathBuf) -> Self {
        self.binary = binary;
        self
    }

    /// Overrides the probe window (tests shrink it).
    #[must_use]
    pub fn with_probe_window(mut self, window: Duration) -> Self {
        self.probe_window = window;
        self
    }

    /// Overrides the transcoder (tests inject a fake ffmpeg and a temp
    /// cache dir; [`Transcoder::disabled`] turns the fallback off).
    #[must_use]
    pub fn with_transcoder(mut self, transcoder: Transcoder) -> Self {
        self.transcoder = transcoder;
        self
    }

    /// The player's transcode fallback; the CLI reads it to stop a cached
    /// twin playback before an interrupting play (see
    /// [`PwCatPlayer::stop_clip_everywhere`]).
    #[must_use]
    pub fn transcoder(&self) -> &Transcoder {
        &self.transcoder
    }

    fn alloc_handle(&mut self) -> Handle {
        let handle = Handle::from_u64(self.next_id);
        self.next_id += 1;
        handle
    }

    /// Spawns `pw-cat` for `clip` and probes it. Outer `Err`: the spawn or
    /// the probe itself failed (binary missing, poll error) — not a decode
    /// verdict. Inner `Err`: the child exited within the probe window with a
    /// non-zero status — the decode-failure case the transcode fallback
    /// reacts to. Inner `Ok`: the clip is playing.
    fn spawn_with_probe(&mut self, clip: &Path, name: &str, volume: f32) -> Result<ProbeOutcome> {
        let volume = volume.clamp(0.0, 1.0);
        let (handle, mut running) = self.spawn(clip, name, volume)?;

        let deadline = Instant::now() + self.probe_window;
        let mut exited: Option<std::process::ExitStatus> = None;
        while Instant::now() < deadline {
            match running.child.try_wait() {
                Ok(Some(status)) => {
                    exited = Some(status);
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = running.child.kill();
                    let _ = running.child.wait();
                    return Err(anyhow::anyhow!("сбой опроса плеера: {e}"));
                }
            }
            thread::sleep(POLL_STEP);
        }
        match exited {
            Some(status) => {
                let stderr = read_first_line(&running.stderr_log);
                remove_log_best_effort(&running.stderr_log);
                let err = anyhow::anyhow!(
                    "плеер завершился сразу с {status}{}",
                    stderr.map_or_else(String::new, |line| format!(": {line}"))
                );
                Ok(Err(err))
            }
            None => {
                // Healthy: pw-cat is quiet on stderr once playing, so the log
                // file is deleted now (the child keeps its fd; the path is
                // never re-created).
                remove_log_best_effort(&running.stderr_log);
                self.children.insert(handle, running);
                Ok(Ok(handle))
            }
        }
    }

    fn spawn(&mut self, clip: &Path, name: &str, volume: f32) -> Result<(Handle, RunningChild)> {
        let volume = volume.clamp(0.0, 1.0);
        let (stderr_log, stderr_stdio) = open_stderr_log();
        let mut command = Command::new(&self.binary);
        command
            .arg("--playback")
            .arg("--target")
            .arg(&self.target)
            .arg("--volume")
            .arg(format!("{volume}"))
            .arg(clip)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr_stdio)
            .process_group(0);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                remove_log_best_effort(&stderr_log);
                if e.kind() == std::io::ErrorKind::NotFound {
                    return Err(anyhow::anyhow!(
                        "бинарь плеера '{}' не найден (установлен ли pipewire-utils?)",
                        self.binary.display()
                    ));
                }
                // The io error stays in the chain: the test helpers classify
                // the transient ETXTBSY by its ErrorKind.
                return Err(e)
                    .with_context(|| format!("не удалось запустить {}", self.binary.display()));
            }
        };
        let handle = self.alloc_handle();
        Ok((
            handle,
            RunningChild {
                name: name.to_string(),
                child,
                stderr_log,
            },
        ))
    }
}

impl Default for PwCatPlayer {
    fn default() -> Self {
        PwCatPlayer::new()
    }
}

impl Player for PwCatPlayer {
    fn play(&mut self, clip: &Path, name: &str, volume: f32) -> Result<Handle> {
        self.reap();
        match self.spawn_with_probe(clip, name, volume)? {
            Ok(handle) => Ok(handle),
            // Decode failure: fall back to a transcoded wav when ffmpeg is
            // available; anything else (transcode failure, no ffmpeg)
            // surfaces the ORIGINAL decode error.
            Err(decode_err) => match self.transcoder.transcode(clip) {
                None => Err(decode_err),
                Some(Err(_transcode_err)) => Err(decode_err),
                Some(Ok(cached)) => match self.spawn_with_probe(&cached, name, volume)? {
                    Ok(handle) => Ok(handle),
                    Err(cached_err) => Err(cached_err),
                },
            },
        }
    }

    fn stop(&mut self, handle: Handle) -> Result<()> {
        self.reap();
        let Some(mut running) = self.children.remove(&handle) else {
            return Ok(()); // already exited and reaped
        };
        let pid = running.child.id();
        signal_group(pid, libc::SIGTERM);
        if !wait_exit(&mut running.child, TERM_GRACE)? {
            signal_group(pid, libc::SIGKILL);
            let _ = signal_pid(pid, libc::SIGKILL); // child.wait() is the authority
            running
                .child
                .wait()
                .with_context(|| format!("не удалось забрать {pid} после SIGKILL"))?;
        }
        remove_log_best_effort(&running.stderr_log);
        Ok(())
    }

    fn stop_all(&mut self) -> Result<()> {
        let handles: Vec<Handle> = self.children.keys().copied().collect();
        let mut first_err = None;
        for handle in handles {
            if let Err(e) = self.stop(handle) {
                first_err.get_or_insert(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn active(&self) -> Vec<(Handle, String)> {
        self.children
            .iter()
            .map(|(h, r)| (*h, r.name.clone()))
            .collect()
    }

    fn reap(&mut self) -> Vec<Handle> {
        let mut finished = Vec::new();
        let handles: Vec<Handle> = self.children.keys().copied().collect();
        for handle in handles {
            let Some(running) = self.children.get_mut(&handle) else {
                continue;
            };
            match running.child.try_wait() {
                Ok(Some(_status)) => {
                    remove_log_best_effort(&running.stderr_log);
                    self.children.remove(&handle);
                    finished.push(handle);
                }
                Ok(None) => {}
                // A failed poll leaves the child alone; do not drop the entry.
                Err(_) => {}
            }
        }
        finished
    }
}

impl PwCatPlayer {
    /// Lists `pw-cat` playbacks into our target currently running anywhere on
    /// the host (see the module docs for the matching rule).
    ///
    /// # Errors
    /// Fails only if `/proc` is unreadable.
    pub fn find_running(clip: Option<&Path>) -> Result<Vec<ExternalPlayback>> {
        let mut found = Vec::new();
        let entries = std::fs::read_dir("/proc").context("cannot scan /proc")?;
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(bytes) = std::fs::read(entry.path().join("cmdline")) else {
                continue; // kernel threads, racing exits, permission gaps
            };
            let args: Vec<String> = String::from_utf8_lossy(&bytes)
                .split('\0')
                .filter(|a| !a.is_empty())
                .map(str::to_string)
                .collect();
            if matches_cat_args(&args, clip) {
                found.push(ExternalPlayback {
                    pid,
                    clip: args.last().map(PathBuf::from),
                });
            }
        }
        Ok(found)
    }

    /// SIGTERM→SIGKILLs every matching external playback; returns how many
    /// were stopped.
    ///
    /// Before each signal the pid's `/proc/<pid>/cmdline` is re-read: a pid
    /// that no longer matches is treated as gone and skipped, so a recycled
    /// pid is never signaled. A pid that exists but refuses our signal
    /// (`EPERM` — owned by another user) is skipped as foreign. Only genuine
    /// failures while signaling our own pids fail the command.
    ///
    /// # Errors
    /// Fails when a matching process survives SIGKILL, or signaling it fails
    /// for a reason other than EPERM/ESRCH (or `/proc` is unreadable).
    pub fn stop_external(clip: Option<&Path>) -> Result<usize> {
        let found = Self::find_running(clip)?;
        let mut stopped = 0usize;
        for playback in found {
            let pid = playback.pid;
            if !cmdline_still_matches(pid, clip) {
                continue; // exited (or pid recycled) since the scan
            }
            match signal_pid(pid, libc::SIGTERM) {
                SignalOutcome::Sent | SignalOutcome::Gone => {}
                SignalOutcome::Foreign => continue,
                SignalOutcome::Failed(e) => {
                    return Err(e).with_context(|| format!("не удалось отправить сигнал {pid}"));
                }
            }
            if !wait_gone(pid, TERM_GRACE) {
                if !cmdline_still_matches(pid, clip) {
                    continue; // gone mid-grace; never SIGKILL a recycled pid
                }
                match signal_pid(pid, libc::SIGKILL) {
                    SignalOutcome::Sent | SignalOutcome::Gone => {}
                    SignalOutcome::Foreign => continue,
                    SignalOutcome::Failed(e) => {
                        return Err(e)
                            .with_context(|| format!("не удалось отправить сигнал {pid}"));
                    }
                }
                if !wait_gone(pid, TERM_GRACE) {
                    bail!("процесс {pid} не завершился");
                }
            }
            stopped += 1;
        }
        Ok(stopped)
    }

    /// Stops every external playback of `clip` — under its original path AND
    /// its transcoded cache twin ([`playback_candidates`]). The CLI uses
    /// this for cross-process per-clip interrupt/stop; the GUI interrupts
    /// through scheduler handles instead, so the cache twin matters only for
    /// playbacks spawned by `soundboard play`.
    ///
    /// # Errors
    /// Same as [`PwCatPlayer::stop_external`].
    pub fn stop_clip_everywhere(clip: &Path, transcoder: &Transcoder) -> Result<usize> {
        let mut stopped = 0usize;
        for path in playback_candidates(clip, transcoder) {
            stopped += Self::stop_external(Some(&path))?;
        }
        Ok(stopped)
    }
}

/// Paths a clip may be playing under right now: the original file plus its
/// transcoded cache twin when one exists (the fallback plays the wav, whose
/// `/proc/<pid>/cmdline` path differs from the source path).
fn playback_candidates(clip: &Path, transcoder: &Transcoder) -> Vec<PathBuf> {
    let mut candidates = vec![clip.to_path_buf()];
    if let Some(cached) = transcoder.cache_path(clip)
        && cached.exists()
    {
        candidates.push(cached);
    }
    candidates
}

/// Transcode fallback: decodes a clip `pw-cat` cannot read into a canonical
/// 48 kHz stereo wav via `ffmpeg`, cached by source mtime + size (see the
/// module docs). `ffmpeg` availability is detected ONCE at construction (a
/// `PATH` scan, no spawn); tests inject a fake binary and a temp cache dir.
#[derive(Debug, Clone)]
pub struct Transcoder {
    binary: Option<PathBuf>,
    cache_dir: PathBuf,
}

impl Transcoder {
    /// Detects `ffmpeg` on `PATH` and points the cache at
    /// `~/.cache/soundboard/transcoded` (temp dir as the fallback when the
    /// user cache dir cannot be determined).
    #[must_use]
    pub fn new() -> Self {
        Transcoder {
            binary: find_in_path(FFMPEG),
            cache_dir: dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("soundboard")
                .join("transcoded"),
        }
    }

    /// A fallback-off transcoder (unit tests, explicit opt-out).
    #[must_use]
    pub fn disabled() -> Self {
        Transcoder {
            binary: None,
            cache_dir: std::env::temp_dir(),
        }
    }

    /// (tests) A transcoder around an injected ffmpeg binary and cache dir.
    #[must_use]
    pub fn with_binary_and_cache(binary: PathBuf, cache_dir: PathBuf) -> Self {
        Transcoder {
            binary: Some(binary),
            cache_dir,
        }
    }

    /// Transcodes `src` into the cached wav and returns its path. `None`
    /// when the fallback is off (no ffmpeg detected/injected). A failed
    /// transcode is `Err` — the caller then keeps the original decode
    /// error, so the user still sees the honest per-clip failure.
    pub fn transcode(&self, src: &Path) -> Option<Result<PathBuf>> {
        let binary = self.binary.as_ref()?;
        let Some(dst) = self.cache_path(src) else {
            return Some(Err(anyhow::anyhow!(
                "не удалось вычислить ключ кэша для {}",
                src.display()
            )));
        };
        // Cache hit: the source mtime + size are unchanged since the wav
        // was made — reuse it instead of paying the transcode again.
        if needs_transcode_for(Some(&dst)) {
            if let Err(e) = std::fs::create_dir_all(&self.cache_dir) {
                return Some(Err(e).with_context(|| {
                    format!(
                        "не удалось создать кэш-каталог {}",
                        self.cache_dir.display()
                    )
                }));
            }
            let output = Command::new(binary)
                .arg("-y")
                .arg("-v")
                .arg("error")
                .arg("-i")
                .arg(src)
                .arg("-ar")
                .arg("48000")
                .arg("-ac")
                .arg("2")
                .arg(&dst)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output();
            match output {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    // Never leave a partial/failed file behind as a cache hit.
                    let _ = std::fs::remove_file(&dst);
                    return Some(Err(anyhow::anyhow!(
                        "ffmpeg не смог перекодировать {}: {}",
                        src.display(),
                        last_stderr_line(&out.stderr)
                    )));
                }
                Err(e) => {
                    return Some(
                        Err(e)
                            .with_context(|| format!("не удалось запустить {}", binary.display())),
                    );
                }
            }
        }
        Some(Ok(dst))
    }

    /// Cached wav path for `src`: `<cache_dir>/<stem>-<mtime+size hash>.wav`.
    /// `None` when the source is unreadable or has no readable stem.
    fn cache_path(&self, src: &Path) -> Option<PathBuf> {
        cache_key(src).map(|key| self.cache_dir.join(format!("{key}.wav")))
    }
}

impl Default for Transcoder {
    fn default() -> Self {
        Transcoder::new()
    }
}

/// True when the cached wav must be (re)created: no cache path or no file.
fn needs_transcode_for(dst: Option<&Path>) -> bool {
    match dst {
        Some(path) => !path.exists(),
        None => true,
    }
}

/// Cache key of a source clip: `<stem>-<fnv1a64(mtime nanos ++ size) in
/// hex>`. Deterministic across runs; a re-recorded or edited clip gets a
/// new key, an untouched one keeps reusing its cached wav.
fn cache_key(src: &Path) -> Option<String> {
    let stem = src.file_stem().and_then(OsStr::to_str)?;
    let meta = std::fs::metadata(src).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64; // FNV-1a 64 offset basis
    let mut bytes = [0_u8; 24];
    bytes[..16].copy_from_slice(&mtime.as_nanos().to_le_bytes());
    bytes[16..].copy_from_slice(&meta.len().to_le_bytes());
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(format!("{stem}-{hash:016x}"))
}

/// Resolves `name` against `PATH` (a plain scan, no spawn): first existing
/// file wins. Executability is assumed — everything on `PATH` is.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Last non-empty stderr line (ffmpeg diagnostics land at the end).
fn last_stderr_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("нет вывода")
        .to_string()
}

/// Best-effort canonical form of `path`; falls back to the raw path when
/// canonicalization fails (missing file, dangling symlink, permission gap).
fn best_effort_canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// True for argument lists of the shape our player spawns:
/// `--playback` plus `--target <our sink>`, optionally restricted to one clip.
/// The clip comparison is canonicalized on both sides (the filter may spell
/// the clip through a symlink or non-normalized path); falls back to raw
/// comparison when either side cannot be canonicalized.
fn matches_cat_args(args: &[String], clip: Option<&Path>) -> bool {
    let has_playback = args.iter().any(|a| a == "--playback");
    let has_target = args
        .windows(2)
        .any(|w| w[0] == "--target" && w[1] == SINK_MEMES)
        || args.iter().any(|a| a == &format!("--target={SINK_MEMES}"));
    let clip_matches = match clip {
        None => true,
        Some(clip) => args.iter().any(|a| {
            let arg = Path::new(a.as_str());
            arg == clip || best_effort_canonical(arg) == best_effort_canonical(clip)
        }),
    };
    has_playback && has_target && clip_matches
}

/// Outcome of trying to signal an external pid.
#[derive(Debug)]
enum SignalOutcome {
    /// Signal was delivered.
    Sent,
    /// Process no longer exists (ESRCH).
    Gone,
    /// Process exists but is not ours to signal (EPERM, e.g. another user's).
    Foreign,
    /// Unexpected failure while signaling.
    Failed(std::io::Error),
}

/// Pure errno→outcome mapping (unit-testable core of [`signal_pid`]).
fn kill_outcome(res: std::io::Result<()>) -> SignalOutcome {
    match res {
        Ok(()) => SignalOutcome::Sent,
        Err(e) => match e.raw_os_error() {
            Some(libc::ESRCH) => SignalOutcome::Gone,
            Some(libc::EPERM) => SignalOutcome::Foreign,
            _ => SignalOutcome::Failed(e),
        },
    }
}

/// True while `/proc/<pid>/cmdline` still holds argument lists of our shape
/// (see [`matches_cat_args`]); a vanished or recycled pid reads as false.
fn cmdline_still_matches(pid: u32, clip: Option<&Path>) -> bool {
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args: Vec<String> = String::from_utf8_lossy(&bytes)
        .split('\0')
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect();
    matches_cat_args(&args, clip)
}

fn signal_group(pid: u32, sig: i32) {
    // The child got process_group(0), so its pgid equals its pid.
    // Safety: kill only sends a signal; it cannot dereference bad pointers.
    let _ = unsafe { libc::kill(-(pid as i32), sig) };
}

fn signal_pid(pid: u32, sig: i32) -> SignalOutcome {
    // Safety: kill only sends a signal; it cannot dereference bad pointers.
    let res = if unsafe { libc::kill(pid as i32, sig) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    };
    kill_outcome(res)
}

/// Polls `try_wait` until the child exited or `timeout` elapsed.
fn wait_exit(child: &mut Child, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(POLL_STEP);
    }
}

/// True once the process is gone OR is an unreaped zombie (empty cmdline) —
/// both mean it is no longer playing anything.
fn wait_gone(pid: u32, timeout: Duration) -> bool {
    let cmdline = PathBuf::from(format!("/proc/{pid}/cmdline"));
    let deadline = Instant::now() + timeout;
    loop {
        match std::fs::read(&cmdline) {
            Ok(bytes) if !bytes.is_empty() => {}
            Ok(_) | Err(_) => return true,
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_STEP);
    }
}

/// Creates a unique stderr log file; falls back to /dev/null on failure
/// (playback must not depend on diagnostics).
fn open_stderr_log() -> (PathBuf, Stdio) {
    let base = std::env::temp_dir();
    for attempt in 0..3u32 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() + attempt)
            .unwrap_or(attempt);
        let path = base.join(format!(
            "soundboard-pwcat-{}-{nanos}.log",
            std::process::id()
        ));
        if let Ok(file) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            return (path, Stdio::from(file));
        }
    }
    let devnull = std::fs::File::create("/dev/null").ok();
    (
        PathBuf::from("/dev/null"),
        devnull.map_or(Stdio::null(), Stdio::from),
    )
}

fn remove_log_best_effort(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn read_first_line(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::File;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    use crate::scheduler::Scheduler;

    /// Writes a fake player script: logs its arguments, then `behavior`.
    ///
    /// The payload is written to a unique temporary sibling and atomically
    /// renamed into place, so the file a child later `exec`s was never open
    /// for writing — without this, parallel test load can hit a flaky
    /// `ETXTBSY` on exec of the just-written script.
    fn fake_player(dir: &Path, behavior: &str, log: &Path) -> PathBuf {
        let script = dir.join("fake-pw-cat.sh");
        write_script_atomic(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n{behavior}\n",
                log.display()
            ),
        );
        script
    }

    /// Writes `content` to `path` and makes it executable via a unique temp
    /// file plus atomic rename (see [`fake_player`] for why).
    fn write_script_atomic(path: &Path, content: &str) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let tmp = path.with_extension(format!("tmp-{}-{nanos}", std::process::id()));
        let mut file = File::create(&tmp).expect("create temp script");
        file.write_all(content.as_bytes())
            .expect("write temp script");
        drop(file);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .expect("chmod temp script");
        std::fs::rename(&tmp, path).expect("rename script into place");
    }

    fn spawn_probe_window() -> Duration {
        Duration::from_millis(30)
    }

    /// Dumps every process holding `path` open (pid, cmdline, fd flags) —
    /// diagnostics for the ETXTBSY hunt in tests.
    fn report_file_holders(path: &Path) -> String {
        let mut out = String::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return out;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
            let Ok(fds) = std::fs::read_dir(&fd_dir) else {
                continue;
            };
            for fd in fds.flatten() {
                if std::fs::read_link(fd.path()).is_ok_and(|l| l == path) {
                    let cmd = std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
                        .map(|s| s.replace('\0', " "))
                        .unwrap_or_else(|_| "?".to_string());
                    let flags = std::fs::read_to_string(format!(
                        "/proc/{pid}/fdinfo/{}",
                        fd.file_name().to_string_lossy()
                    ))
                    .map(|s| {
                        s.lines()
                            .find(|l| l.starts_with("flags:"))
                            .unwrap_or("")
                            .to_string()
                    })
                    .unwrap_or_default();
                    out.push_str(&format!("\n  holder pid={pid} flags=[{flags}] cmd={cmd}"));
                }
            }
        }
        out
    }

    /// True when `err` is the transient ETXTBSY of exec'ing a script file.
    fn is_etxtbsy(err: &anyhow::Error) -> bool {
        err.downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::ExecutableFileBusy)
    }

    /// Plays via `player`, retrying while the spawn fails with ETXTBSY.
    ///
    /// The atomic-rename write in [`fake_player`] shrinks the classic
    /// exec-just-written-file race; when it still bites despite the retries,
    /// the panic lists the /proc holders of the file for diagnosis.
    fn play_retrying(player: &mut PwCatPlayer, clip: &Path, name: &str, volume: f32) -> Handle {
        for attempt in 0..50u32 {
            match player.play(clip, name, volume) {
                Ok(handle) => return handle,
                Err(err) if attempt < 49 && is_etxtbsy(&err) => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => {
                    let holders = report_file_holders(&player.binary);
                    panic!("play {name} failed: {err:#}{holders}");
                }
            }
        }
        unreachable!("retry loop always returns or panics")
    }

    /// Spawns a fake script directly, retrying on the same transient ETXTBSY
    /// window as [`play_retrying`].
    fn spawn_fake(script: &Path, args: &[&str]) -> Child {
        for attempt in 0..50u32 {
            match Command::new(script)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => return child,
                Err(e) if attempt < 49 && e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("spawn fake failed: {e}"),
            }
        }
        unreachable!("retry loop always returns or panics")
    }

    #[test]
    fn spawn_ok_registers_active_handle_with_args() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let clip = dir.path().join("airhorn.wav");
        File::create(&clip).expect("clip");
        let script = fake_player(dir.path(), "exec sleep 5", &log);
        let mut player = PwCatPlayer::new()
            .with_binary(script)
            .with_probe_window(spawn_probe_window());

        let handle = play_retrying(&mut player, &clip, "airhorn", 0.9);
        assert_eq!(player.active(), vec![(handle, "airhorn".to_string())]);

        let calls = std::fs::read_to_string(&log).expect("read log");
        assert_eq!(
            calls.trim(),
            format!(
                "--playback --target soundboard --volume 0.9 {}",
                clip.display()
            )
        );

        player.stop(handle).expect("stop");
        assert!(player.active().is_empty());
    }

    #[test]
    fn stop_kills_the_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let clip = dir.path().join("x.wav");
        File::create(&clip).expect("clip");
        let script = fake_player(dir.path(), "exec sleep 30", &log);
        let mut player = PwCatPlayer::new()
            .with_binary(script)
            .with_probe_window(spawn_probe_window());
        let handle = play_retrying(&mut player, &clip, "x", 1.0);
        player.stop(handle).expect("stop");
        // The process group is gone: nothing matches the external scan.
        let external = PwCatPlayer::find_running(Some(&clip)).expect("find");
        assert!(
            external.is_empty(),
            "expected no survivors, got {external:?}"
        );
    }

    #[test]
    fn exited_child_is_reaped_and_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let clip = dir.path().join("x.wav");
        File::create(&clip).expect("clip");
        // No exec: the script itself exits ~200ms in.
        let script = fake_player(dir.path(), "sleep 0.2", &log);
        let mut player = PwCatPlayer::new()
            .with_binary(script)
            .with_probe_window(Duration::ZERO);
        let handle = play_retrying(&mut player, &clip, "x", 1.0);
        assert_eq!(player.active().len(), 1);
        thread::sleep(Duration::from_millis(400));
        let finished = player.reap();
        assert_eq!(finished, vec![handle]);
        assert!(player.active().is_empty());
    }

    #[test]
    fn immediate_decode_failure_is_reported_with_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let clip = dir.path().join("broken.mp3");
        File::create(&clip).expect("clip");
        let script = fake_player(
            dir.path(),
            "echo 'fake: unsupported format' >&2\nexit 42",
            &log,
        );
        let mut player = PwCatPlayer::new()
            .with_binary(script)
            .with_probe_window(spawn_probe_window());
        // Wait out the transient ETXTBSY (see play_retrying); the expected
        // outcome is the decode failure, not a spawn failure.
        let err = loop {
            match player.play(&clip, "broken", 1.0) {
                Ok(_handle) => panic!("expected decode error, got a running playback"),
                Err(err) if is_etxtbsy(&err) => thread::sleep(Duration::from_millis(20)),
                Err(err) => break err,
            }
        };
        let text = err.to_string();
        assert!(text.contains("завершился сразу"), "{text}");
        assert!(text.contains("fake: unsupported format"), "{text}");
        assert!(player.active().is_empty());
    }

    #[test]
    fn missing_binary_is_a_clear_error() {
        let clip = Path::new("/nonexistent/clip.wav");
        let mut player = PwCatPlayer::new().with_binary(PathBuf::from("/nonexistent/sb-pw-cat"));
        let err = player.play(clip, "x", 1.0).expect_err("not found");
        assert!(err.to_string().contains("не найден"), "{err:?}");
    }

    #[test]
    fn two_clips_overlap_then_stop_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let a = dir.path().join("a.wav");
        let b = dir.path().join("b.wav");
        File::create(&a).expect("a");
        File::create(&b).expect("b");
        let script = fake_player(dir.path(), "exec sleep 5", &log);
        let mut player = PwCatPlayer::new()
            .with_binary(script)
            .with_probe_window(spawn_probe_window());
        let h1 = play_retrying(&mut player, &a, "a", 1.0);
        let h2 = play_retrying(&mut player, &b, "b", 1.0);
        assert_eq!(player.active().len(), 2);
        player.stop_all().expect("stop all");
        assert!(player.active().is_empty());
        // Idempotent: stopping finished/unknown handles is a no-op.
        player.stop(h1).expect("stop again");
        player.stop(h2).expect("stop again");
    }

    fn press(sched: &mut Scheduler, name: &str, interrupt: bool) -> crate::scheduler::Decision {
        sched.decide(crate::scheduler::Request::Play(
            crate::scheduler::PlayRequest {
                name: name.to_string(),
                interrupt_mode: interrupt,
            },
        ))
    }

    #[test]
    fn scheduler_and_player_per_clip_interrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let airhorn = dir.path().join("airhorn.wav");
        let applause = dir.path().join("applause.wav");
        File::create(&airhorn).expect("airhorn");
        File::create(&applause).expect("applause");
        let script = fake_player(dir.path(), "exec sleep 5", &log);
        let mut player = PwCatPlayer::new()
            .with_binary(script)
            .with_probe_window(spawn_probe_window());
        let mut sched = Scheduler::new();

        let d1 = press(&mut sched, "airhorn", true);
        assert_eq!(d1, crate::scheduler::Decision::Start { stop: vec![] });
        let h1 = play_retrying(&mut player, &airhorn, "airhorn", 1.0);
        sched.started("airhorn", h1);

        // Different clip overlaps.
        let d2 = press(&mut sched, "applause", true);
        assert_eq!(d2, crate::scheduler::Decision::Start { stop: vec![] });
        let h2 = play_retrying(&mut player, &applause, "applause", 1.0);
        sched.started("applause", h2);
        assert_eq!(player.active().len(), 2);

        // Same clip in interrupt mode: scheduler orders the old one killed.
        let d3 = press(&mut sched, "airhorn", true);
        let crate::scheduler::Decision::Start { stop } = d3 else {
            panic!("expected Start, got {d3:?}");
        };
        assert_eq!(stop, vec![h1]);
        for handle in stop {
            player.stop(handle).expect("stop old");
        }
        let h3 = play_retrying(&mut player, &airhorn, "airhorn", 1.0);
        sched.started("airhorn", h3);
        assert_eq!(
            player.active(),
            vec![(h2, "applause".to_string()), (h3, "airhorn".to_string())]
        );
        assert_eq!(sched.active(), vec![(h3, "airhorn"), (h2, "applause")]);

        // Stop everything via the scheduler decision.
        let d4 = sched.stop_all();
        let crate::scheduler::Decision::Stop(handles) = d4 else {
            panic!("expected Stop, got {d4:?}");
        };
        assert_eq!(handles.len(), 2);
        player.stop_all().expect("stop all");
        assert!(player.active().is_empty());
        assert!(sched.is_empty());
    }

    #[test]
    fn volume_is_clamped_into_pw_cat_range() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let clip = dir.path().join("v.wav");
        File::create(&clip).expect("clip");
        let script = fake_player(dir.path(), "exec sleep 5", &log);
        let mut player = PwCatPlayer::new()
            .with_binary(script)
            .with_probe_window(spawn_probe_window());
        let handle = play_retrying(&mut player, &clip, "v", 2.0);
        player.stop(handle).expect("stop");
        let calls = std::fs::read_to_string(&log).expect("read log");
        assert_eq!(
            calls.trim(),
            format!(
                "--playback --target soundboard --volume 1 {}",
                clip.display()
            )
        );
    }

    #[test]
    fn find_and_stop_external_playbacks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let clip = dir.path().join("ext.wav");
        File::create(&clip).expect("clip");
        // A fake "long-running player" whose /proc cmdline keeps our argument
        // shape: no exec, and a TERM trap that kills its background sleep so
        // nothing is orphaned when stop_external signals the shell.
        let script = dir.path().join("fake-external.sh");
        write_script_atomic(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ntrap 'kill $pid 2>/dev/null; exit 0' TERM\nsleep 30 &\npid=$!\nwait $pid\n",
                log.display()
            ),
        );
        let mut child = spawn_fake(
            &script,
            &[
                "--playback",
                "--target",
                "soundboard",
                "--volume",
                "0.9",
                clip.to_str().expect("utf8 clip"),
            ],
        );
        thread::sleep(Duration::from_millis(50));

        assert!(
            PwCatPlayer::find_running(None)
                .expect("find all")
                .iter()
                .any(|p| p.pid == child.id())
        );
        let mine = PwCatPlayer::find_running(Some(&clip)).expect("find clip");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].pid, child.id());
        assert_eq!(mine[0].clip.as_deref(), Some(clip.as_path()));
        assert!(
            PwCatPlayer::find_running(Some(Path::new("/other/clip.wav")))
                .expect("find other")
                .is_empty()
        );

        let stopped = PwCatPlayer::stop_external(Some(&clip)).expect("stop external");
        assert_eq!(stopped, 1);
        assert!(
            PwCatPlayer::find_running(Some(&clip))
                .expect("find after stop")
                .is_empty()
        );
        child.wait().expect("reap our test child");
        drop(script);
    }

    #[test]
    fn matches_cat_args_shape_only() {
        let args: Vec<String> = ["pw-cat", "--playback", "--target", "soundboard", "/x.wav"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(matches_cat_args(&args, None));
        assert!(matches_cat_args(&args, Some(Path::new("/x.wav"))));
        assert!(!matches_cat_args(&args, Some(Path::new("/y.wav"))));
        let other_target: Vec<String> = ["--playback", "--target", "alsa", "/x.wav"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(!matches_cat_args(&other_target, None));
        let eq_form: Vec<String> = ["--playback", "--target=soundboard"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(matches_cat_args(&eq_form, None));
    }

    #[test]
    fn clip_filter_matches_through_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let link_dir = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path(), &link_dir).expect("symlink");
        let real = dir.path().join("x.wav");
        File::create(&real).expect("clip");
        let args: Vec<String> = [
            "pw-cat",
            "--playback",
            "--target",
            "soundboard",
            real.to_str().expect("utf8"),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // The filter may spell the clip through a symlinked directory.
        assert!(
            matches_cat_args(&args, Some(&link_dir.join("x.wav"))),
            "symlinked filter path must match the real arg path"
        );
        // And the other way around: a real filter path matches a symlinked arg.
        let linked_arg: Vec<String> = [
            "pw-cat",
            "--playback",
            "--target",
            "soundboard",
            link_dir.join("x.wav").to_str().expect("utf8"),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(matches_cat_args(&linked_arg, Some(&real)));
        // Unrelated clips still do not match.
        assert!(!matches_cat_args(&args, Some(&link_dir.join("y.wav"))));
    }

    #[test]
    fn kill_outcome_maps_errnos() {
        assert!(matches!(kill_outcome(Ok(())), SignalOutcome::Sent));
        assert!(matches!(
            kill_outcome(Err(std::io::Error::from_raw_os_error(libc::ESRCH))),
            SignalOutcome::Gone
        ));
        assert!(matches!(
            kill_outcome(Err(std::io::Error::from_raw_os_error(libc::EPERM))),
            SignalOutcome::Foreign
        ));
        assert!(matches!(
            kill_outcome(Err(std::io::Error::from_raw_os_error(libc::EINVAL))),
            SignalOutcome::Failed(_)
        ));
    }

    #[test]
    fn cmdline_still_matches_rejects_missing_pids() {
        // No live process has this pid in this namespace with our args; the
        // helper must read it as gone rather than panicking or matching.
        // (Linux pid_max caps far below this value by default.)
        let absent: u32 = 4_000_000_001;
        assert!(!cmdline_still_matches(absent, None));
    }

    /// A fake `pw-cat`: ANY invocation with an `.ogg` file argument dies
    /// immediately with a decode error on stderr (the malformed-clip
    /// signal); other files (the cached wav) play.
    fn fake_player_failing_once(dir: &Path, log: &Path) -> PathBuf {
        let script = dir.join("fake-pw-cat-once.sh");
        write_script_atomic(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nfor last; do :; done\ncase \"$last\" in\n  *.ogg) echo 'sndfile: failed to open audio file: malformed' >&2\n        exit 42\n        ;;\nesac\nexec sleep 5\n",
                log.display()
            ),
        );
        script
    }

    /// A fake `ffmpeg`: writes a dummy wav payload to its LAST argument
    /// (the `-o`-style destination our transcoder passes) and exits 0.
    fn fake_ffmpeg(dir: &Path) -> PathBuf {
        let script = dir.join("fake-ffmpeg.sh");
        write_script_atomic(
            &script,
            "#!/bin/sh\nfor last; do :; done\nprintf 'RIFFfake' > \"$last\"\n",
        );
        script
    }

    #[test]
    fn transcode_fallback_plays_the_cached_wav() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let clip = dir.path().join("доброе утро девочки.ogg");
        File::create(&clip).expect("clip");
        let cache_dir = tempfile::tempdir().expect("cache");
        let transcoder = Transcoder::with_binary_and_cache(
            fake_ffmpeg(dir.path()),
            cache_dir.path().to_path_buf(),
        );
        let script = fake_player_failing_once(dir.path(), &log);
        let mut player = PwCatPlayer::new()
            .with_binary(script)
            .with_probe_window(spawn_probe_window())
            .with_transcoder(transcoder);

        let handle = play_retrying(&mut player, &clip, "доброе", 1.0);
        player.stop(handle).expect("stop");

        // The cached wav exists under the deterministic mtime+size key and
        // is exactly what the second pw-cat call was given.
        let key = cache_key(&clip).expect("cache key");
        let cached = cache_dir.path().join(format!("{key}.wav"));
        assert_eq!(std::fs::read(&cached).expect("cached wav"), b"RIFFfake");
        let calls = std::fs::read_to_string(&log).expect("read log");
        let mut lines = calls.lines();
        let first = lines.next().expect("first call");
        assert!(first.contains(clip.to_str().expect("utf8 clip")), "{first}");
        let second = lines.next().expect("second call");
        assert!(
            second.contains(cached.to_str().expect("utf8 dst")),
            "{second}"
        );
    }

    #[test]
    fn cached_wav_is_reused_on_the_next_play() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let clip = dir.path().join("malformed.ogg");
        File::create(&clip).expect("clip");
        let cache_dir = tempfile::tempdir().expect("cache");
        let transcoder = Transcoder::with_binary_and_cache(
            fake_ffmpeg(dir.path()),
            cache_dir.path().to_path_buf(),
        );
        let script = fake_player_failing_once(dir.path(), &log);
        let mut player = PwCatPlayer::new()
            .with_binary(script)
            .with_probe_window(spawn_probe_window())
            .with_transcoder(transcoder);

        let h1 = play_retrying(&mut player, &clip, "x", 1.0);
        player.stop(h1).expect("stop");
        let h2 = play_retrying(&mut player, &clip, "x", 1.0);
        player.stop(h2).expect("stop");

        // 4 calls total: (ogg fails, cached wav plays) twice — the
        // transcoder only had to run once because the cache key did not
        // change between the plays.
        let calls = std::fs::read_to_string(&log).expect("read log");
        assert_eq!(calls.lines().count(), 4, "{calls}");
    }

    #[test]
    fn transcode_failure_surfaces_the_original_decode_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let clip = dir.path().join("broken.ogg");
        File::create(&clip).expect("clip");
        let cache_dir = tempfile::tempdir().expect("cache");
        // ffmpeg exits non-zero without writing the destination.
        let bad_ffmpeg = dir.path().join("bad-ffmpeg.sh");
        write_script_atomic(
            &bad_ffmpeg,
            "#!/bin/sh\necho 'fake: transcode boom' >&2\nexit 3\n",
        );
        let transcoder =
            Transcoder::with_binary_and_cache(bad_ffmpeg, cache_dir.path().to_path_buf());
        let script = fake_player_failing_once(dir.path(), &log);
        let mut player = PwCatPlayer::new()
            .with_binary(script)
            .with_probe_window(spawn_probe_window())
            .with_transcoder(transcoder.clone());

        let err = loop {
            match player.play(&clip, "broken", 1.0) {
                Ok(_) => panic!("expected the decode error to survive"),
                Err(err) if is_etxtbsy(&err) => thread::sleep(Duration::from_millis(20)),
                Err(err) => break err,
            }
        };
        let text = err.to_string();
        assert!(
            text.contains("sndfile: failed to open audio file"),
            "{text}"
        );
        // No partial file was cached.
        assert_eq!(
            playback_candidates(&clip, &transcoder).len(),
            1,
            "only the original path is a candidate"
        );
    }

    #[test]
    fn disabled_transcoder_keeps_the_decode_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let clip = dir.path().join("broken.ogg");
        File::create(&clip).expect("clip");
        let script = fake_player_failing_once(dir.path(), &log);
        let mut player = PwCatPlayer::new()
            .with_binary(script)
            .with_probe_window(spawn_probe_window())
            .with_transcoder(Transcoder::disabled());
        let err = loop {
            match player.play(&clip, "broken", 1.0) {
                Ok(_) => panic!("expected the decode error to survive"),
                Err(err) if is_etxtbsy(&err) => thread::sleep(Duration::from_millis(20)),
                Err(err) => break err,
            }
        };
        assert!(err.to_string().contains("malformed"), "{err}");
    }

    #[test]
    fn cache_key_is_deterministic_and_sensitive_to_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("clip.wav");
        File::create(&a)
            .expect("a")
            .write_all(b"one")
            .expect("write");
        let same = cache_key(&a).expect("key");
        assert_eq!(cache_key(&a).expect("key"), same, "same file, same key");

        // A different size (re-recorded clip) must change the key.
        let b = dir.path().join("clip2.wav");
        File::create(&b)
            .expect("b")
            .write_all(b"two bytes!")
            .expect("write");
        assert_ne!(cache_key(&b).expect("key"), same);

        // A changed mtime alone must change the key too.
        File::create(&a)
            .expect("a")
            .write_all(b"one!")
            .expect("write");
        let changed = a
            .metadata()
            .expect("meta")
            .modified()
            .expect("mtime")
            .checked_add(std::time::Duration::from_secs(3600))
            .expect("mtime add");
        let file = File::options().write(true).open(&a).expect("reopen");
        file.set_times(std::fs::FileTimes::new().set_modified(changed))
            .expect("set mtime");
        drop(file);
        assert_ne!(
            cache_key(&a).expect("key"),
            same,
            "edited mtime must invalidate the cache entry"
        );

        // The key embeds the readable stem.
        assert!(same.starts_with("clip-"), "{same}");
    }

    #[test]
    fn needs_transcode_for_follows_cache_existence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dst = dir.path().join("cached.wav");
        assert!(needs_transcode_for(Some(&dst)), "missing wav -> transcode");
        File::create(&dst).expect("dst");
        assert!(!needs_transcode_for(Some(&dst)), "existing wav -> reuse");
        assert!(needs_transcode_for(None), "no path -> transcode");
    }

    #[test]
    fn playback_candidates_include_the_cache_twin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = tempfile::tempdir().expect("cache");
        let clip = dir.path().join("clip.ogg");
        File::create(&clip).expect("clip");
        let transcoder = Transcoder::with_binary_and_cache(
            PathBuf::from("/nonexistent/ffmpeg"),
            cache_dir.path().to_path_buf(),
        );

        // Without a cached wav only the original path is a candidate.
        assert_eq!(playback_candidates(&clip, &transcoder), vec![clip.clone()]);

        // Once the fallback has produced the wav, the twin is a candidate.
        let key = cache_key(&clip).expect("key");
        let cached = cache_dir.path().join(format!("{key}.wav"));
        File::create(&cached).expect("cached");
        assert_eq!(
            playback_candidates(&clip, &transcoder),
            vec![clip.clone(), cached]
        );

        // A disabled transcoder never adds candidates.
        assert_eq!(
            playback_candidates(&clip, &Transcoder::disabled()),
            vec![clip]
        );
    }
}

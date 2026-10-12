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
/// How long `play` watches for an immediate (decode) failure.
const PROBE_WINDOW: Duration = Duration::from_millis(300);
/// Grace period for SIGTERM before escalating to SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(2);
/// Poll step for process-state checks.
const POLL_STEP: Duration = Duration::from_millis(10);

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

/// v1 player: spawns `pw-cat` per clip (see module docs).
#[derive(Debug)]
pub struct PwCatPlayer {
    binary: PathBuf,
    target: String,
    probe_window: Duration,
    children: BTreeMap<Handle, RunningChild>,
    next_id: u64,
}

impl PwCatPlayer {
    /// Player targeting [`SINK_MEMES`] with `pw-cat` from `PATH`.
    #[must_use]
    pub fn new() -> Self {
        PwCatPlayer {
            binary: PathBuf::from(PW_CAT),
            target: SINK_MEMES.to_string(),
            probe_window: PROBE_WINDOW,
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

    fn alloc_handle(&mut self) -> Handle {
        let handle = Handle::from_u64(self.next_id);
        self.next_id += 1;
        handle
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
                        "player binary '{}' not found (is pipewire-utils installed?)",
                        self.binary.display()
                    ));
                }
                return Err(anyhow::anyhow!(
                    "failed to spawn {}: {e}",
                    self.binary.display()
                ));
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
                    bail!("player probe failed: {e}");
                }
            }
            thread::sleep(POLL_STEP);
        }
        match exited {
            Some(status) => {
                let stderr = read_first_line(&running.stderr_log);
                remove_log_best_effort(&running.stderr_log);
                bail!(
                    "player exited immediately with {status}{}",
                    stderr.map_or_else(String::new, |line| format!(": {line}"))
                )
            }
            None => {
                // Healthy: pw-cat is quiet on stderr once playing, so the log
                // file is deleted now (the child keeps its fd; the path is
                // never re-created).
                remove_log_best_effort(&running.stderr_log);
                self.children.insert(handle, running);
                Ok(handle)
            }
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
            signal_pid(pid, libc::SIGKILL);
            running
                .child
                .wait()
                .with_context(|| format!("failed to reap {pid} after SIGKILL"))?;
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
    /// # Errors
    /// Fails when a process survives SIGKILL (or `/proc` is unreadable).
    pub fn stop_external(clip: Option<&Path>) -> Result<usize> {
        let found = Self::find_running(clip)?;
        let mut stopped = 0usize;
        for playback in found {
            let pid = playback.pid;
            signal_pid(pid, libc::SIGTERM);
            if !wait_gone(pid, TERM_GRACE) {
                signal_pid(pid, libc::SIGKILL);
                if !wait_gone(pid, TERM_GRACE) {
                    bail!("process {pid} refused to die");
                }
            }
            stopped += 1;
        }
        Ok(stopped)
    }
}

/// True for argument lists of the shape our player spawns:
/// `--playback` plus `--target <our sink>`, optionally restricted to one clip.
fn matches_cat_args(args: &[String], clip: Option<&Path>) -> bool {
    let has_playback = args.iter().any(|a| a == "--playback");
    let has_target = args
        .windows(2)
        .any(|w| w[0] == "--target" && w[1] == SINK_MEMES)
        || args.iter().any(|a| a == &format!("--target={SINK_MEMES}"));
    let clip_matches = match clip {
        None => true,
        Some(clip) => args.iter().any(|a| Path::new(a.as_str()) == clip),
    };
    has_playback && has_target && clip_matches
}

fn signal_group(pid: u32, sig: i32) {
    // The child got process_group(0), so its pgid equals its pid.
    // Safety: kill only sends a signal; it cannot dereference bad pointers.
    let _ = unsafe { libc::kill(-(pid as i32), sig) };
}

fn signal_pid(pid: u32, sig: i32) {
    // Safety: kill only sends a signal; it cannot dereference bad pointers.
    let _ = unsafe { libc::kill(pid as i32, sig) };
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
    fn fake_player(dir: &Path, behavior: &str, log: &Path) -> PathBuf {
        let script = dir.join("fake-pw-cat.sh");
        let mut file = File::create(&script).expect("create fake script");
        write!(
            file,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n{behavior}\n",
            log.display()
        )
        .expect("write fake script");
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake script");
        script
    }

    fn spawn_fake(script: &Path, args: &[&str]) -> Child {
        Command::new(script)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fake")
    }

    fn spawn_probe_window() -> Duration {
        Duration::from_millis(30)
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

        let handle = player.play(&clip, "airhorn", 0.9).expect("play");
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
        let handle = player.play(&clip, "x", 1.0).expect("play");
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
        let handle = player.play(&clip, "x", 1.0).expect("play");
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
        let err = player.play(&clip, "broken", 1.0).expect_err("decode error");
        let text = err.to_string();
        assert!(text.contains("exited immediately"), "{text}");
        assert!(text.contains("fake: unsupported format"), "{text}");
        assert!(player.active().is_empty());
    }

    #[test]
    fn missing_binary_is_a_clear_error() {
        let clip = Path::new("/nonexistent/clip.wav");
        let mut player = PwCatPlayer::new().with_binary(PathBuf::from("/nonexistent/sb-pw-cat"));
        let err = player.play(clip, "x", 1.0).expect_err("not found");
        assert!(err.to_string().contains("not found"), "{err:?}");
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
        let h1 = player.play(&a, "a", 1.0).expect("play a");
        let h2 = player.play(&b, "b", 1.0).expect("play b");
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
        let h1 = player.play(&airhorn, "airhorn", 1.0).expect("play 1");
        sched.started("airhorn", h1);

        // Different clip overlaps.
        let d2 = press(&mut sched, "applause", true);
        assert_eq!(d2, crate::scheduler::Decision::Start { stop: vec![] });
        let h2 = player.play(&applause, "applause", 1.0).expect("play 2");
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
        let h3 = player.play(&airhorn, "airhorn", 1.0).expect("play 3");
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
        let handle = player.play(&clip, "v", 2.0).expect("play");
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
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ntrap 'kill $pid 2>/dev/null; exit 0' TERM\nsleep 30 &\npid=$!\nwait $pid\n",
                log.display()
            ),
        )
        .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
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
}

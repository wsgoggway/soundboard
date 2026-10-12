//! Live peak meter for the virtual microphone source.
//!
//! Captures f32 samples from [`crate::graph::SOURCE_VIRTMIC`] — the native
//! `Audio/Source/Virtual` node (pulse alias `virtmic`, NOT a `*.monitor`;
//! see ADR 0004) — on a dedicated thread with a private libpulse session
//! (the graph session stays owned by the UI worker; all libpulse calls are
//! blocking, so this thread must never be the GTK main thread). Peak
//! amplitude is computed per ~33 ms window and pushed into an
//! `async_channel::Sender<f32>` for the UI `LevelBar`, throttled: a new
//! value is only sent when it differs from the previous one by more than
//! [`SEND_THRESHOLD`]. (async-channel is the current gtk4-rs book pattern
//! for worker→UI communication; the older `glib::MainContext::channel` was
//! removed in glib 0.22.)
//!
//! Failure model: if the source cannot be connected (absent graph, pulse
//! server down) the setup error is returned synchronously by
//! [`MeterHandle::start`] — the caller surfaces it once and the meter stays
//! stopped. If the source disappears mid-run, the pump thread exits silently
//! and the UI simply freezes at the last value. Recovery policy (M3b GUI):
//! the audio worker restarts the meter on EVERY successful `EnsureGraph`
//! (not only when it never started), so «Починить граф» — and the
//! mic-mix/monitoring toggles that trigger a reconcile — heal a frozen or
//! failed meter without an app restart.
//!
//! Readiness protocol: the setup phase sends `Ok(())` over an internal
//! channel the moment the capture stream is live (BEFORE the pump loop —
//! the pump itself only terminates when the meter stops), so a healthy
//! start returns promptly; the timeout in [`MeterHandle::start`] fires only
//! when the setup genuinely stalls. Because the source may be created
//! moments before the meter connects (the graph reconciles right before
//! the meter starts, and the pulse alias appears asynchronously), a failed
//! connect is retried a bounded number of times with a small backoff.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use async_channel::Sender;
use libpulse_binding::context::{Context, FlagSet as ContextFlags, State as ContextState};
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::{FlagSet, PeekResult, State as StreamState, Stream};

/// How often a meter value is pushed to the UI (~30 Hz).
const SEND_INTERVAL: Duration = Duration::from_millis(33);
/// Minimum absolute change between sent values (UI spam guard).
const SEND_THRESHOLD: f32 = 0.01;
/// Poll step of the pump loop.
const PUMP_STEP: Duration = Duration::from_millis(5);
/// Upper bound for one readiness wait (pulse context connect, or the
/// capture stream reaching the READY state).
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Connect attempts for the capture stream when the source is not (yet)
/// visible: the graph may have created it moments before the meter connects
/// (the pulse alias of the native node appears asynchronously), so the
/// first attempt may legitimately fail fast.
const SOURCE_ATTEMPTS: u32 = 4;
/// Backoff between capture-stream connect attempts (small, constant).
const SOURCE_RETRY_DELAY: Duration = Duration::from_millis(300);
/// Upper bound for the whole blocking [`MeterHandle::start`]: the first
/// readiness wait may consume the full [`READY_TIMEOUT`] (a stalled server),
/// while the absent-source retries fail fast (3 × 300 ms backoff ≈ 1 s);
/// the extra second is slack for the context connect.
const SETUP_TIMEOUT: Duration = Duration::from_secs(7);
/// Capture spec requested from the server (it converts from the source's
/// `float32le 2ch 48000Hz`, contract `docs/pipewire-recipe.md`).
const CAPTURE_SPEC: Spec = Spec {
    format: Format::F32le,
    rate: 48_000,
    channels: 1,
};

/// Peak amplitude of `samples`: max `|s|` (unclamped; values above 1.0 are
/// possible with float audio and are clamped by the display layer).
pub fn peak_of(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0_f32, |peak, s| peak.max(s.abs()))
}

/// Peak of a little-endian f32 byte buffer (the wire format of the capture
/// stream), computed by [`peak_of`]. Trailing bytes shorter than one sample
/// are ignored.
fn peak_of_le_bytes(bytes: &[u8]) -> f32 {
    let samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    peak_of(&samples)
}

/// Handle over the running meter thread.
///
/// `start` must be called from a blocking-friendly thread (the UI worker),
/// never from the GTK main thread: the pulse connection is set up
/// synchronously so that a missing source surfaces as an error return.
pub struct MeterHandle {
    stop_flag: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MeterHandle {
    /// Connects to `source` (blocking the caller until the capture stream is
    /// ready, up to [`SETUP_TIMEOUT`]) and spawns the pump thread that sends
    /// peak values into `tx`. On success the meter runs until [`Self::stop`].
    ///
    /// # Errors
    /// Fails when the pulse server is unreachable or the source does not
    /// exist / cannot be opened; the meter is not started in that case.
    pub fn start(source: &str, tx: Sender<f32>) -> Result<Self> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let source = source.to_string();
        let stop = Arc::clone(&stop_flag);
        let thread = thread::Builder::new()
            .name("soundboard-meter".to_string())
            .spawn(move || {
                // Readiness is delivered from inside `pump` as soon as the
                // capture stream is live (the pump loop only ends when the
                // meter stops, i.e. after readiness); only a setup failure
                // that happened before that still needs to reach the
                // receiver here.
                let outcome = pump(source, tx, Arc::clone(&stop), &ready_tx);
                if outcome.is_err() {
                    let _ = ready_tx.send(outcome);
                }
            })
            .context("failed to spawn the meter thread")?;
        match ready_rx.recv_timeout(SETUP_TIMEOUT) {
            Ok(Ok(())) => Ok(MeterHandle {
                stop_flag,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                stop_flag.store(true, Ordering::Relaxed);
                let _ = thread.join();
                bail!("виртуальный микрофон: {e:#}");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                stop_flag.store(true, Ordering::Relaxed);
                let _ = thread.join();
                bail!("виртуальный микрофон: источник не ответил вовремя");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("виртуальный микрофон: поток измерителя завершился аварийно");
            }
        }
    }

    /// Stops the meter thread (blocking up to a few milliseconds) and
    /// disconnects the capture stream. Idempotent.
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for MeterHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Outcome of one capture-stream connect attempt: [`StreamWait::Ready`],
/// a retryable [`StreamWait::SourceUnavailable`] (the source may be created
/// moments before the meter connects) or a [`StreamWait::Fatal`] failure
/// (no retry would help).
enum StreamWait {
    Ready,
    SourceUnavailable(anyhow::Error),
    Fatal(anyhow::Error),
}

/// Pumps the private mainloop until `stream` becomes [`StreamState::Ready`]
/// (bounded by [`READY_TIMEOUT`]) or the attempt fails: a stream that
/// entered `Failed`/`Terminated` means the source is not (yet) there
/// (retryable), anything else is terminal.
fn wait_stream_ready(stream: &Stream, mainloop: &mut Mainloop, source: &str) -> StreamWait {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return StreamWait::Fatal(anyhow::anyhow!("источник {source} не стал готов вовремя"));
        }
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => {
                return StreamWait::Fatal(anyhow::anyhow!(
                    "pulse-мэйнлуп завершился при открытии источника"
                ));
            }
            IterateResult::Err(e) => {
                return StreamWait::Fatal(anyhow::anyhow!(
                    "ошибка pulse-мэйнлупа при открытии источника: {e}"
                ));
            }
        }
        match stream.get_state() {
            StreamState::Ready => return StreamWait::Ready,
            StreamState::Failed | StreamState::Terminated => {
                return StreamWait::SourceUnavailable(anyhow::anyhow!(
                    "источник {source} недоступен (граф не поднят?)"
                ));
            }
            _ => {}
        }
    }
}

/// Sets up the pulse connection and capture stream (with a bounded retry
/// for a source that is not up yet), reports readiness over `ready`, then
/// pumps peaks into `tx` until `stop` is set or the stream dies.
fn pump(
    source: String,
    tx: Sender<f32>,
    stop: Arc<AtomicBool>,
    ready: &std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    // The graph's PulseSession keeps its context private (read-only slice),
    // so the meter connects independently — same recipe, different name.
    let mut mainloop = Mainloop::new().context("не удалось создать pulse-мэйнлуп")?;
    let mut context =
        Context::new(&mainloop, "soundboard-meter").context("не удалось создать pulse-контекст")?;
    context
        .connect(None, ContextFlags::NOFLAGS, None)
        .map_err(|e| anyhow::anyhow!("не удалось начать pulse-соединение: {e}"))?;
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            bail!("pulse-сервер не ответил вовремя");
        }
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => bail!("pulse-мэйнлуп завершился при подключении"),
            IterateResult::Err(e) => bail!("ошибка pulse-мэйнлупа при подключении: {e}"),
        }
        match context.get_state() {
            ContextState::Ready => break,
            ContextState::Failed | ContextState::Terminated => {
                bail!("нет связи с pulse-сервером (запущен ли pipewire-pulse?)")
            }
            _ => {}
        }
    }

    // Bounded retry: a fresh source (the graph reconciles right before the
    // meter starts) may not be visible to the pulse server yet; each failed
    // attempt returns quickly, so a couple of short backoffs cover the
    // publication race without delaying a healthy start.
    let mut stream = None;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..SOURCE_ATTEMPTS {
        if attempt > 0 {
            thread::sleep(SOURCE_RETRY_DELAY);
        }
        let mut candidate = Stream::new(&mut context, "soundboard-meter", &CAPTURE_SPEC, None)
            .ok_or_else(|| anyhow::anyhow!("не удалось создать поток захвата"))?;
        candidate
            .connect_record(Some(&source), None, FlagSet::NOFLAGS)
            .with_context(|| format!("не удалось подключиться к источнику {source}"))?;
        match wait_stream_ready(&candidate, &mut mainloop, &source) {
            StreamWait::Ready => {
                stream = Some(candidate);
                break;
            }
            StreamWait::SourceUnavailable(e) => {
                let _ = candidate.disconnect();
                last_err = Some(e);
            }
            StreamWait::Fatal(e) => {
                let _ = candidate.disconnect();
                return Err(e);
            }
        }
    }
    let mut stream = stream.ok_or_else(|| {
        last_err.unwrap_or_else(|| anyhow::anyhow!("источник {source} недоступен"))
    })?;

    // The capture stream is live: unblock the blocked `start` (it must not
    // wait for the pump to END — the pump runs until the meter is stopped),
    // then enter the pump loop.
    let _ = ready.send(Ok(()));

    let mut last_sent = f32::NAN;
    let mut last_emit = Instant::now();
    let mut window_peak = 0.0_f32;
    while !stop.load(Ordering::Relaxed) {
        match mainloop.iterate(false) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) | IterateResult::Err(_) => break,
        }
        if !matches!(stream.get_state(), StreamState::Ready) {
            break; // source disappeared mid-run: exit quietly
        }
        // Drain everything readable into the current window peak.
        loop {
            match stream.peek() {
                Ok(PeekResult::Data(bytes)) => {
                    window_peak = window_peak.max(peak_of_le_bytes(bytes));
                    let _ = stream.discard();
                }
                Ok(PeekResult::Hole(_)) => {
                    let _ = stream.discard();
                }
                Ok(PeekResult::Empty) | Err(_) => break,
            }
            if stream.readable_size().is_none_or(|n| n == 0) {
                break;
            }
        }
        if last_emit.elapsed() >= SEND_INTERVAL {
            // Window closed: send when the value moved enough, then reset.
            if (window_peak - last_sent).abs() > SEND_THRESHOLD {
                last_sent = window_peak;
                let _ = tx.send_blocking(window_peak);
            }
            last_emit = Instant::now();
            window_peak = 0.0;
        }
        thread::sleep(PUMP_STEP);
    }
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_of_empty_slice_is_zero() {
        assert_eq!(peak_of(&[]), 0.0);
    }

    #[test]
    fn peak_of_silence_is_zero() {
        assert_eq!(peak_of(&[0.0; 128]), 0.0);
    }

    #[test]
    fn peak_of_known_sine_matches_amplitude() {
        // 440 Hz at 48 kHz, amplitude 0.5: over a full period the max |sine|
        // is exactly 1.0, so the peak must equal the amplitude.
        let samples: Vec<f32> = (0..4800)
            .map(|i| 0.5 * (std::f32::consts::TAU * 440.0 * i as f32 / 48_000.0).sin())
            .collect();
        assert!(
            (peak_of(&samples) - 0.5).abs() < 1e-6,
            "{}",
            peak_of(&samples)
        );
    }

    #[test]
    fn peak_of_takes_absolute_value_and_is_unclamped() {
        assert_eq!(peak_of(&[-0.7, 0.2]), 0.7);
        assert_eq!(peak_of(&[-2.0, 1.0]), 2.0);
    }

    #[test]
    fn peak_of_le_bytes_reads_little_endian_f32() {
        let bytes: Vec<u8> = [0.25_f32, -0.5_f32, 0.125_f32]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        assert!((peak_of_le_bytes(&bytes) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn peak_of_le_bytes_ignores_trailing_partial_sample() {
        let mut bytes: Vec<u8> = 0.5_f32.to_le_bytes().to_vec();
        bytes.push(0xFF); // incomplete trailing sample must not panic
        assert!((peak_of_le_bytes(&bytes) - 0.5).abs() < f32::EPSILON);
    }
}

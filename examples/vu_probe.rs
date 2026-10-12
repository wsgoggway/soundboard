//! vu_probe — live peak-level probe for a PipeWire/PulseAudio source.
//!
//! Debugging tool for the soundboard virtual mic: connects to `source`
//! (default `virtmic`, the native Audio/Source/Virtual node — what Zoom
//! selects as "Virtual_Mic") and prints the capture peak about 10 times per
//! second, e.g.
//!
//! ```text
//! 0.413  ################......................
//! 0.087  ###..................................
//! ```
//!
//! Usage: `cargo run --example vu_probe -- [source]`; Ctrl+C to stop.
//!
//! Self-contained on purpose: the capture logic mirrors `src/meter.rs`,
//! but the crate has no library target, so `meter` cannot be imported from
//! an example. Exposing it would require adding `src/lib.rs` — a structural
//! change outside the M3b scope (candidate for M4 if more tools follow).

use std::time::{Duration, Instant};

use anyhow::Context as _;
use libpulse_binding::context::{Context, FlagSet as ContextFlags, State as ContextState};
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::{FlagSet, PeekResult, State as StreamState, Stream};

/// Default source: the virtual mic itself (the native Audio/Source/Virtual
/// node; contract `docs/pipewire-recipe.md`).
const DEFAULT_SOURCE: &str = "virtmic";
/// Capture spec requested from the server (same as the GUI meter).
const CAPTURE_SPEC: Spec = Spec {
    format: Format::F32le,
    rate: 48_000,
    channels: 1,
};
/// Print cadence (~10 lines per second).
const PRINT_INTERVAL: Duration = Duration::from_millis(100);
/// Poll step of the pump loop.
const POLL_STEP: Duration = Duration::from_millis(5);
/// Bar width in characters.
const BAR_WIDTH: usize = 40;
/// Upper bound for the stream to become ready.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

fn main() {
    let source = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_SOURCE.to_string());
    if let Err(e) = run(&source) {
        eprintln!("ошибка: {e:#}");
        std::process::exit(1);
    }
}

fn run(source: &str) -> anyhow::Result<()> {
    let mut mainloop =
        Mainloop::new().ok_or_else(|| anyhow::anyhow!("не удалось создать pulse-мэйнлуп"))?;
    let mut context = Context::new(&mainloop, "soundboard-vu-probe")
        .ok_or_else(|| anyhow::anyhow!("не удалось создать pulse-контекст"))?;
    context
        .connect(None, ContextFlags::NOFLAGS, None)
        .map_err(|e| anyhow::anyhow!("не удалось начать pulse-соединение: {e}"))?;
    loop {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => anyhow::bail!("pulse-мэйнлуп завершился при подключении"),
            IterateResult::Err(e) => anyhow::bail!("ошибка pulse-мэйнлупа при подключении: {e}"),
        }
        match context.get_state() {
            ContextState::Ready => break,
            ContextState::Failed | ContextState::Terminated => {
                anyhow::bail!("нет связи с pulse-сервером (запущен ли pipewire-pulse?)")
            }
            _ => {}
        }
    }

    let mut stream = Stream::new(&mut context, "soundboard-vu-probe", &CAPTURE_SPEC, None)
        .ok_or_else(|| anyhow::anyhow!("не удалось создать поток захвата"))?;
    stream
        .connect_record(Some(source), None, FlagSet::NOFLAGS)
        .with_context(|| format!("не удалось подключиться к источнику {source}"))?;
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!("источник {source} не стал готов вовремя");
        }
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => {
                anyhow::bail!("pulse-мэйнлуп завершился при открытии источника")
            }
            IterateResult::Err(e) => {
                anyhow::bail!("ошибка pulse-мэйнлупа при открытии источника: {e}")
            }
        }
        match stream.get_state() {
            StreamState::Ready => break,
            StreamState::Failed | StreamState::Terminated => {
                anyhow::bail!("источник {source} недоступен (граф не поднят?)")
            }
            _ => {}
        }
    }

    eprintln!("источник: {source}, Ctrl+C — выход");
    let mut last_print = Instant::now();
    let mut window_peak = 0.0_f32;
    // A Quit/Err mainloop ends the probe (the stream is disconnected below).
    while let IterateResult::Success(_) = mainloop.iterate(false) {
        if !matches!(stream.get_state(), StreamState::Ready) {
            anyhow::bail!("источник {source} исчез");
        }
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
        if last_print.elapsed() >= PRINT_INTERVAL {
            println!("{window_peak:.3}  {}", bar(window_peak));
            last_print = Instant::now();
            window_peak = 0.0;
        }
        std::thread::sleep(POLL_STEP);
    }
    let _ = stream.disconnect();
    Ok(())
}

/// Peak of a little-endian f32 byte buffer (max |sample|; mirrors
/// `src/meter.rs`).
fn peak_of_le_bytes(bytes: &[u8]) -> f32 {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .fold(0.0_f32, |peak, s| peak.max(s.abs()))
}

/// Text bar: `#` proportional to the clamped peak, `.` for the rest.
fn bar(peak: f32) -> String {
    let filled = (peak.clamp(0.0, 1.0) * BAR_WIDTH as f32).round() as usize;
    let filled = filled.min(BAR_WIDTH);
    let mut out = String::with_capacity(BAR_WIDTH);
    out.extend(std::iter::repeat_n('#', filled));
    out.extend(std::iter::repeat_n('.', BAR_WIDTH - filled));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_of_le_bytes_reads_little_endian_f32() {
        let bytes: Vec<u8> = [0.25_f32, -0.5_f32, 0.125_f32]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        assert!((peak_of_le_bytes(&bytes) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn bar_spans_zero_to_full() {
        assert_eq!(bar(0.0), format!("{}", ".".repeat(BAR_WIDTH)));
        assert_eq!(bar(1.0), format!("{}", "#".repeat(BAR_WIDTH)));
        assert_eq!(
            bar(0.5),
            format!("{}{}", "#".repeat(BAR_WIDTH / 2), ".".repeat(BAR_WIDTH / 2))
        );
        // Out-of-range peaks clamp instead of panicking.
        assert_eq!(bar(-2.0), bar(0.0));
        assert_eq!(bar(7.5), bar(1.0));
    }
}

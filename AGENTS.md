# soundboard

A Rust + GTK4 soundboard for Zoom on Linux: creates a PipeWire virtual
microphone and plays short meme audio clips into it.

## Stack

- Rust (stable via mise; currently rustc 1.97.0 — mise's `rust` is a symlink
  backend to the rustup toolchain in `~/.cargo/bin`, pinned as `stable` in
  `mise.toml`).
- CLI (done, M1–M2): clap (derive) — `up`/`down`/`doctor`/`list`/`play`/`stop`;
  graph reconcile over the PulseAudio/PipeWire session via
  `libpulse-binding`; playback via a `pw-cat` subprocess (`PwCatPlayer`,
  cross-process stop through `/proc`); clips registry + per-clip-interrupt
  scheduler.
- Module ownership state: `$XDG_STATE_HOME/soundboard/modules.json` records
  the indices we loaded (see `src/graph.rs`); foreign modules are never
  touched.
- GTK4 / gtk4-rs 0.11 (features `v4_10`) + libadwaita 0.9 (feature `v1_4`) —
  the GUI. Landed in M3a: `src/ui/worker.rs` (one worker thread owns all
  blocking audio: PulseSession+Graph+PwCatPlayer+meter; UI talks via
  std mpsc commands + async-channel replies, drained with
  `glib::spawn_future_local` — `glib::MainContext::channel` is gone in
  glib 0.22), `src/meter.rs` (peak meter off `virtmic.monitor` on its own
  thread), `src/ui/window.rs` (doctor status rows, repair/test buttons, VU
  `LevelBar`). Clip grid + toggles — next milestone (M3b). User-facing
  strings are Russian; identifiers/comments English.
- Errors: anyhow (app) + thiserror (typed error enums).

## Commands

- `just build` — `cargo build`
- `just test` — `cargo test`
- `just lint` — `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings`
- `just run` — `cargo run`

Toolchains come via mise only; never install system packages.

## Layout (plan)

- `src/main.rs` — entry point (clap parse, app bootstrap)
- `src/cli.rs` — clap definitions
- `src/config.rs` — config load (~/.config/soundboard/config.toml)
- `src/graph.rs` — PipeWire graph management (null-sink, virtual mic, loopbacks) plus module-ownership state
- `src/player.rs` — clip playback (pw-cat subprocess in v1)
- `src/clips.rs` — clips directory registry
- `src/scheduler.rs` — per-clip interrupt decisions
- `src/meter.rs` — audio level metering (M3)
- `src/ui/` — GTK4/libadwaita UI (M3)

## Docs

- `docs/pipewire-recipe.md` — CONTRACT for M1: the live-verified PipeWire
  module recipe (exact commands, node names, teardown).
- `docs/adr/` — architecture decision records.
- `docs/hld.md` — high-level design (Mermaid), keep in sync with code.

## Approved deviation (host audio e2e)

Audio e2e verification runs on the host audio session (PipeWire), not in
containers — user-approved; only reversible `pactl` module loads allowed
(null-sink / loopback modules created by this project), with mandatory full
teardown after verification. No other host audio state may be touched.

## Rules

- Conventional commits, one commit per task; never `git push`.
- Strict typing: no `unwrap`/`expect` in non-test code paths, explicit return
  types on exported functions, no `unsafe` without a recorded ADR.
- Run `just lint` and `just test` before declaring any task done.

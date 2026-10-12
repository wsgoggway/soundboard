# soundboard

A Rust + GTK4 soundboard for Zoom on Linux: creates a PipeWire virtual
microphone and plays short meme audio clips into it.

## Stack

- Rust (stable via mise; currently rustc 1.97.0 — mise's `rust` is a symlink
  backend to the rustup toolchain in `~/.cargo/bin`, pinned as `stable` in
  `mise.toml`).
- GTK4 / gtk4-rs + libadwaita — planned for the UI (milestone M2+).
- `libpulse-binding` — planned for PulseAudio/PipeWire control (M1+).
- v1 playback strategy: `pw-cat` subprocess as the player (no audio library
  dependency yet).
- CLI: clap (derive). Errors: anyhow (app) + thiserror (typed error enums).

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
- `src/graph.rs` — PipeWire graph management (null-sink, virtual mic, loopbacks)
- `src/player.rs` — clip playback (pw-cat subprocess in v1)
- `src/meter.rs` — audio level metering
- `src/ui/` — GTK4/libadwaita UI

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

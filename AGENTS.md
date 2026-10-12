# soundboard

A Rust + GTK4 soundboard for Zoom on Linux: creates a PipeWire virtual
microphone and plays short meme audio clips into it.

## Stack

- Rust (stable via mise; currently rustc 1.97.0 — mise's `rust` is a symlink
  backend to the rustup toolchain in `~/.cargo/bin`, pinned as `stable` in
  `mise.toml`).
- CLI (done, M1–M2): clap (derive) — `up`/`down`/`doctor`/`list`/`play`/`stop`;
  graph reconcile over two layers (pulse via `libpulse-binding`: null-sink
  `soundboard` + optional monitoring loopback; native via `pw-cli`/`pw-link`
  subprocesses: the `Audio/Source/Virtual` node `virtmic` + pw-link feeds,
  ADR 0004); playback via a `pw-cat` subprocess (`PwCatPlayer`,
  cross-process stop through `/proc`); clips registry + per-clip-interrupt
  scheduler.
- KDE hotkeys (M4): `src/hotkeys.rs` — `soundboard hotkeys` prints a Russian
  clip → `Alt+N` → command table + manual Plasma 6 steps; `--write` generates
  `~/.local/share/applications/net.local.soundboard.<slug>.desktop` files
  (`X-KDE-Shortcuts=Alt+N`, generator marker, timestamped backups, never
  touches foreign files or `kglobalshortcutsrc`). Design + sources:
  `docs/kde-hotkeys.md`.
- Module/node ownership state: `$XDG_STATE_HOME/soundboard/modules.json`
  (schema v2) records the pulse module indices and native node ids we
  created (see `src/graph.rs`); native matching is by `node.name`/port
  pairs, foreign modules and links are never touched.
- GTK4 / gtk4-rs 0.11 (features `v4_10`) + libadwaita 0.9 (feature `v1_4`) —
  the GUI. M3a: `src/ui/worker.rs` (one worker thread owns all blocking
  audio: PulseSession+Graph+PwCatPlayer+Scheduler+meter; UI talks via std
  mpsc commands + async-channel replies, drained with
  `glib::spawn_future_local` — `glib::MainContext::channel` is gone in
  glib 0.22), `src/meter.rs` (peak meter off `virtmic` — the native source
  — on its own thread). M3b: worker-side playback engine (`WorkerCmd::Play`/`Stop`/
  `SaveConfig`, `GuiMsg::PlayingSet`, per-play clip rescan, meter restart on
  every successful reconcile), shared `SharedConfig` (Arc<Mutex<Config>>;
  UI mutates, worker snapshots — `Config::save` runs only on the worker),
  clip grid (FlowBox of buttons, `suggested-action` highlight while
  playing), volume slider (persists on the next explicit save or shutdown),
  mic-mix/monitoring/interrupt `SwitchRow`s, error toasts
  (`adw::ToastOverlay`), graceful SIGINT/SIGTERM (libc handler + atomic +
  main-loop poll → the same close-request path — glib 0.22 has no unix
  signal sources). User-facing strings are Russian; identifiers/comments
  English.
- Errors: anyhow (app) + thiserror (typed error enums).

## Commands

- `just build` — `cargo build`
- `just test` — `cargo test`
- `just lint` — `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings`
- `just run` — `cargo run`
- `cargo run --example vu_probe -- [source]` — live peak probe of a pulse
  source (default `virtmic`, the native virtual mic), ~10 lines/sec,
  Ctrl+C to stop; the debugging tool for the virtual mic meter
  (self-contained: the crate has no lib target, so it mirrors
  `src/meter.rs` instead of importing it)

Toolchains come via mise only; never install system packages.

## Layout

- `src/main.rs` — entry point (clap parse, app bootstrap)
- `src/cli.rs` — clap definitions
- `src/hotkeys.rs` — KDE Plasma hotkey table + `.desktop` generation (M4)
- `src/config.rs` — config load (~/.config/soundboard/config.toml)
- `src/graph.rs` — PipeWire graph management (pulse null-sink + monitoring loopback; native `Audio/Source/Virtual` node `virtmic` + pw-link feeds) plus module/node-ownership state
- `src/player.rs` — clip playback (pw-cat subprocess in v1)
- `src/clips.rs` — clips directory registry
- `src/scheduler.rs` — per-clip interrupt decisions
- `src/meter.rs` — audio level metering (M3)
- `src/ui/` — GTK4/libadwaita UI (M3)
- `examples/vu_probe.rs` — CLI peak probe for debugging the meter (M3b)

## Docs

- `README.md` — user-facing overview: install, quick start, CLI reference,
  troubleshooting, manual e2e checklist.
- `docs/pipewire-recipe.md` — CONTRACT for the graph: the live-verified
  two-layer recipe (pulse modules + native pw-cli node, exact commands,
  port names, SPA-JSON traps, teardown).
- `docs/kde-hotkeys.md` — CONTRACT for M4: how Plasma 6 registers command
  shortcuts (research findings, sources, the `--write` design).
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

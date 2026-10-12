# soundboard

A Rust + GTK4 soundboard for video calls on Linux: it creates a PipeWire
virtual microphone (the native `Audio/Source/Virtual` node `virtmic`, shown
to apps as **Virtual_Mic**) and plays short meme audio clips into it, so a
keypress or click "says" it straight into the call while your real
microphone keeps working. Works with any conferencing app that captures a
microphone: Zoom, Google Meet, Telegram, Jitsi, Discord, …

## Features

- PipeWire virtual microphone built as a native `Audio/Source/Virtual`
  node plus `pw-link` feeds (see `docs/pipewire-recipe.md`,
  [ADR 0004](docs/adr/0004-virtmic-native-source-node.md)) — idempotent
  `up`/`down`/`doctor` management. Conferencing apps list it as a real
  microphone (they filter monitor sources — exactly why the native node is
  used).
- GTK4/libadwaita GUI on a three-view `gtk::Stack` («Саундборд» daily
  board, «Настройки», «Дебаг»): clip grid with playing highlight, live VU
  meter, volume, mic-mix / hear-yourself / re-click-interrupt toggles, a
  failure banner that jumps to the debug view, error toasts.
- CLI playback (`play`/`stop`) that works across processes, including
  stopping playbacks started by an earlier command.
- Per-clip interrupt or overlap playback modes; clips auto-rescan from disk.
- KDE Plasma 6 global hotkeys (`Alt+1..9`) via generated `.desktop` files,
  managed in-app («Настройки» → «Глобальные хоткеи») or from the CLI.
- Russian user-facing strings, English identifiers and docs.

## Architecture

Overview, component responsibilities and the verified audio topology
diagram live in [`docs/hld.md`](docs/hld.md). Short version: clips are
played by a `pw-cat` subprocess into the `soundboard` null-sink, `pw-link`
routes its monitor ports into the native `virtmic` source node, the
conferencing app captures the pulse source `virtmic` ("Virtual_Mic"); with
`mic_mix` the
real microphone is linked into the same inputs (PipeWire mixes multiple
feeds), with `monitoring` a loopback duplicates clips to the real speakers.
Design decisions: [ADR 0001](docs/adr/0001-pwcat-player-process-model.md)
(pw-cat process model), [ADR 0002](docs/adr/0002-kde-hotkeys-via-desktop-files.md)
(KDE hotkeys), [ADR 0003](docs/adr/0003-gui-toolkit-gtk4.md) (GUI toolkit),
[ADR 0004](docs/adr/0004-virtmic-native-source-node.md) (native virtual
mic).

## Requirements

Linux with PipeWire (developed and verified on CachyOS / Arch):

- `pipewire`, `pipewire-pulse` (the pulse-compatible session; `pw-cat`,
  `pw-cli` and `pw-link` ship with the PipeWire audio utilities package,
  `pipewire-audio` on Arch)
- `gtk4`, `libadwaita` (GUI build/runtime libraries)
- Rust toolchain via [mise](https://mise.jdx.dev) (pinned `stable` in
  `mise.toml`)

## Sounds

The repo bundles the meme clips (14 files, `clips/`). Copy them into the
default clips directory (no-clobber) with:

```sh
just clips-install
```

## Install

```bash
mise install            # rust toolchain per mise.toml
cargo install --path .  # installs the `soundboard` binary to ~/.cargo/bin
```

**PATH note:** hotkey files embed the absolute path of the binary that
registered them (`current_exe()`), so they work regardless of `PATH`. If you
move or reinstall the binary elsewhere, re-run `soundboard hotkeys --write`
(or the GUI button) to update the paths.

## Quick start

1. Drop clips into `~/Music/soundboard-clips`
   (`wav`/`mp3`/`ogg`/`oga`/`flac`/`opus`/`m4a`; file stem = clip name).
2. Run `soundboard` — the GUI starts and sets up the audio graph
   automatically at launch.
3. In your conferencing app's audio settings (Zoom: Настройки → Звук;
   Google Meet, Telegram, Jitsi, Discord — similar) set the microphone to
   **Virtual_Mic**. The pulse source is `virtmic` — a real (non-monitor)
   source backed by the native Audio/Source/Virtual node. If the app had
   another device selected, re-select it — apps cache input devices.
4. Click **Тест** in the GUI: a 2 s 440 Hz tone plays and the VU bar
   ("Виртуальный микрофон") reacts.
5. Or headless: `soundboard play airhorn`, `soundboard doctor`,
   `soundboard down` to remove the graph again.

## CLI reference

| Command | What it does |
|---|---|
| `soundboard` (no subcommand) | GTK4/libadwaita GUI (three views: «Саундборд» / «Настройки» / «Дебаг»): auto-reconciles the graph at startup, clip grid + VU meter + volume on the board view, toggles + global hotkeys + clips dir on settings, merged doctor checks + repair buttons on debug; leaves the graph running on close. |
| `soundboard up` | Create or repair the audio graph (idempotent reconcile; prints applied changes). |
| `soundboard down` | Unload soundboard's modules; foreign modules are never touched. |
| `soundboard doctor` | Check the graph state; exit code 1 when problems are found. |
| `soundboard list` | List clips from the configured clips directory. |
| `soundboard play <name>` | Play a clip (non-blocking; the graph is reconciled first automatically). Names resolve by exact match, then unique case-insensitive/prefix match. |
| `soundboard stop [name]` | Stop one clip, or everything without a name (works even while the pulse server is down — matches `pw-cat` playbacks via `/proc`). |
| `soundboard hotkeys [--write]` | Print the clip → `Alt+N` → command table plus manual Plasma 6 steps; `--write` also generates the `.desktop` shortcut files. |

## Configuration

`~/.config/soundboard/config.toml` (TOML; missing file → defaults, unknown
keys / malformed values → error):

| Key | Default | Meaning |
|---|---|---|
| `clips_dir` | `~/Music/soundboard-clips` | Clips directory (leading `~` expanded). |
| `volume` | `0.9` | Playback volume, `0.0..=1.0` (out-of-range values are clamped). The GUI slider persists here with a short debounce, so hotkey-spawned `soundboard play` plays at the shown level. |
| `interrupt_mode` | `true` | Re-clicking a clip restarts it; `false` plays overlapping copies. |
| `mic_mix` | `true` | Pass the real microphone through into the virtual mic. |
| `monitoring` | `false` | Also route clip audio to the real speakers. |

Ownership is tracked in `$XDG_STATE_HOME/soundboard/modules.json`
(schema v2: pulse module indices + native node ids, typically
`~/.local/state/soundboard/modules.json`).

## Hotkeys

`soundboard hotkeys` prints the clip → key → command table; the first 9
clips (sorted) map to `Alt+1`…`Alt+9`. `soundboard hotkeys --write`
generates idempotent, marker-checked
`~/.local/share/applications/net.local.soundboard.<slug>.desktop` files
(`X-KDE-Shortcuts=Alt+N`); foreign files are never touched, overwrites are
backed up. Activate with `kbuildsycoca6` (or re-login). The same management
lives in the app: **Настройки → «Глобальные хоткеи»** shows the table and
the registration state (registered / not registered / stale), **Включить /
Обновить** writes the files and runs `kbuildsycoca6` for you, **Выключить**
removes only soundboard's own files. Details and Plasma internals:
[`docs/kde-hotkeys.md`](docs/kde-hotkeys.md),
[ADR 0002](docs/adr/0002-kde-hotkeys-via-desktop-files.md).

## Manual e2e checklist

Verified once per release on the real desktop session:

1. `soundboard` starts; the graph comes up automatically, summary shows
   «Граф: OK».
2. `soundboard doctor` is all green («все проверки пройдены»), exit code 0.
3. The VU bar reacts to the «Тест» button (440 Hz) and to clips.
4. Clicking a clip is audible in the call with microphone = Virtual_Mic.
5. Interrupt/overlap: re-clicking the same clip restarts it
   (`interrupt_mode` on), different clips play over each other; turning the
   «Перезапускать клип повторным нажатием» toggle off switches to copies.
6. «Стоп всё» stops every GUI playback.
7. Toggles behave: «Микс с микрофоном» passes the real mic into the virtual
   mic, «Слышать себя» duplicates clips to the speakers; both reconcile the
   graph immediately.
8. Ctrl+C (or closing the window) exits gracefully — config saved,
   playbacks and meter stopped, **the graph stays up** for the conferencing
   app.
9. `soundboard hotkeys --write` + `kbuildsycoca6` (or **Настройки →
   «Включить хоткеи»** in the app), then `Alt+1` plays the first clip
   (sorted) even with another window focused.

## Troubleshooting

- **Virtual mic is silent in the call**: run `soundboard doctor` — fix
  whatever fails (or press «Починить граф» / run `soundboard up`), then
  re-select "Virtual_Mic" in the app's audio settings; apps cache the
  previously chosen device. **If Virtual_Mic is missing from the app's mic
  list entirely**: run `soundboard doctor` and re-open the app's audio
  settings after the graph is green (apps usually rescan devices only when
  their settings are reopened).
- **No signal although the graph is green**: probe the source directly with
  `cargo run --example vu_probe --` (defaults to `virtmic`, prints
  ~10 peak levels/second, Ctrl+C to stop). Peaks while playing → capture
  side is fine, check the conferencing app. Flat → check
  `pactl list sink-inputs` while playing.
- **Foreign loopbacks**: soundboard unloads only modules recorded in its
  state file — a lookalike module it never loaded is left alone with a
  warning. If you lost the state file while the graph was up, run
  `soundboard up` once (it adopts the exact-args modules back into the
  state), then `soundboard down` works again.
- **The graph survives app close by design** (the conferencing app keeps
  its device); `soundboard down` removes it completely — including the
  native node (`pw-cli destroy`; a pipewire daemon restart also removes it,
  `up` recreates everything).
- **Do NOT hand-roll the virtual mic** with pulse-level
  `media.class=Audio/Source/Virtual` null-sinks or `pw-loopback` — both are
  live-tested broken on PipeWire 1.6.9 (silently re-targeted to the default
  sink). Use `soundboard up`; the details are in
  [`docs/pipewire-recipe.md`](docs/pipewire-recipe.md).

## Development

```bash
just build   # cargo build
just test    # cargo test
just lint    # cargo fmt --check + clippy -D warnings
just run     # cargo run (GUI)
```

- Toolchains come via mise only; never install system packages.
- Layout: `src/main.rs` (entry, dispatch), `src/cli.rs`, `src/graph.rs`,
  `src/player.rs`, `src/scheduler.rs`, `src/clips.rs`, `src/config.rs`,
  `src/meter.rs`, `src/hotkeys.rs`, `src/ui/` (window + audio worker),
  `examples/vu_probe.rs` (standalone peak probe). See `AGENTS.md` for the
  full map and project rules.
- Documentation: `docs/pipewire-recipe.md` (audio contract),
  `docs/kde-hotkeys.md` (Plasma research), `docs/hld.md` (architecture,
  keep in sync with the code), `docs/adr/` — architecture decision records,
  append-only (Context → Decision → Alternatives → Consequences; a revised
  decision is a new ADR, numbering is never reused).
- Conventional commits; never `git push` from an agent session.

# ADR 0003: GUI toolkit — GTK4 via gtk4-rs + libadwaita

Date: 2026-10-12. Status: accepted (M3a/M3b).

## Context

The soundboard needs a small desktop GUI (one window: doctor status rows,
graph repair/test buttons, a live VU meter, a clip grid, volume, behavior
toggles) written in Rust, running on a KDE Plasma 6 Wayland host. The
salient technical constraint is that **every audio API in this project is
blocking**: libpulse calls pump their own mainloop synchronously, `play`
holds a 300 ms decode-probe window, `stop` can wait up to 2 s. Whatever
toolkit is chosen must coexist with long blocking calls without freezing
the UI.

## Decision

GTK4 through **gtk4-rs 0.11** (`v4_10` feature) with **libadwaita 0.9**
(`v1_4`): a plain libadwaita `ApplicationWindow` + `HeaderBar`/`ToolbarView`,
`FlowBox` clip grid, `LevelBar` meter, `Scale` volume,
`SwitchRow`/`PreferencesGroup` toggles, `ToastOverlay` errors.

Threading pattern (dictated by the toolkit, recorded here because it shapes
the codebase): the glib main loop owns the UI thread, so **all blocking
audio lives on one dedicated worker thread** (`src/ui/worker.rs`), which
owns the pulse session, graph, player, scheduler and meter. The UI sends
`WorkerCmd`s over a `std::sync::mpsc` channel and receives `GuiMsg`s and
meter peaks over `async_channel` receivers drained with
`glib::spawn_future_local` (`glib::MainContext::channel` was removed in
glib 0.22; async-channel is the current gtk4-rs book pattern). Config is
shared as `Arc<Mutex<Config>>`: the UI mutates under the lock, the worker
snapshots before blocking operations and is the only side that persists.

## Alternatives considered

- **Qt6 / QML via cxx-qt**: would look native on Plasma (Breeze widgets),
  but the Rust bindings are immature for production use (young crate,
  QML-centric API, C++ build-system integration); wrapping a small utility
  window in a C++/QML toolchain is disproportionate.
- **Slint**: pure-Rust and pleasant, but a smaller widget ecosystem, its own
  markup language, and no libadwaita-style platform integration; the VU
  meter and grid are doable, but gtk4-rs matched the GNOME HIG widgets
  (SwitchRow, ToastOverlay, LevelBar) out of the box.
- **No GUI (CLI only)**: `play`/`stop` already work headless, but the
  feature requirement is a clickable clip grid with live metering and
  toggles; CLI-only would not meet it.

## Consequences

- **Not visually native on Plasma**: the window renders with libadwaita
  styling, not Breeze/QQC2. Accepted for a utility; it follows the user's
  GTK theme and dark-mode preference and works correctly under Wayland.
- **Mature, memory-safe bindings**: gtk-rs is actively maintained, and the
  widget set needed here is fully covered; system packages `gtk4` and
  `libadwaita` are build/runtime requirements.
- **The glib mainloop dictates the worker-thread pattern** (see Decision):
  every future blocking audio feature must land on the worker, never on the
  UI thread; worker→UI traffic goes through the async channels already in
  place.
- Graceful signal handling had to be hand-rolled: glib 0.22 has no
  unix-signal sources, so SIGINT/SIGTERM are caught with a minimal libc
  handler (async-signal-safe atomic store) and polled into the normal
  close-request path by a main-loop timeout.

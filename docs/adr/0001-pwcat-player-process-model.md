# ADR 0001: pw-cat player process model

Date: 2026-10-12. Status: accepted (M2).

## Context

The v1 player is a `pw-cat` subprocess per clip (`docs/pipewire-recipe.md`).
Decisions were needed around process lifetime, signaling, and how the CLI
stops playbacks that a previous (already exited) CLI invocation started.

## Decision

1. **One `pw-cat` child per clip**, spawned into its own process group
   (`Command::process_group(0)`); handles are opaque `scheduler::Handle(u64)`
   ids shared by the scheduler and the player.
2. **Stop = SIGTERM to the process group, 2 s grace, then SIGKILL + reap.**
   std's `Child::kill` is SIGKILL-only, so signaling goes through a minimal
   `libc` dependency (already in the tree via libpulse-sys). Abrupt kills are
   harmless here: the stream feeds a null-sink, and PipeWire cleans the
   stream up when the connection drops.
3. **stderr to a temp-dir log file, never a pipe**: a long-playing child must
   not block on a full pipe nor die of SIGPIPE when the parent exits. The log
   is deleted as soon as the child proves healthy.
4. **Probe window (300 ms) in `play`**: decode failures exit within
   milliseconds, so a short synchronous window surfaces them (first stderr
   line + exit status) while keeping `play` non-blocking for healthy clips.
5. **Cross-process stop via `/proc/*/cmdline` scan**: `soundboard play` exits
   right after spawning, so `soundboard stop` finds orphans by their exact
   argument shape (`--playback --target soundboard [clip]`), SIGTERMs them,
   SIGKILLs after the grace period. Zombies count as gone (empty cmdline).
   A manually started `pw-cat --target soundboard` also matches — by design.
6. **No reaper thread**: children are reaped by `try_wait` polling on every
   player API call — the simplest correct option for the single-threaded CLI
   and the M3 GTK main loop alike.

The two `libc::kill` call sites (`signal_group`/`signal_pid`) are the only
`unsafe` blocks in the repo and they never create or dereference pointers —
`kill` receives only scalar pid and signal numbers — so the repo's
"no `unsafe` without a recorded ADR" rule is respected, and this ADR is that
record.

## Alternatives considered

- `pulse`/`libpulse` playback in-process: no decoding for mp3/ogg without an
  audio-library dependency; postponed until the GUI needs metering.
- Daemon keeping children (state file with pids): extra moving parts; the
  /proc scan is stateless and self-healing against stale pids.
- Pipes + background drain thread for stderr: SIGPIPE/pipe-full hazards for
  zero benefit.

## Consequences

- `libc` becomes a direct dependency (signal + no other use).
- Orphaned `pw-cat`s write nothing after the stderr log is deleted; killed
  playbacks cut off mid-clip (acceptable for memes).
- The GUI (M3) reuses `Player`/`Scheduler` as-is and additionally gets
  in-process per-clip interrupts via the scheduler.

# ADR 0004: virtmic as a native Audio/Source/Virtual node

Date: 2026-10-12
Status: Accepted (supersedes the pulse null-sink virtual mic from
`docs/pipewire-recipe.md` v1 / M1)

## Context

Zoom filters monitor sources from its microphone list. The M1 design created
the virtual mic as a pulse `module-null-sink` named `virtmic` and captured
its monitor `virtmic.monitor` ("Virtual_Mic"). User-confirmed live test
(device-picker screenshot): Zoom never offers `virtmic.monitor` — it is a
monitor, so it never appears in Settings → Audio → Microphone. The
soundboard was therefore unusable as designed for its primary target app.

A research slice (user-approved live tests, 2026-10-12) verified the
replacement: a NATIVE PipeWire node created with
`pw-cli create-node adapter '{ factory.name=support.null-audio-sink
node.name=virtmic media.class=Audio/Source/Virtual node.description=
Virtual_Mic audio.position=[ FL, FR ] object.linger=true }'`. The pulse
layer exposes it as a REAL source `virtmic` («Monitor of Sink: n/a»,
HARDWARE flag) — and that source IS listed by Zoom. Feeding works through
plain `pw-link` port pairs; multiple feeds into one input port are mixed by
the adapter's soft-mixer (two-tone Goertzel test: both tones at 0.5 FS).

The earlier "failed variants" verdict was too broad: the broken path was
driving `media.class=Audio/Source/Virtual` through the pulse module layer
(`module-null-sink media.class=...` registers no pulse sink and pulse
loopbacks silently fall back to the default sink). The native pw-cli path
needs no pulse sink — the node itself is the source.

## Decision

`src/graph.rs` reconciles TWO layers:

1. Pulse (unchanged rules): null-sink `soundboard`; optional
   `soundboard.monitor -> <default_sink>` loopback while `monitoring`.
2. Native (new):
   - create the `virtmic` adapter node when absent (match by `node.name`
     among `pw-cli ls Node`); reuse when present; if MULTIPLE are found,
     destroy extras keeping the lowest id and warn;
   - always link `soundboard:monitor_{FL,FR} -> virtmic:input_{FL,FR}`;
   - when `mic_mix` is on, link
     `<real_mic>:capture_{FL,FR} -> virtmic:input_{FL,FR}`.
   - Links are matched stateless by directed port pairs (as reported by
     `pw-link -l`): a link is ours iff it touches the `virtmic` node;
     links between other ports are never touched.
   - Teardown: destroy our links by port pair, then
     `pw-cli destroy <virtmic-id>`. The native node has no pulse owner
     module, so `pw-cli` is the only removal path.

State file schema bumped to version 2 (`native_nodes: [{id, name}]` added).
Native matching is BY NAME, not by state: a lost state file is recovered by
the `node.name` / port-pair adoption on the next `up` (a v1 file is dropped
whole and everything re-adopts).

The pulse capture constant changes from `virtmic.monitor` to `virtmic`
(`SOURCE_VIRTMIC`); the meter and `vu_probe` capture it. The user-facing
surface is unchanged: the conferencing app selects "Virtual_Mic", playback
target stays `soundboard`.

## Alternatives considered

- **module-pipe-source + a relay process** (read the soundboard monitor
  from a daemon and write into a FIFO source): a permanent relay process to
  supervise, restart, and own; latency and underrun behavior on the FIFO;
  rejected as the most moving parts for the least fidelity.
- **Duplex application node** (soundboard itself becomes a PipeWire node
  with capture+playback ports): would put the whole DSP in-process; a much
  bigger rewrite of the player model for M7, and the adapter approach was
  already verified. Revisit if per-feed volume control inside the node is
  ever needed.
- **Static `pipewire.conf.d` fragment** declaring the node + links at
  daemon level: survives daemon restarts, but is machine-global config the
  app would have to install AND track (idempotent edits of system config;
  the `down` story becomes "edit /etc-grade files"), and it leaves the
  node running for all users/sessions regardless of the soundboard state.
  Rejected; the app-managed lifecycle (linger node + links) matches the
  existing pulse module model.

## Consequences

- **No pulse owner module**: `virtmic` cannot be removed via `pactl`;
  teardown is `pw-cli destroy` only. Teardown verification must check
  `pw-cli ls Node`, not just `pactl list short modules`.
- **Duplicate-name race**: two concurrent `up` invocations could each
  create a `virtmic` node before either lists the other's. The next `up`
  reconciles the duplicates away (lowest id kept, extras destroyed with a
  warning). Documented, not locked (no server-side name reservation).
- **Node/link lifecycle = daemon lifecycle**: a pipewire daemon restart
  kills the node and its links, exactly like pulse modules die on a
  pipewire-pulse restart. Recovery is the same: run `up` (or reopen the
  GUI, which auto-reconciles at startup).
- Conferencing apps see the source as a real mic-shaped device; the doctor
  check "источник virtmic … не является монитором" pins the regression: if
  the graph ever regresses to a monitor-named-virtmic, doctor fails.
- `pw-cli`/`pw-link` binaries become runtime requirements of the graph
  layer (they ship with pipewire-audio, same as pw-cat).

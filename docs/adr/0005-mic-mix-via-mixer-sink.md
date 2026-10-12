# ADR 0005: mic_mix via the sbmicmix mixer sink

Date: 2026-10-12
Status: Accepted (bug fix on top of ADR 0004)

## Context

User-confirmed bug: ticking «Микс с микрофоном» made the microphone buzz
loudly («микрофон гудит») on the virtual mic / in the call instead of
passing voice.

The old design created DIRECT native port links
`<real_mic>:capture_{FL,FR} -> virtmic:input_{FL,FR}`. Two live-verified,
compounding problems (measurements 2026-10-12):

1. **Suspended-mic ENOENT.** A SUSPENDED ALSA source has no PipeWire ports
   until a capture stream runs on it, so `pw-link` to it fails with ENOENT
   and the link is silently missing — mic_mix worked only while something
   else kept the mic awake.
2. **Self-loop when the default source is virtmic.** `real_mic` was
   resolved as the pulse DEFAULT SOURCE (excluding only `*.monitor`), but
   on the user's session the default source IS `virtmic` — they set
   Virtual_Mic as the system default so Zoom's "Same as System" picks it.
   The binary then linked `virtmic:capture_* -> virtmic:input_*` — a
   digital SELF-LOOP: the adapter mixes its own monitor back into its
   inputs, the node becomes an integrator (`y = input + previous_output`)
   and the level ramps to float saturation. Measured on room noise through
   the broken graph: peak 2.77 FS, RMS 1.20 flat, DC 0.146 — the "buzz"
   was full-scale digital garbage.

The fix pattern was PROVEN LIVE on the user's graph before this ADR was
recorded: null-sink `sbmicmix` + `module-loopback source=<real_mic>
sink=sbmicmix` + `pw-link sbmicmix:monitor_{FL,FR} virtmic:input_{FL,FR}`.
Same 3 s room-noise capture of `virtmic` after the switch: peak
2.77 → **0.21** FS, RMS 1.20 → **0.044** FS — clean.

## Decision

When `mic_mix` is on, the desired state is THREE reconciled pieces:

1. pulse `module-null-sink sink_name=sbmicmix media.class=Audio/Sink
   sink_properties=device.description=SB_MicMix` — the mixer;
2. pulse `module-loopback source=<real_mic> sink=sbmicmix` — keeps the mic
   awake (no ENOENT) and resamples; re-created when the resolved mic
   changes (same exact-args semantics as the monitoring loopback);
3. native links `sbmicmix:monitor_{FL,FR} -> virtmic:input_{FL,FR}`
   (port-pair matched, like the memes feed).

Direct `<mic>:capture_* -> virtmic:input_*` links are NEVER desired; the
reconciler destroys any such link of ours that is not in the desired set —
including pre-existing `virtmic -> virtmic` self-loops (regression-tested;
also flagged by a dedicated `doctor` «самопетля» check).

`real_mic` resolution is a pure function: candidates = pulse sources minus
`virtmic` and all monitors (flag or `*.monitor` name); prefer the current
default source when usable, else the first `alsa_input.*`; none → the mic
path is skipped with a Russian warning and `doctor` FAILs. `doctor` shows
the resolved mic honestly (default vs fallback) and checks the mixer
triple (sink, loopback, links) iff `mic_mix` is on.

## Alternatives considered

- **Direct port links (the old design)** — broken in the two ways above;
  kept only as a documented failed variant in the recipe.
- **PipeWire filter-chain** mixing mic + monitor into virtmic — more
  moving parts (an SPA filter node to author and version), no benefit over
  the pulse mixer sink for a 2-input sum; rejected as complexity without a
  payoff.
- **Refuse to run when the default source is virtmic** (an intermediate
  unshipped draft made mic_mix a no-op with a warning) — rejected: it
  punished the user's sensible setup (virtmic as system default) instead of
  just working via the fallback resolution.

## Consequences

- `mic_mix = true` adds one null-sink and one loopback module (+2 pulse
  modules in `modules.json`); `mic_mix = false` tears them back down.
- The real mic stays awake while mic_mix is on (the loopback is a capture
  stream) — that is the fix for the ENOENT trap, at the usual small idle
  power cost of an open mic.
- The user keeps virtmic as the system default source; resolution falls
  back to the first `alsa_input.*` source, and `doctor` says which mic was
  picked.
- A manual `virtmic:* -> virtmic:*` self-loop (the buzz regression) is
  cleaned by the next `up` and flagged by `doctor`.

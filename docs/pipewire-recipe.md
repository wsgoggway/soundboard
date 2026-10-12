# PipeWire virtual microphone recipe (M1/M7 contract)

Live-verified on the host user audio session: CachyOS, PipeWire 1.6.9
(`pactl 17.0-98-gb096`), PulseAudio-compatible layer. Pulse-level parts
verified 2026-10-12; the native `Audio/Source/Virtual` recipe verified the
same day by the research slice and by M7 live tests. This file is the
CONTRACT for `graph.rs` — code against these exact names and commands.

## Working recipe (verified, current design — ADR 0004, ADR 0005)

The graph spans two layers. The virtual mic is a NATIVE PipeWire node; the
pulse layer provides the playback sink, the mic-mix mixer chain (when
`mic_mix` is on) and the optional monitoring loopback.

```bash
# 1. Soundboard sink (clips are played into this) — pulse layer
pactl load-module module-null-sink sink_name=soundboard \
  media.class=Audio/Sink sink_properties=device.description=Meme_Soundboard

# 2. Virtual microphone — NATIVE PipeWire node (pw-cli, not pactl)
#    (argv form; no shell, no quotes around values)
pw-cli create-node adapter '{ factory.name=support.null-audio-sink \
  node.name=virtmic media.class=Audio/Source/Virtual \
  node.description=Virtual_Mic audio.position=[ FL, FR ] \
  object.linger=true }'

# 3. Feed the virtual mic from the soundboard monitor — always
pw-link soundboard:monitor_FL virtmic:input_FL
pw-link soundboard:monitor_FR virtmic:input_FR

# 4. Mix the real mic in — only when mic_mix is on. THREE pieces (ADR 0005):
#    a dedicated mixer sink + a rate-adapting loopback from the real mic
#    (keeps it awake, no ENOENT, proper resampling) + native links from the
#    mixer monitor. <real_mic> NEVER resolves to virtmic or *.monitor —
#    prefer the pulse default source, fall back to the first `alsa_input.*`.
pactl load-module module-null-sink sink_name=sbmicmix \
  media.class=Audio/Sink sink_properties=device.description=SB_MicMix
pactl load-module module-loopback source=<real_mic> sink=sbmicmix
pw-link sbmicmix:monitor_FL virtmic:input_FL
pw-link sbmicmix:monitor_FR virtmic:input_FR

# 5. Local monitoring — only when monitoring is on (pulse layer)
pactl load-module module-loopback source=soundboard.monitor \
  sink=<default_sink>
```

Playback into the graph (v1 player, pw-cat):

```bash
pw-cat --playback --target soundboard /path/to/clip.wav
```

### pw-cli / pw-link syntax traps (research-verified)

- NO quotes around property values — `node.description=Virtual_Mic`, not
  `node.description="Virtual Mic"`. Inside the braced SPA-JSON payload
  quotes change the meaning and the value would carry the quote characters.
- `audio.position` MUST be the array form `[ FL, FR ]` (one argv token with
  spaces inside the brackets is fine). A bare comma form
  (`audio.position=FL,FR`) breaks SPA-JSON parsing.
- Do NOT set `monitor.channel-volumes` on the node: in live tests it
  corrupted the device description and the channel map.
- `pw-cli create-node` prints NOTHING on success and returns 0 — silence is
  success, do not retry and do not treat empty output as failure. The new
  node id is discovered by re-listing nodes (`pw-cli ls Node`) and matching
  `node.name`.
- `object.linger=true` keeps the node alive after pw-cli exits.

### Why a native node (the monitor-source problem)

Conferencing apps FILTER monitor sources from their microphone lists
(research-verified on Zoom). The old pulse design (null-sink `virtmic`
captured via its monitor `virtmic.monitor`) produced a source that the
apps never offered — user-confirmed on the Zoom device-picker screenshot.
The native `Audio/Source/Virtual` node is exposed by
pipewire-pulse as a REAL source named `virtmic` — «Monitor of Sink: n/a»,
with the HARDWARE flag — and THIS source is what the conferencing app
lists as "Virtual_Mic" (ADR 0004 has the full decision record).

## Exact names as the pulse layer sees them

| Pulse object | Name                 | Notes                                                      |
|--------------|----------------------|------------------------------------------------------------|
| sink         | `soundboard`         | description "Meme_Soundboard"; clips play here             |
| source       | `soundboard.monitor` | monitor of the soundboard sink; feeds the links/loopbacks  |
| sink         | `sbmicmix`           | description "SB_MicMix"; mic-mix mixer (mic_mix only, ADR 0005) |
| source       | `sbmicmix.monitor`   | monitor of the mixer sink; feeds the mic-mix links         |
| source       | `virtmic`            | **NOT a monitor** — the native node itself; THIS is what the conferencing app selects as "Virtual_Mic" |

The native node has NO pulse owner module — it cannot be unloaded via
`pactl`; teardown is `pw-cli destroy <node-id>` (see below). The node and
its links die with the pipewire daemon restart (same lifecycle as pulse
modules).

## Port names (verified)

| Node                    | Ports (DSP `32 bit float mono audio`) |
|-------------------------|----------------------------------------|
| `soundboard`            | outputs `monitor_FL`, `monitor_FR`     |
| `sbmicmix`              | inputs `playback_FL/FR`; outputs `monitor_FL`, `monitor_FR` |
| `virtmic`               | inputs `input_FL`, `input_FR`          |
| real mic (`alsa_input.…`) | outputs `capture_FL`, `capture_FR`   |

Multiple feeds into the same `virtmic` input port are MIXED by the
adapter's soft-mixer (research-verified: two 440 Hz + 1 kHz tones summed,
both recovered at 0.5 FS) — that is why the memes and mic-mix feeds can
share the inputs.

## Mic mix path (ADR 0005)

The real mic reaches the virtual mic through a dedicated mixer chain, NOT
through direct `<mic>:capture_* -> virtmic:input_*` links. Two live-verified
traps kill the direct-link design:

1. **ENOENT on a suspended mic.** A SUSPENDED ALSA source has no PipeWire
   ports until a capture stream runs on it, so `pw-link
   <mic>:capture_FL virtmic:input_FL` fails with ENOENT — the link is
   silently missing until something happens to wake the mic. The pulse
   `module-loopback source=<real_mic> sink=sbmicmix` keeps the mic awake
   (a running capture stream) and does proper rate adaptation.
2. **The default source can be `virtmic` itself.** `pactl
   get-default-source` returned `virtmic` on the user's session (they set
   Virtual_Mic as the system default so Zoom's "Same as System" picks it).
   Resolving "the real mic" as the default source then produced
   `virtmic:capture_* -> virtmic:input_*` SELF-LINKS: the adapter mixes its
   own monitor back into its inputs, the node becomes an integrator and the
   level ramps to float saturation — the user heard a loud buzz instead of
   their voice (measured 2026-10-12: peak 2.77 FS, RMS 1.20 flat, DC 0.146
   on room noise).

Resolution rules (pure function in `graph.rs`, unit-tested): candidates =
all pulse sources excluding `virtmic`, `*.monitor` (by flag or by name);
prefer the current default source when it survives the exclusions, else the
first `alsa_input.*` source; none → the whole mic path is skipped with a
warning and `doctor` FAILs with an actionable Russian message.

Live numbers after switching the user's graph from the broken direct links
to the mixer chain (2026-10-12, 3 s room-noise capture of `virtmic`):
peak 2.77 → **0.21** FS, RMS 1.20 → **0.044** FS — clean, no clipping.

Regression guard: any `virtmic:* -> virtmic:*` link is never desired; the
reconciler destroys pre-existing self-links on `up`, and `doctor` carries a
dedicated «самопетля» check.

## Discovery / teardown

```bash
# Nodes (match id + node.name=virtmic among the entries)
pw-cli ls Node
# Links (port-pair listing; each link appears under BOTH endpoints,
# `|->` marks the output side, `|<-` the input side)
pw-link -l

# Teardown: our links first (same pairs as creation), then the node
pw-link -d soundboard:monitor_FL virtmic:input_FL
pw-link -d soundboard:monitor_FR virtmic:input_FR
pw-link -d sbmicmix:monitor_FL virtmic:input_FL         # when mic_mix was on
pw-link -d sbmicmix:monitor_FR virtmic:input_FR         # when mic_mix was on
pw-cli destroy <virtmic-node-id>

# pulse layer teardown as before (soundboard + sbmicmix modules + loopbacks)
pactl unload-module <index>
```

`soundboard down` does exactly this (links by port pair, node by id from
the name match) and never touches links between ports that are not on our
nodes.

## Failed variants (do NOT use — live-tested, broken)

1. **Pulse-level `module-null-sink` with
   `media.class=Audio/Source/Virtual`** (the pre-M7 design): PipeWire
   registers NO pulse sink for the virtual source, so
   `module-loopback ... sink=virtmic` **silently falls back to the default
   sink** (audio would play out of the real speakers and the mic stays
   silent). `pw-loopback --playback virtmic` is re-targeted to the default
   sink by WirePlumber as well (playback streams may not target
   Source/Virtual nodes). NOTE: `media.class=Audio/Source/Virtual` itself is
   VALID — but only through the NATIVE pw-cli path above, where nothing
   needs a pulse sink: the node IS the source. The broken part was driving
   it (and feeding it) through the pulse module layer.
2. `pw-loopback` (either direction): the capture half mis-binds
   (`soundboard.monitor` is a PulseAudio alias, not a PipeWire node name —
   it binds to the default mic unless
   `--capture-props '{ node.target = "soundboard", stream.capture.sink =
   true }'` is used) and the playback half mis-binds (see 1). Plain
   `pw-link` port pairs are exact and are what the reconciler uses.
3. `pw-record --target <pulse source alias>`: on this setup pw-record ignores
   the target and records from the default microphone (verified: capture
   stream bound to `Source: 55`, the real mic, while asking for virtmic).
   For tests use `parec -d virtmic`, which binds exactly.
4. **`virtmic:capture_* -> virtmic:input_*` self-links (measured
   2026-10-12, the mic_mix buzz bug):** the adapter MIXES its own monitor
   back into its inputs, so the node becomes an integrator
   (`y = input + previous_output`) whose level ramps to float saturation.
   Measured on a real clip through the soundboard sink: monitor peak
   0.749 FS (clean) while virtmic peaked at 3.79 FS with DC drift
   (+0.10) — loud digital garbage on everything the virtual mic carries.
   Triggered when `real_mic` resolved to `virtmic` itself (the pulse
   default source WAS virtmic) and the old binary dutifully created the
   two self-links. Fix (ADR 0005): the mic goes through the `sbmicmix`
   mixer chain, `real_mic` excludes `virtmic`/monitors, the reconciler
   destroys any pre-existing self-links and `doctor` carries a
   «самопетля» check.
   Note: `pw-cli`/user clients cannot destroy these lingering
   (`object.linger=true`, `link.feedback=true`) kernel links directly —
   the app's own teardown (`pw-link -d` by name pair) can.

## End-to-end signal proof (verified numbers)

4 s 440 Hz sine, 0.5 FS amplitude, 48 kHz mono wav, played with
`pw-cat --playback --target soundboard`; captured from the native source
with `parec --format=s16le --rate=48000 --channels=2 -d virtmic`:

- capture binding: pulse `Source` = the native virtmic node (exact)
- peak: **0.500 FS** (equals the generated amplitude exactly; the s16
  stream reads 16383/32768)
- RMS over a 1 s in-signal window: **0.3535 FS** (= 0.5/sqrt(2), the
  signature of a pure 0.5-FS sine)

(Re-verified in M7 live tests on the new topology — see the M7 journal.)

## pw-cat decode support (verified 2026-10-12)

Live check: 1 s 440 Hz sine per format, played with
`pw-cat --playback --target soundboard --volume 1.0`, captured peak of the
raw s16 stream:

| format | generator                       | peak (FS) | verdict |
|--------|---------------------------------|-----------|---------|
| wav    | python3 `wave`, 0.500 FS sine   | **0.500** | decodes (exact) |
| mp3    | ffmpeg lavfi sine, 0.125 FS     | **0.119** | decodes (codec ripple) |
| ogg    | ffmpeg lavfi sine, 0.125 FS     | **0.128** | decodes (codec ripple) |
| flac   | ffmpeg lavfi sine, 0.125 FS     | **0.125** | decodes (exact) |

`pw-cat` exited 0 on every format; no format in the M2 extension list is
known-unplayable. The `--volume` flag is documented by `pw-cat --help`
("Stream volume 0-1.0") and is passed by the player explicitly.

## Post-teardown verification

`soundboard down` is complete when ALL of the following are empty:

```bash
pactl list short modules | grep -E 'soundboard|sbmicmix|virtmic'
pactl list short sinks   | grep -E 'soundboard|sbmicmix'
pactl list short sources | grep -E 'soundboard|sbmicmix|virtmic'
pw-cli ls Node | grep 'node.name = "virtmic"'
pw-link -l | grep -B1 virtmic
```

(A native node has no pulse owner module, so the pulse module grep alone is
NOT enough — the `pw-cli ls Node` check is mandatory.)

# PipeWire virtual microphone recipe (M1 contract)

Live-verified on the host user audio session: CachyOS, PipeWire 1.6.9
(`pactl 17.0-98-gb096`), PulseAudio-compatible layer. Verified 2026-10-12.
This file is the CONTRACT for `graph.rs` (M1) — code against these exact
names and commands.

## Working recipe (verified)

```bash
# 1. Soundboard sink (clips are played into this)
pactl load-module module-null-sink sink_name=soundboard \
  media.class=Audio/Sink sink_properties=device.description=Meme_Soundboard

# 2. Virtual microphone = plain null-sink; apps capture its monitor
pactl load-module module-null-sink sink_name=virtmic \
  media.class=Audio/Sink \
  sink_properties=device.description=Virtual_Mic_sink \
  source_properties=device.description=Virtual_Mic

# 3. Route soundboard monitor into the virtual mic
pactl load-module module-loopback source=soundboard.monitor sink=virtmic
```

Playback into the graph (v1 player, pw-cat):

```bash
pw-cat --playback --target soundboard /path/to/clip.wav
```

## Exact names as PulseAudio sees them (node ids differ per session)

| Pulse object | Name               | Notes                                             |
|--------------|--------------------|---------------------------------------------------|
| sink         | `soundboard`       | description "Meme_Soundboard"                     |
| source       | `soundboard.monitor` | monitor of the soundboard sink                  |
| sink         | `virtmic`          | description "Virtual_Mic_sink"                    |
| source       | `virtmic.monitor`  | THIS is what Zoom selects; description "Virtual_Mic" |

Quirk: `source_name=virtmic` is IGNORED for null-sink monitors on PipeWire
1.6.9 — the monitor source always gets the `.monitor` suffix. Capture apps
must select `virtmic.monitor`. All four nodes negotiate
`float32le 2ch 48000Hz`.

## Failed variants (do NOT use — live-tested, both broken)

1. `module-null-sink media.class=Audio/Source/Virtual sink_name=virtmic`:
   PipeWire registers NO pulse sink for the virtual source, so
   `module-loopback ... sink=virtmic` **silently falls back to the default
   sink** (audio would play out of the real speakers and the mic stays
   silent). `pw-loopback --playback virtmic` is re-targeted to the default
   sink by WirePlumber as well (playback streams may not target
   Source/Virtual nodes).
2. `pw-loopback --capture soundboard.monitor ...`: `soundboard.monitor` is a
   PulseAudio alias, not a PipeWire node name — the capture half binds to the
   default mic instead. Native pw-loopback needs
   `--capture-props '{ node.target = "soundboard", stream.capture.sink = true }'`
   and even then the playback half mis-binds (see 1).
3. `pw-record --target <pulse source alias>`: on this setup pw-record ignores
   the target and records from the default microphone (verified: capture
   stream bound to `Source: 55`, the real mic, while asking for virtmic).
   For tests use `parec -d virtmic.monitor`, which binds exactly
   (verified `Source: 40719` = virtmic.monitor).

## End-to-end signal proof (verified numbers)

4 s 440 Hz sine, 0.5 FS amplitude, 48 kHz mono wav, played with
`pw-cat --playback --target soundboard`; captured with
`parec --format=s16le --rate=48000 --channels=2 -d virtmic.monitor`:

- capture binding: pulse `Source: 40719` = `virtmic.monitor` (exact)
- peak: **16383 / 32768 = 0.500 FS** (equals the generated amplitude exactly)
- RMS over a 1 s in-signal window: **11584 = 0.3535 FS** (= 0.5/sqrt(2),
  the signature of a pure 0.5-FS sine)

## pw-cat decode support (verified 2026-10-12)

Live check: 1 s 440 Hz sine per format, played with
`pw-cat --playback --target soundboard --volume 1.0`, captured from
`virtmic.monitor` (`parec --format=s16le --rate=48000 --channels=2`), peak of
the raw s16 stream:

| format | generator                       | peak (FS) | verdict |
|--------|---------------------------------|-----------|---------|
| wav    | python3 `wave`, 0.500 FS sine   | **0.500** | decodes (exact) |
| mp3    | ffmpeg lavfi sine, 0.125 FS     | **0.119** | decodes (codec ripple) |
| ogg    | ffmpeg lavfi sine, 0.125 FS     | **0.128** | decodes (codec ripple) |
| flac   | ffmpeg lavfi sine, 0.125 FS     | **0.125** | decodes (exact) |

`pw-cat` exited 0 on every format; no format in the M2 extension list is
known-unplayable. The `--volume` flag is documented by `pw-cat --help`
("Stream volume 0-1.0") and is passed by the player explicitly.

## Extra loopback checks (verified loading cleanly)

```bash
# real mic -> virtual mic (for pass-through mode)
pactl load-module module-loopback \
  source=alsa_input.pci-0000_00_1f.3-platform-sof_sdw.HiFi__Mic__source \
  sink=virtmic

# soundboard monitor -> default sink (for local monitoring)
pactl load-module module-loopback source=soundboard.monitor \
  sink=alsa_output.usb-XREAL_XREAL_1S-05.analog-stereo
```

Both loaded and unloaded cleanly (default device names are session-specific —
discover them at runtime via `pactl get-default-sink` / `pactl get-default-source`).

## Teardown (mandatory)

```bash
for m in $(pactl list short modules | grep -E 'soundboard|virtmic' | awk '{print $1}'); do
  pactl unload-module "$m"
done
# verify empty:
pactl list short modules | grep -E 'soundboard|virtmic'
```

Post-teardown verification on 2026-10-12: no `soundboard`/`virtmic` modules,
sinks, sources, or `pw-loopback` processes remained.

# MIDI + VST hosting: research and plan (branch `midi-vst`)

## Goal

Let Magic Mixer host VST instruments and effects, and take MIDI in, without
breaking the "anyone can use it" patch-bay UI:

```
SOURCES                  MIDDLE                         OUTPUTS
Spotify  ─────────────►  Tape Delay (effect) ─────► Speakers
USB keyboard ─MIDI─► Synth (instrument) ──────►  CABLE Input ─► Discord
```

## What the test PC had (checked 2026-11-24)

| Thing | Status |
|---|---|
| VST3 plugins | one instrument and three effects from a single vendor. VST2 copies too. |
| MIDI hardware | none connected |
| Windows MIDI Services (new MIDI 1.0/2.0 stack) | service running; only internal test loopbacks exist (hidden from apps) |
| Shareable loopback / virtual MIDI ports | not created; needs Microsoft's "Windows MIDI Services Tools" / App SDK |

## Spike results (in this branch)

- `magic-mixer plugins`: scans `%CommonProgramFiles%\VST3`, probes **each
  plugin in its own child process** (crash/hang isolation, 20 s timeout) via
  the VST3 COM ABI with hand-written vtables. Correctly classified all 4
  plugins (Instrument|Synth, Fx|Modulation, Fx|Delay, Fx|EQ).
- `magic-mixer midi`: lists MIDI ports (midir / WinMM). Today: only
  "Microsoft GS Wavetable Synth".
- `magic-mixer midi-test [send] [listen]`: sends a note through a loopback
  pair and checks it arrives. Ready for when loopback ports exist.

## Facts the plan relies on

- **VST3 SDK is MIT-licensed from 3.8 (Oct 2025).** Commercial use, no source
  disclosure; the "VST" logo is optional and has its own trademark rules.
- **VST2**: SDK no longer licensed to new developers. Skip it; all plugins here
  also ship as VST3.
- **CLAP**: open (MIT) format; a good second format later via `clack-host`.
- **Rust hosting crates are young**: `vst3` (raw bindings), `vst3-host`
  (high-level, mostly exercised on macOS), `rack`, `plugin_host`. None is a
  proven Windows host yet, so we keep our own thin layer over `vst3` bindings
  (the spike shows the ABI is straightforward) and evaluate `vst3-host`.
- **Windows MIDI Services** ships in Windows 11 (24H2+); built-in loopbacks
  and app-created virtual devices come with the separate App SDK / Tools.

## Design

### 1. Audio engine: from pipes to a small graph
Today every connection is an independent pipe (capture → ring → playback).
Plugins need a node in the middle:

- **Plugin node** = one thread with its own block clock (48 kHz, 256-frame
  blocks ≈ 5.3 ms). Each block: sum all incoming rings → `process()` → push
  to every outgoing ring. Same ring buffers and latency cap as today, so the
  existing routes keep working unchanged.
- **Instrument node** = the same, but its input is MIDI events instead of
  audio.
- Clock drift between devices is absorbed by the rings' 150 ms cap (as now).
  A proper drift-corrected resampler is a later improvement.

### 2. Plugins run in a separate host process
`magic-mixer-host.exe` (one per plugin node). Reasons: a crashing or
copy-protected plugin can't take the mixer or your audio down, and 32/64-bit
or misbehaving plugins can be handled later. Audio moves over a shared-memory
ring; control (parameters, state, open editor) over a named pipe.
Phase 1 may run plugins in-process to move faster, then switch.

### 3. Plugin windows
Plugin editors (IPlugView) need a native window. The host process opens a
separate window per plugin ("Open plugin" button on the card), which is how
most hosts do it; we don't embed them in the egui window.

### 4. MIDI
- Phase 1: `midir` for hardware controllers (USB/Bluetooth MIDI keyboards).
- Phase 2: Windows MIDI Services App SDK to create a **"Magic Mixer" virtual
  MIDI port**, so Ableton/other apps can play hosted instruments, and to see
  MIDI 2.0 devices.

### 5. UI
- **Instruments** appear in **Sources** (they make sound), with a "MIDI from:"
  picker on the card.
- **Effects** appear in the **middle column** under "Effects", next to the
  recording apps: click Spotify → Tape Delay → Speakers.
- "+ Add effect / instrument" opens a list from the plugin scan (cached; the
  scan already runs out-of-process).
- Plugin state and settings are saved with connections.

## Phases

1. **Effects in-process**: load VST3, process audio, one effect node in the
   middle column, saved state. Test: Spotify → Tape Delay → Speakers.
2. **Instruments + MIDI in**: Synth as a source played from a MIDI
   keyboard (needs a controller to test, or phase 3's virtual port).
3. **Out-of-process host**, plugin editor windows.
4. **Virtual MIDI port** via Windows MIDI Services App SDK; CLAP support.

## Risks / open questions

- Some commercial plugins need their own activation (the vendor's installer); they
  behave normally inside any VST3 host, but a plugin that isn't activated
  will run in demo mode.
- Latency: each plugin node adds ~1 block (5 ms) plus ring buffering.
- No MIDI controller here to test phase 2 end to end until one is connected
  or the virtual port exists.
- Windows MIDI Services Tools/App SDK is a separate install (preview/RC
  status should be re-checked before phase 4).

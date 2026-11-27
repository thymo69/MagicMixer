# Magic Mixer

Per-app audio routing for Windows 10 (2004+) / 11. Send any app's sound to any
output: your speakers, or a virtual cable (VB-Cable) that Discord, OBS or
Ableton can record from.

Version 0.1.0.

## Features

- Click-to-patch GUI: sources -> recording/effects apps -> outputs
- Per-app capture (WASAPI process loopback), per-connection volume and mute
- Microphones and other inputs as sources
- Connections saved between runs and reconnected when an app reopens
- VST3 plugin racks (instruments and effects), MIDI keyboard, on-screen keys,
  MIDI from other apps
- Command-line tool for scripting routes and setting the default device

## Requirements

- Windows 10 version 2004 or later, or Windows 11
- [Rust](https://rustup.rs) (stable, MSVC toolchain) to build
- [VB-CABLE](https://vb-audio.com/Cable/) to feed audio into another app
  (Discord, OBS, Ableton)
- Optional: Windows MIDI Services Tools (preview) for MIDI from other apps
- Optional: VST3 plugins in `%CommonProgramFiles%\VST3`

## Build and run

```
cargo build --release
target\release\magic-mixer-gui.exe
```

Tip: to keep build output out of a synced folder such as OneDrive, add a
local `.cargo/config.toml` with `[build] target-dir = "..."` (it is ignored
by git).

## Install

```
powershell -ExecutionPolicy Bypass -File install.ps1
```

Builds, copies the app to `%LOCALAPPDATA%\Programs\MagicMixer`, and adds
"Magic Mixer" shortcuts to the Start menu and Desktop. No admin needed.
Re-run it to update.

## The app (`magic-mixer-gui.exe`)

Three columns follow the signal from left to right:

| SOURCES | RECORDING / EFFECTS APPS | OUTPUTS |
|---|---|---|
| apps playing sound (Spotify, a game) | Ableton, Discord, OBS, ... | speakers, headphones, cables |

1. Click a source, then click a recording app (or an output). A cable appears.
2. Click a recording app, then an output, to send *its* sound on.
3. Use the Connections bar at the bottom for volume, mute and disconnect.

Recording apps are fed through VB-Cable: Magic Mixer plays the source into
"CABLE Input", and the app must record from "CABLE Output". The card says
so, and shows "ready" when Windows can confirm the app is listening there
(apps using ASIO, like Ableton often does, can't be seen, so the hint stays).
Right-click any app to move it between the Sources and Recording columns.
One VB-Cable is shared: every app recording from "CABLE Output" hears
everything sent into it.

Click an existing app-to-output pair again to disconnect it. Press Esc to
cancel a selection. If an app closes and reopens, its cables reconnect
automatically, keeping their volume and mute.

To feed another app: send it to **CABLE Input**, then choose **CABLE Output**
as the microphone or input in Discord or Ableton.

Connections are saved automatically (`%APPDATA%\MagicMixer\connections.json`)
and come back next time you open Magic Mixer. A saved connection whose app
isn't running waits in the list ("Will connect when ... plays sound") and
starts by itself when the app makes sound; "Forget" removes it. Only one
copy of Magic Mixer runs at a time; opening it again shows the open window.

Extra start-up connections: `magic-mixer-gui --connect "spotify=CABLE Input"` (repeatable).

## Plugins (VST3)

- **Plugin racks** hold a chain of VST3 plugins. Every plugin gets a quick
  sound check when first scanned: does it *make* sound from MIDI, *change* the
  audio sent into it, or *both*? Racks then sit in the matching lane:
  Sources (makes sound), **Makes + changes sound**, or Effects.
- Adding or swapping a plugin opens its own window; plugins without one get
  Magic Mixer's slider window. Click a plugin's name to reopen it.
- Instruments play from a MIDI keyboard, the on-screen keys, or **other apps**:
  in Ableton (or any app) set the MIDI output to **"Send to Magic Mixer"**, and
  in the rack choose *MIDI from: Other apps*. This loopback comes from
  Windows MIDI Services Tools (preview); Magic Mixer recreates it at start-up.

## Command line (`magic-mixer.exe`)

```
magic-mixer apps                              # apps with audio open, live level
magic-mixer devices                           # playback + recording devices
magic-mixer route spotify "CABLE Input" 80    # route at 80% gain
magic-mixer meter "CABLE Output" 5            # level arriving at a device
magic-mixer default "Microphone Array"        # set the Windows default device
```

## Layout

| file                       | role                                                     |
|----------------------------|----------------------------------------------------------|
| `src/sources.rs`           | list apps with audio; per-process loopback capture        |
| `src/outputs.rs`           | list/find devices; play into a device; meter; set default |
| `src/routes.rs`            | one route = source + output + gain/mute, two threads      |
| `src/mixer.rs`             | the set of live routes; connect/disconnect/reattach       |
| `src/saved.rs`             | saved connections (load/save JSON, matching)              |
| `src/icon.rs`              | app icon drawn in code (window icon + .ico for shortcuts) |
| `src/ring.rs`              | FIFO joining capture to playback (latency capped 150 ms)  |
| `src/com.rs`               | COM init, the shared 48 kHz stereo float format           |
| `src/main.rs`              | CLI front end                                             |
| `src/midi.rs`, `src/rack.rs`, `src/plugins.rs`, `src/vst3host.rs` | MIDI input, plugin racks, VST3 scan and hosting |
| `src/bin/magic-mixer-gui/` | GUI front end (egui)                                    |

Build: `cargo build --release` (see Build and run above).

## Known limits

- "CABLE In 16ch" is hidden: it is the same VB-Cable as "CABLE Input", and the
  cable only accepts one of the two at a time.
- Bluetooth headsets: when an app opens the headset's microphone, Windows
  switches it to hands-free mode (low-quality mono audio). Use a separate
  microphone to keep stereo playback.
- Apps that use ASIO or exclusive mode can't be seen or captured per app.
- One VB-Cable is shared by every app recording from "CABLE Output".
- MIDI from other apps needs the Windows MIDI Services preview.
- No automated tests yet.
- No license chosen yet; all rights reserved until one is added.

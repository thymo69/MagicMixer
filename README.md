# Magic Mixer

Per-app audio routing for Windows 10 (2004+) / 11. Send any app's sound to any
output: your speakers, or a virtual cable (VB-Cable) that Discord, OBS or
Ableton can record from.

## The app (`magic-mixer-gui.exe`)

1. Click an app on the left (only apps that have made sound are listed).
2. Click where its sound should go on the right. A cable appears.
3. Use the Connections bar at the bottom for volume, mute and disconnect.

Click an existing app-to-output pair again to disconnect it. Press Esc to
cancel a selection. If an app closes and reopens, its cables reconnect
automatically, keeping their volume and mute.

To feed another app: send it to **CABLE Input**, then choose **CABLE Output**
as the microphone or input in Discord or Ableton.

Start-up connections: `magic-mixer-gui --connect "spotify=CABLE Input"` (repeatable).

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
| `src/ring.rs`              | FIFO joining capture to playback (latency capped 150 ms)  |
| `src/com.rs`               | COM init, the shared 48 kHz stereo float format           |
| `src/main.rs`              | CLI front end                                             |
| `src/bin/magic-mixer-gui.rs` | GUI front end (egui)                                    |

Build: `cargo build --release`. Output goes to `%LOCALAPPDATA%\magic-mixer-target`
(see `.cargo/config.toml`) to keep it out of OneDrive.

## Known limits

- "CABLE In 16ch" is hidden: it is the same VB-Cable as "CABLE Input", and the
  cable only accepts one of the two at a time.
- Connections are not saved between runs yet.

# magic-mixer (backend, step 1)

Command-line audio router for Windows 10 (2004+) / 11. Patches one app's audio
into any playback device, using WASAPI per-process loopback. No drivers needed
for the routing itself; to feed another app (Discord, Ableton) you route into a
virtual cable (VB-Cable, Voicemeeter) and pick that cable's recording side there.

```
magic-mixer apps                              # apps with audio open, live level
magic-mixer devices                           # playback + recording devices
magic-mixer route spotify "CABLE Input" 80    # route at 80% gain
magic-mixer meter "CABLE Output" 5            # level arriving at a device
```

While a route runs: type a number (0-400) for gain %, `m` to mute, `q` to stop.

## Layout

| module         | role                                                        |
|----------------|-------------------------------------------------------------|
| `sources.rs`   | list apps with audio sessions; per-process loopback capture  |
| `outputs.rs`   | list/find devices; play into a device; level meter           |
| `routes.rs`    | a route = source + output + gain/mute, two threads           |
| `ring.rs`      | FIFO joining capture to playback (latency capped at 150 ms)  |
| `com.rs`       | COM init, the shared 48 kHz stereo float format              |

Build output goes to `%LOCALAPPDATA%\magic-mixer-target` (see `.cargo/config.toml`)
to keep it out of OneDrive.

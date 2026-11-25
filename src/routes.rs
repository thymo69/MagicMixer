//! Routes: a patch cable from one source (an app) to one output (a device),
//! with its own gain and mute. Each route runs two threads (capture + playback)
//! joined by a small ring buffer, so routes are fully independent of each other.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::com::{CHANNELS, SAMPLE_RATE};
use crate::outputs::{self, Device};
use crate::ring::Ring;
use crate::sources::{self, AppSource};

/// Lock-free gain + mute, adjustable while the route is playing.
pub struct Gain {
    linear: AtomicU32, // f32 bits
    muted: AtomicBool,
    peak: AtomicU32, // f32 bits, post-gain peak since last read
}

impl Gain {
    pub fn new(linear: f32) -> Self {
        Gain { linear: AtomicU32::new(linear.to_bits()), muted: AtomicBool::new(false), peak: AtomicU32::new(0) }
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.linear.load(Ordering::Relaxed))
    }
    pub fn set(&self, linear: f32) {
        self.linear.store(linear.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    pub fn toggle_mute(&self) -> bool {
        !self.muted.fetch_xor(true, Ordering::Relaxed)
    }
    /// Called by the playback thread with the peak of each block it writes.
    pub fn report_peak(&self, p: f32) {
        let _ = self.peak.fetch_max(p.to_bits(), Ordering::Relaxed); // positive f32 bits sort like the floats
    }
    /// Peak output level since the last call (0.0..1.0).
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }
    pub fn effective(&self) -> f32 {
        if self.is_muted() { 0.0 } else { self.get() }
    }
}

pub struct Route {
    pub id: u64,
    pub source: AppSource,
    pub output: Device,
    pub gain: Arc<Gain>,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    threads: Vec<JoinHandle<()>>,
    /// Racks this route is attached to (detached on stop).
    racks: Vec<u64>,
    ring: Arc<Ring>,
}

/// Run one side of a route on its own thread. If it fails, record the error
/// and stop the other side too.
fn spawn_side(
    label: &'static str,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    work: impl FnOnce() -> windows::core::Result<()> + Send + 'static,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(label.into())
        .spawn(move || {
            if let Err(e) = work() {
                error.lock().unwrap().get_or_insert_with(|| describe_error(label, &e));
                stop.store(true, Ordering::Relaxed);
            }
        })
        .expect("spawn route thread")
}

/// Turn a WASAPI error into something a non-expert can act on.
fn describe_error(label: &str, e: &windows::core::Error) -> String {
    let code = e.code().0 as u32;
    let plain = match code {
        0x8889_000A => "the device is busy (another app or device is using it exclusively)",
        0x8889_0004 => "the device was unplugged or disabled",
        0x8889_0008 => "the device doesn't support this audio format",
        0x8007_0005 => "access denied",
        0x8007_0057 if label == "capture" => "couldn't capture that app (it may have closed)",
        _ => "",
    };
    if plain.is_empty() {
        format!("{label} failed: {} (0x{code:08X})", e.message().trim())
    } else {
        format!("{plain} (0x{code:08X})")
    }
}

impl Route {
    /// Start routing `source` into `output`.
    pub fn start(id: u64, source: AppSource, output: Device, gain: f32) -> Route {
        // Cap latency at ~150 ms; anything older is dropped.
        let ring = Arc::new(Ring::new(SAMPLE_RATE as usize * CHANNELS as usize * 150 / 1000));
        let gain = Arc::new(Gain::new(gain));
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));

        let mut threads = Vec::new();
        let mut racks = Vec::new();
        let missing = |error: &Arc<Mutex<Option<String>>>| {
            error.lock().unwrap().get_or_insert_with(|| "that plugin rack no longer exists".into());
        };

        // Source side: a rack's output, or capture from an app / input device.
        match crate::rack::rack_id(&source.name) {
            Some(rid) => match crate::rack::get(rid) {
                Some(r) => {
                    r.attach_output(id, ring.clone());
                    racks.push(rid);
                }
                None => missing(&error),
            },
            None => {
                let (ring, stop2, pid, input) = (ring.clone(), stop.clone(), source.pid, source.input_device.clone());
                threads.push(spawn_side("capture", stop.clone(), error.clone(), move || match input {
                    Some(dev) => outputs::capture_device(&dev, ring, stop2),
                    None => sources::capture_app(pid, ring, stop2),
                }));
            }
        }
        // Destination side: into a rack, or play to a device.
        match crate::rack::rack_id(&output.id) {
            Some(rid) => match crate::rack::get(rid) {
                Some(r) => {
                    r.attach_input(id, ring.clone(), gain.clone());
                    racks.push(rid);
                }
                None => missing(&error),
            },
            None => {
                let (stop2, gain, dev, ring) = (stop.clone(), gain.clone(), output.id.clone(), ring.clone());
                threads.push(spawn_side("playback", stop.clone(), error.clone(), move || {
                    outputs::play_to_device(&dev, ring, gain, stop2)
                }));
            }
        }
        Route { id, source, output, gain, stop, error, threads, racks, ring }
    }

    /// The first error either side hit, if any. A failed route has stopped.
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    /// True if either side of the route has exited.
    pub fn has_stopped(&self) -> bool {
        self.threads.iter().any(|t| t.is_finished())
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        for rid in self.racks.drain(..) {
            if let Some(r) = crate::rack::get(rid) {
                r.detach(&self.ring);
            }
        }
    }

    /// Stop the route and return its error, if it had one.
    pub fn stop(mut self) -> Option<String> {
        self.shutdown();
        self.error()
    }
}

impl Drop for Route {
    fn drop(&mut self) {
        self.shutdown();
    }
}

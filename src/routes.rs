//! Routes: a patch cable from one source (an app) to one output (a device),
//! with its own gain and mute. Each route runs two threads (capture + playback)
//! joined by a small ring buffer, so routes are fully independent of each other.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
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

#[allow(dead_code)] // source/output are read by the upcoming GUI
pub struct Route {
    pub source: AppSource,
    pub output: Device,
    pub gain: Arc<Gain>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<windows::core::Result<()>>>,
}

impl Route {
    /// Start routing `source` into `output`.
    pub fn start(source: AppSource, output: Device, gain: f32) -> Route {
        // Cap latency at ~150 ms; anything older is dropped.
        let ring = Arc::new(Ring::new(SAMPLE_RATE as usize * CHANNELS as usize * 150 / 1000));
        let gain = Arc::new(Gain::new(gain));
        let stop = Arc::new(AtomicBool::new(false));

        let cap = {
            let (ring, stop, pid) = (ring.clone(), stop.clone(), source.pid);
            std::thread::Builder::new()
                .name(format!("capture-{pid}"))
                .spawn(move || sources::capture_app(pid, ring, stop))
                .expect("spawn capture thread")
        };
        let play = {
            let (ring, stop, gain, id) = (ring, stop.clone(), gain.clone(), output.id.clone());
            std::thread::Builder::new()
                .name("playback".into())
                .spawn(move || outputs::play_to_device(&id, ring, gain, stop))
                .expect("spawn playback thread")
        };
        Route { source, output, gain, stop, threads: vec![cap, play] }
    }

    /// True if either side of the route has exited (e.g. on an error).
    pub fn has_failed(&self) -> bool {
        self.threads.iter().any(|t| t.is_finished())
    }

    /// Stop the route and return any error from its threads.
    pub fn stop(self) -> Vec<String> {
        self.stop.store(true, Ordering::Relaxed);
        let labels = ["capture", "playback"];
        self.threads
            .into_iter()
            .zip(labels)
            .filter_map(|(t, label)| match t.join() {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(format!("{label}: {e}")),
                Err(_) => Some(format!("{label}: thread panicked")),
            })
            .collect()
    }
}

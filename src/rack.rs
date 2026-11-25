//! Racks: containers for VST3 plugins.
//!
//! A rack is a node in the signal graph. Routes can feed audio *into* it
//! (sources -> rack) and take audio *out* of it (rack -> outputs / other racks).
//! Each rack runs its own processing thread in 256-frame blocks:
//!
//!   sum of inputs (each with its own volume)  ->  plugin chain  ->  every output
//!
//! An instrument rack (first plugin is an instrument) is played by MIDI
//! instead of audio input. The chain can be changed while audio is running
//! ("hot swap"): the UI thread swaps plugins under a short lock, and the old
//! plugin is shut down on the UI thread, never on the audio thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::ring::Ring;
use crate::routes::Gain;
use crate::vst3host::{Plugin, BLOCK, SAMPLE_RATE};

/// Audio feeding into a rack from one route.
struct Input {
    route: u64,
    ring: Arc<Ring>,
    gain: Arc<Gain>,
    /// Waits for ~20 ms of buffered audio before reading, so the rack never
    /// reads half-empty buffers (which would click).
    primed: bool,
}

pub struct Rack {
    pub id: u64,
    name: Mutex<String>,
    /// The plugin chain, in order. Locked briefly by the audio thread per block.
    chain: Mutex<Vec<Plugin>>,
    inputs: Mutex<Vec<Input>>,
    outputs: Mutex<Vec<(u64, Arc<Ring>)>>,
    /// Raw MIDI messages waiting for the next block.
    midi: Mutex<Vec<[u8; 3]>>,
    /// Output level (f32 bits), peak since last read.
    out_peak: AtomicU32,
    stop: AtomicBool,
}

fn registry() -> &'static Mutex<HashMap<u64, Arc<Rack>>> {
    static R: OnceLock<Mutex<HashMap<u64, Arc<Rack>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Racks appear to routes as a source named "rack:<id>" and as an output
/// device with id "rack:<id>".
pub fn rack_id(name_or_id: &str) -> Option<u64> {
    name_or_id.strip_prefix("rack:")?.parse().ok()
}

/// A rack's output as a route source.
pub fn as_source(id: u64) -> crate::sources::AppSource {
    crate::sources::AppSource { pid: 0xC000_0000 | (id as u32 & 0x3FFF_FFFF), name: format!("rack:{id}"), active: true, ..Default::default() }
}

/// A rack's input as a route destination.
pub fn as_output(id: u64) -> crate::outputs::Device {
    crate::outputs::Device {
        id: format!("rack:{id}"),
        name: format!("rack:{id}"),
        direction: crate::outputs::Direction::Output,
        is_default: false,
    }
}

/// Find a running rack by id.
pub fn get(id: u64) -> Option<Arc<Rack>> {
    registry().lock().unwrap().get(&id).cloned()
}

/// Create a rack and start its processing thread.
pub fn create(id: u64, name: &str) -> Arc<Rack> {
    let rack = Arc::new(Rack {
        id,
        name: Mutex::new(name.to_string()),
        chain: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
        outputs: Mutex::new(Vec::new()),
        midi: Mutex::new(Vec::new()),
        out_peak: AtomicU32::new(0),
        stop: AtomicBool::new(false),
    });
    registry().lock().unwrap().insert(id, rack.clone());
    let r = rack.clone();
    std::thread::Builder::new()
        .name(format!("rack-{id}"))
        .spawn(move || r.run())
        .expect("spawn rack thread");
    rack
}

/// Stop a rack's thread and remove it. Its plugins are returned so the
/// caller (UI thread) can drop them there.
pub fn destroy(id: u64) -> Vec<Plugin> {
    let Some(rack) = registry().lock().unwrap().remove(&id) else { return Vec::new() };
    rack.stop.store(true, Ordering::Relaxed);
    let plugins = std::mem::take(&mut *rack.chain.lock().unwrap());
    plugins
}

impl Rack {
    pub fn name(&self) -> String {
        self.name.lock().unwrap().clone()
    }
    pub fn set_name(&self, n: &str) {
        *self.name.lock().unwrap() = n.to_string();
    }

    // ---- chain editing (UI thread) ----

    /// Names of the plugins in the chain, with their bypass state.
    pub fn chain_info(&self) -> Vec<(String, bool, bool)> {
        self.chain.lock().unwrap().iter().map(|p| (p.desc.name.clone(), p.bypass, p.desc.is_instrument())).collect()
    }
    pub fn is_instrument(&self) -> bool {
        self.chain.lock().unwrap().first().is_some_and(|p| p.desc.is_instrument())
    }
    pub fn push(&self, p: Plugin) {
        self.chain.lock().unwrap().push(p);
    }
    pub fn insert(&self, index: usize, p: Plugin) {
        let mut c = self.chain.lock().unwrap();
        let i = index.min(c.len());
        c.insert(i, p);
    }
    /// Replace the plugin at `index` while audio keeps running. Returns the old one.
    pub fn swap(&self, index: usize, p: Plugin) -> Option<Plugin> {
        let mut c = self.chain.lock().unwrap();
        c.get_mut(index).map(|slot| std::mem::replace(slot, p))
    }
    pub fn remove(&self, index: usize) -> Option<Plugin> {
        let mut c = self.chain.lock().unwrap();
        (index < c.len()).then(|| c.remove(index))
    }
    pub fn move_plugin(&self, from: usize, to: usize) {
        let mut c = self.chain.lock().unwrap();
        if from < c.len() && to < c.len() {
            let p = c.remove(from);
            c.insert(to, p);
        }
    }
    pub fn set_bypass(&self, index: usize, bypass: bool) {
        if let Some(p) = self.chain.lock().unwrap().get_mut(index) {
            p.bypass = bypass;
        }
    }
    /// Run `f` on the plugin at `index` (e.g. to open its editor or save state).
    pub fn with_plugin<R>(&self, index: usize, f: impl FnOnce(&Plugin) -> R) -> Option<R> {
        self.chain.lock().unwrap().get(index).map(f)
    }

    // ---- MIDI ----

    pub fn send_midi(&self, msg: [u8; 3]) {
        let mut m = self.midi.lock().unwrap();
        if m.len() < 4096 {
            m.push(msg);
        }
    }

    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.out_peak.swap(0, Ordering::Relaxed))
    }

    // ---- connections (called by routes) ----

    pub fn attach_input(&self, route: u64, ring: Arc<Ring>, gain: Arc<Gain>) {
        self.inputs.lock().unwrap().push(Input { route, ring, gain, primed: false });
    }
    pub fn attach_output(&self, route: u64, ring: Arc<Ring>) {
        self.outputs.lock().unwrap().push((route, ring));
    }
    pub fn detach(&self, route: u64) {
        self.inputs.lock().unwrap().retain(|i| i.route != route);
        self.outputs.lock().unwrap().retain(|o| o.0 != route);
    }

    // ---- audio thread ----

    fn run(&self) {
        let block_time = Duration::from_secs_f64(BLOCK as f64 / SAMPLE_RATE);
        let prime = (SAMPLE_RATE as usize / 50) * 2; // 20 ms of stereo samples
        let mut l = vec![0f32; BLOCK];
        let mut r = vec![0f32; BLOCK];
        let mut tmp = vec![0f32; BLOCK * 2];
        let mut out = vec![0f32; BLOCK * 2];
        let mut next = Instant::now();
        while !self.stop.load(Ordering::Relaxed) {
            next += block_time;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else if now - next > Duration::from_millis(50) {
                next = now; // fell far behind (e.g. system stall): don't burst
            }

            // 1. Mix all inputs.
            l.fill(0.0);
            r.fill(0.0);
            {
                let mut inputs = self.inputs.lock().unwrap();
                for inp in inputs.iter_mut() {
                    if !inp.primed {
                        if inp.ring.len() < prime {
                            continue;
                        }
                        inp.primed = true;
                    }
                    tmp.fill(0.0);
                    let got = inp.ring.pop(&mut tmp);
                    if got < tmp.len() {
                        inp.primed = false; // ran dry: wait for a cushion again
                    }
                    let g = inp.gain.effective();
                    let mut peak = 0f32;
                    for i in 0..BLOCK {
                        let (a, b) = (tmp[2 * i] * g, tmp[2 * i + 1] * g);
                        l[i] += a;
                        r[i] += b;
                        peak = peak.max(a.abs()).max(b.abs());
                    }
                    inp.gain.report_peak(peak);
                }
            }

            // 2. Plugin chain. MIDI goes to every plugin (effects ignore it).
            let midi: Vec<[u8; 3]> = std::mem::take(&mut *self.midi.lock().unwrap());
            if let Ok(mut chain) = self.chain.try_lock() {
                if chain.first().is_some_and(|p| p.desc.is_instrument()) {
                    // Instruments make the sound; audio inputs are not used.
                    l.fill(0.0);
                    r.fill(0.0);
                }
                for p in chain.iter_mut() {
                    p.process(&mut l, &mut r, &midi);
                }
            } else if !midi.is_empty() {
                // Chain is being edited this instant: keep the MIDI for next block.
                self.midi.lock().unwrap().splice(0..0, midi);
            }

            // 3. Send to every output.
            let mut peak = 0f32;
            for i in 0..BLOCK {
                let (a, b) = (l[i].clamp(-1.0, 1.0), r[i].clamp(-1.0, 1.0));
                out[2 * i] = a;
                out[2 * i + 1] = b;
                peak = peak.max(a.abs()).max(b.abs());
            }
            self.out_peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
            for (_, ring) in self.outputs.lock().unwrap().iter() {
                ring.push(&out);
            }
        }
    }
}

//! Magic Mixer GUI. Three columns, left to right, follow the signal:
//!
//!   SOURCES  ->  RECORDING / EFFECTS APPS  ->  OUTPUTS
//!   Spotify       Ableton, Discord, OBS        Speakers, CABLE Input
//!
//! Click something, then click where its sound should go.
//!
//! Feeding a recording app works through a virtual cable: the source is routed
//! into "CABLE Input" and the app records from "CABLE Output". When Windows
//! can see which input an app records from, that is used; otherwise the card
//! tells the user which input to pick in that app.
//!
//! Optional start-up connections (handy for testing and shortcuts):
//!   magic-mixer-gui --connect "spotify=CABLE Input" --connect "chrome=Speakers"
#![windows_subsystem = "windows"]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Stroke, Vec2};
mod racks_ui;

use magic_mixer::com::ComGuard;
use magic_mixer::mixer::Mixer;
use magic_mixer::rack;
use magic_mixer::outputs::{self, Device, Direction};
use magic_mixer::saved::{self, Saved, SavedConnection};
use magic_mixer::sources::{self, AppSource, Recorder};

// ---------------------------------------------------------------------------
// Background poller: all COM/WASAPI enumeration happens off the UI thread.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Snapshot {
    apps: Vec<AppSource>,
    recorders: Vec<Recorder>,
    processes: Vec<sources::ProcessInfo>,
    outputs: Vec<Device>,
    /// Recording devices (mics, headsets, line in).
    inputs: Vec<Device>,
    /// MIDI input port names (keyboards, controllers, virtual ports).
    midi_ins: Vec<String>,
    error: Option<String>,
    loaded: bool,
}

fn spawn_poller(ctx: egui::Context) -> Arc<Mutex<Snapshot>> {
    let snap = Arc::new(Mutex::new(Snapshot::default()));
    let shared = snap.clone();
    std::thread::Builder::new()
        .name("poller".into())
        .spawn(move || {
            let _com = ComGuard::new();
            // Ports other apps (Ableton etc.) use to play Magic Mixer's instruments.
            let _ = magic_mixer::midi::ensure_loopback();
            let mut tick = 0u32;
            loop {
                // The process list changes slowly; refresh it every ~1.5 s.
                if tick % 10 == 0 {
                    let procs = sources::running_processes();
                    let midi = magic_mixer::midi::list().map(|p| p.inputs).unwrap_or_default();
                    let mut s = shared.lock().unwrap();
                    s.processes = procs;
                    s.midi_ins = midi;
                }
                tick = tick.wrapping_add(1);
                let apps = sources::list_apps();
                let recs = sources::list_recorders();
                let outs = outputs::list_devices(Direction::Output);
                let ins = outputs::list_devices(Direction::Input);
                {
                    let mut s = shared.lock().unwrap();
                    s.loaded = true;
                    if let Ok(i) = ins {
                        s.inputs = i;
                    }
                    match (apps, recs, outs) {
                        (Ok(a), Ok(r), Ok(o)) => {
                            s.apps = a;
                            s.recorders = r;
                            s.outputs = o;
                            s.error = None;
                        }
                        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => s.error = Some(e.message().to_string()),
                    }
                }
                ctx.request_repaint();
                std::thread::sleep(Duration::from_millis(150));
            }
        })
        .expect("spawn poller");
    snap
}

// ---------------------------------------------------------------------------
// Naming and classification
// ---------------------------------------------------------------------------

fn app_label(name: &str) -> String {
    let base = name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name);
    let mut c = base.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => base.to_string(),
    }
}

/// Display name for an app / input / rack source ("rack:3" -> the rack's name).
fn display_name(racks: &HashMap<u64, String>, name: &str) -> String {
    match rack::rack_id(name) {
        Some(id) => racks.get(&id).cloned().unwrap_or_else(|| format!("Rack {id}")),
        None => app_label(name),
    }
}

/// Display name for a device / rack destination.
fn display_device(racks: &HashMap<u64, String>, name: &str) -> String {
    match rack::rack_id(name) {
        Some(id) => racks.get(&id).cloned().unwrap_or_else(|| format!("Rack {id}")),
        None => short_device(name).to_string(),
    }
}

fn short_device(name: &str) -> &str {
    name.split(" (").next().unwrap_or(name)
}

/// Split "CABLE Input (VB-Audio Virtual Cable)" into ("CABLE Input", hint).
fn output_label(d: &Device) -> (String, String) {
    let short = short_device(&d.name).to_string();
    let lower = d.name.to_lowercase();
    let hint = if lower.contains("cable input") {
        "Virtual cable. Other apps record it as \u{201C}CABLE Output\u{201D}".to_string()
    } else if d.is_default {
        "Your default speakers".to_string()
    } else {
        d.name.split(" (").nth(1).map(|s| s.trim_end_matches(')').to_string()).unwrap_or_default()
    };
    (short, hint)
}

/// Apps that usually take audio in and do something with it.
const KNOWN_RECORDING_APPS: &[&str] = &[
    "ableton", "discord", "obs", "audacity", "reaper", "fl64", "fl studio", "bitwig", "cubase",
    "studio one", "studioone", "zoom", "teams", "skype", "streamlabs", "voicemod", "nvidia broadcast",
    "audition", "protools", "pro tools", "wavelab", "cakewalk", "reason",
];

/// Main executables of recording apps, used to list them while they are
/// open but silent. Matched as a prefix of the lowercase exe name, so helper
/// processes like "Ableton Web Connector.exe" are not listed.
const KNOWN_RECORDING_EXES: &[&str] = &[
    "ableton live", "discord.exe", "obs64.exe", "obs32.exe", "audacity.exe", "reaper.exe", "fl64.exe",
    "fl.exe", "bitwig studio.exe", "cubase", "studio one.exe", "zoom.exe", "ms-teams.exe",
    "streamlabs obs.exe", "voicemod", "nvidia broadcast.exe",
];

fn contains_name(list: &[String], name: &str) -> bool {
    list.iter().any(|n| n.eq_ignore_ascii_case(name))
}

const CABLE_COLORS: [Color32; 6] = [
    Color32::from_rgb(0x4c, 0x9a, 0xff),
    Color32::from_rgb(0xff, 0x8a, 0x3d),
    Color32::from_rgb(0x3d, 0xc9, 0x8a),
    Color32::from_rgb(0xd0, 0x5c, 0xe3),
    Color32::from_rgb(0xf2, 0xc9, 0x4c),
    Color32::from_rgb(0xff, 0x5c, 0x7a),
];
const GREEN: Color32 = Color32::from_rgb(0x3d, 0xc9, 0x8a);
const ORANGE: Color32 = Color32::from_rgb(0xff, 0x8a, 0x3d);
const RED: Color32 = Color32::from_rgb(0xff, 0x5c, 0x5c);

fn cable_color(id: u64) -> Color32 {
    CABLE_COLORS[(id as usize) % CABLE_COLORS.len()]
}

/// A recording/effects app as shown in the middle column.
#[derive(Clone)]
struct MiddleApp {
    source: AppSource,
    /// The input Windows sees it recording from, if any.
    listening: Option<(String, String)>, // (id, name)
    recording: bool,
    /// The playback device that feeds it (usually "CABLE Input"), if any.
    feed: Option<Device>,
}

/// Everything the UI needs for one frame, derived from the snapshot.
struct View {
    sources: Vec<AppSource>,
    middle: Vec<MiddleApp>,
    outputs: Vec<Device>,
    all_outputs: Vec<Device>,
    /// Every routable app (sources + middle), for reattaching after restarts.
    routable: Vec<AppSource>,
    cable: Option<Device>,
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct MagicMixer {
    snap: Arc<Mutex<Snapshot>>,
    mixer: Mixer,
    selected: Option<u32>,
    status: Option<String>,
    levels: HashMap<u64, f32>,
    /// Route id -> the recording app (exe name) it feeds.
    via: HashMap<u64, String>,
    /// Saved (or --connect) connections whose app or device isn't here yet.
    waiting: Vec<SavedConnection>,
    middle_apps: Vec<String>,
    source_apps: Vec<String>,
    last_saved: String,
    last_save_check: Instant,
    /// VST plugin racks.
    racks: racks_ui::Racks,
    /// Column widths as fractions (sources, middle, outputs).
    col_frac: [f32; 4],
    /// Device names hidden from the columns.
    hidden: Vec<String>,
    show_hidden: bool,
}

impl MagicMixer {
    fn new(cc: &eframe::CreationContext<'_>, extra: Vec<SavedConnection>) -> Self {
        cc.egui_ctx.set_zoom_factor(1.1);
        let saved = saved::load();
        let last_saved = saved::to_json(&saved);
        let mut racks = racks_ui::Racks::new(cc.egui_ctx.clone());
        let rack_errors = racks.restore(&saved.racks);
        let status = (!rack_errors.is_empty()).then(|| format!("Some plugins didn\u{2019}t load: {}", rack_errors.join("; ")));
        let col_frac = match saved.columns.as_deref() {
            Some(&[a, b, c, d]) => [a, b, c, d],
            _ => [0.25; 4], // first run, or saved by the 3-column version
        };
        let hidden = saved.hidden_devices.clone();
        let mut waiting = saved.connections;
        for c in extra {
            if !waiting.iter().any(|w| w.app.eq_ignore_ascii_case(&c.app) && w.device_name == c.device_name) {
                waiting.push(c);
            }
        }
        MagicMixer {
            snap: spawn_poller(cc.egui_ctx.clone()),
            mixer: Mixer::new(),
            selected: None,
            status,
            levels: HashMap::new(),
            via: HashMap::new(),
            waiting,
            middle_apps: saved.middle_apps,
            source_apps: saved.source_apps,
            last_saved,
            last_save_check: Instant::now(),
            racks,
            col_frac,
            hidden,
            show_hidden: false,
        }
    }

    /// Display name for an app, input, or rack ("rack:3" -> its name).
    fn rack_names(&self) -> HashMap<u64, String> {
        self.racks.list.iter().map(|r| (r.id, self.racks.name(r.id))).collect()
    }

    fn is_middle(&self, name: &str, records: bool) -> bool {
        if contains_name(&self.source_apps, name) {
            return false;
        }
        if contains_name(&self.middle_apps, name) || records {
            return true;
        }
        let lower = name.to_lowercase();
        KNOWN_RECORDING_APPS.iter().any(|k| lower.contains(k))
    }

    fn build_view(&self, apps: &[AppSource], recs: &[Recorder], procs: &[sources::ProcessInfo], outs: &[Device], ins: &[Device]) -> View {
        // "CABLE In 16ch" is the same VB-Cable as "CABLE Input" and only one of
        // the two can be used at a time, so hide it.
        let all_outputs: Vec<Device> =
            outs.iter().filter(|d| !d.name.to_lowercase().contains("cable in 16ch")).cloned().collect();
        let cable = all_outputs.iter().find(|d| d.name.to_lowercase().contains("cable input")).cloned();

        // Every app that plays or records, once per pid.
        let mut everyone: Vec<AppSource> = apps.to_vec();
        for r in recs {
            if !everyone.iter().any(|a| a.pid == r.pid) {
                everyone.push(r.as_source());
            }
        }

        // Recording apps that are open but silent (e.g. Discord between calls).
        let mut seen: HashSet<String> = everyone.iter().map(|a| a.name.to_lowercase()).collect();
        for (_, _, exe) in procs {
            let lower = exe.to_lowercase();
            if seen.contains(&lower) || !self.is_middle(exe, false) || contains_name(&self.source_apps, exe) {
                continue;
            }
            if !(contains_name(&self.middle_apps, exe) || KNOWN_RECORDING_EXES.iter().any(|k| lower.starts_with(k))) {
                continue;
            }
            if let Some(pid) = sources::root_process(procs, exe) {
                if pid != std::process::id() {
                    everyone.push(AppSource { pid, name: exe.clone(), ..Default::default() });
                }
            }
            seen.insert(lower);
        }

        let mut sources = Vec::new();
        let mut middle = Vec::new();
        for a in &everyone {
            let rec = recs.iter().filter(|r| r.pid == a.pid).max_by_key(|r| r.active);
            if self.is_middle(&a.name, rec.is_some()) {
                let listening = rec.map(|r| (r.input_id.clone(), r.input_name.clone()));
                let feed = match &listening {
                    Some((_, input)) => outputs::feeding_output(input, &all_outputs).cloned(),
                    None => cable.clone(),
                };
                middle.push(MiddleApp { source: a.clone(), listening, recording: rec.is_some_and(|r| r.active), feed });
            } else {
                sources.push(a.clone());
            }
        }

        // Mics / headsets / line-ins are sources too. The recording ends of
        // virtual cables ("CABLE Output") are left out: patching them back
        // into an output would feed the cable into itself.
        for d in ins {
            if outputs::feeding_output(&d.name, &all_outputs).is_some() {
                continue;
            }
            let src = sources::input_source(&d.id, &d.name);
            everyone.push(src.clone());
            sources.push(src);
        }

        // A cable that feeds a recording app is shown as that app, not as an output.
        let feeds: HashSet<String> = middle.iter().filter_map(|m| m.feed.as_ref().map(|d| d.id.clone())).collect();
        let outputs = all_outputs.iter().filter(|d| !feeds.contains(&d.id)).cloned().collect();
        View { sources, middle, outputs, all_outputs, routable: everyone, cable }
    }

    /// Start any waiting connection whose app and device are now present.
    fn start_waiting(&mut self, view: &View) {
        let mut still = Vec::new();
        for w in std::mem::take(&mut self.waiting) {
            match (w.find_app(&view.routable), w.find_device(&view.all_outputs)) {
                (Some(a), Some(d)) => {
                    let id = self.mixer.connect(a.clone(), d.clone(), w.volume);
                    if let Some(v) = &w.via {
                        self.via.insert(id, v.clone());
                    }
                    if w.muted {
                        if let Some(r) = self.mixer.routes().iter().find(|r| r.id == id) {
                            if !r.gain.is_muted() {
                                r.gain.toggle_mute();
                            }
                        }
                    }
                }
                _ => still.push(w),
            }
        }
        self.waiting = still;
    }

    /// Save to disk if anything changed (checked at most twice a second).
    fn autosave(&mut self) {
        if self.last_save_check.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_save_check = Instant::now();
        let mut connections: Vec<SavedConnection> = self
            .mixer
            .routes()
            .iter()
            .map(|r| SavedConnection { via: self.via.get(&r.id).cloned(), ..SavedConnection::from_route(r) })
            .collect();
        connections.extend(self.waiting.iter().cloned());
        let state = Saved {
            connections,
            middle_apps: self.middle_apps.clone(),
            source_apps: self.source_apps.clone(),
            racks: self.racks.saved(),
            hidden_devices: self.hidden.clone(),
            columns: Some(self.col_frac.to_vec()),
        };
        let json = saved::to_json(&state);
        if json != self.last_saved {
            match saved::save_json(&json) {
                Ok(()) => self.last_saved = json,
                Err(e) => self.status = Some(format!("Couldn\u{2019}t save: {e}")),
            }
        }
    }

    fn move_to_middle(&mut self, name: &str, to_middle: bool) {
        let (add, remove) = if to_middle {
            (&mut self.middle_apps, &mut self.source_apps)
        } else {
            (&mut self.source_apps, &mut self.middle_apps)
        };
        remove.retain(|n| !n.eq_ignore_ascii_case(name));
        if !contains_name(add, name) {
            add.push(name.to_string());
        }
        self.selected = None;
    }

    /// Connect (or disconnect, if already connected) `src` -> `dest`.
    fn toggle(&mut self, src: AppSource, dest: Device, via: Option<String>, label: String) {
        let src_name = display_name(&self.rack_names(), &src.name);
        match self.mixer.find(src.pid, &dest.id).map(|r| r.id) {
            Some(id) => {
                self.mixer.disconnect(id);
                self.via.remove(&id);
                self.levels.remove(&id);
                self.status = Some(format!("Disconnected {src_name} -> {label}"));
            }
            None => {
                let id = self.mixer.connect(src, dest, 1.0);
                if let Some(v) = via {
                    self.via.insert(id, v);
                }
                self.status = Some(format!("Connected {src_name} -> {label}"));
            }
        }
        self.selected = None;
    }
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

/// A clickable card.
fn card(ui: &mut egui::Ui, selected: bool, connected: Option<Color32>, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let visuals = ui.visuals().clone();
    let fill = if selected { visuals.selection.bg_fill.linear_multiply(0.6) } else { visuals.faint_bg_color };
    let stroke = if selected {
        Stroke::new(2.0_f32, visuals.selection.stroke.color)
    } else if let Some(c) = connected {
        Stroke::new(1.5_f32, c)
    } else {
        Stroke::new(1.0_f32, visuals.widgets.noninteractive.bg_stroke.color)
    };
    let inner = egui::Frame::none()
        .fill(fill)
        .stroke(stroke)
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add(ui);
        });
    let resp = ui.interact(inner.response.rect, inner.response.id.with("click"), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// A thin horizontal level meter.
fn meter(ui: &mut egui::Ui, level: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 6.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    let l = level.clamp(0.0, 1.0).sqrt(); // perceptual-ish
    if l > 0.0 {
        let fill = if level > 0.9 { RED } else { GREEN };
        p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * l, rect.height())), 3.0, fill);
    }
}

fn column_header(ui: &mut egui::Ui, title: &str, sub: &str) {
    ui.label(RichText::new(title).small().strong().weak());
    ui.label(RichText::new(sub).small().weak());
    ui.add_space(4.0);
}

fn cable(painter: &egui::Painter, a: Pos2, b: Pos2, stroke: Stroke) {
    let dx = (b.x - a.x) * 0.5;
    painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
        [a, a + Vec2::new(dx, 0.0), b - Vec2::new(dx, 0.0), b],
        false,
        Color32::TRANSPARENT,
        stroke,
    ));
}

// ---------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------

impl eframe::App for MagicMixer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(60));
        let (apps, recs, procs, outs, ins, midi_ports, poll_error, loaded) = {
            let s = self.snap.lock().unwrap();
            (
                s.apps.clone(),
                s.recorders.clone(),
                s.processes.clone(),
                s.outputs.clone(),
                s.inputs.clone(),
                s.midi_ins.clone(),
                s.error.clone(),
                s.loaded,
            )
        };
        let mut view = self.build_view(&apps, &recs, &procs, &outs, &ins);
        // Racks are both sources (their output) and destinations (their input).
        for r in &self.racks.list {
            view.routable.push(rack::as_source(r.id));
            view.all_outputs.push(rack::as_output(r.id));
        }
        self.racks.tick();
        self.racks.generic_windows(ctx);
        let names = self.rack_names();
        if loaded {
            if !self.waiting.is_empty() {
                self.start_waiting(&view);
            }
            self.mixer.reattach(&view.routable);
            self.autosave();
        }
        if let Some(pid) = self.selected {
            if !view.routable.iter().any(|a| a.pid == pid) {
                self.selected = None;
            }
        }
        let peak_of = |pid: u32| apps.iter().find(|a| a.pid == pid).map(|a| a.peak).unwrap_or(0.0);

        // ---------------- Connections (bottom panel) ----------------
        let mut to_disconnect = None;
        let mut forget = None;
        egui::TopBottomPanel::bottom("connections").resizable(false).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Connections");
            ui.add_space(2.0);
            if self.mixer.routes().is_empty() && self.waiting.is_empty() {
                ui.label(RichText::new("No connections yet.").weak());
            }
            egui::Grid::new("routes").num_columns(5).spacing([14.0, 6.0]).show(ui, |ui| {
                for r in self.mixer.routes() {
                    let dest = match self.via.get(&r.id) {
                        Some(v) => app_label(v),
                        None => {
                            // A shared cable: say who hears it.
                            let hearers: Vec<String> = view
                                .middle
                                .iter()
                                .filter(|m| m.feed.as_ref().is_some_and(|f| f.id == r.output.id))
                                .map(|m| app_label(&m.source.name))
                                .collect();
                            if hearers.is_empty() {
                                display_device(&names, &r.output.name)
                            } else {
                                format!("{} (heard by {})", short_device(&r.output.name), hearers.join(", "))
                            }
                        }
                    };
                    let lvl = self.levels.entry(r.id).or_insert(0.0);
                    *lvl = r.gain.take_peak().max(*lvl * 0.85);
                    let lvl = *lvl;

                    ui.horizontal(|ui| {
                        let (dot, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 5.0, cable_color(r.id));
                        ui.label(RichText::new(format!("{}  ->  {}", display_name(&names, &r.source.name), dest)).strong());
                    });
                    let mut pct = r.gain.get() * 100.0;
                    if ui.add(egui::Slider::new(&mut pct, 0.0..=200.0).suffix("%").integer().text("volume")).changed() {
                        r.gain.set(pct / 100.0);
                    }
                    let muted = r.gain.is_muted();
                    let label = if muted { RichText::new("Unmute").color(ORANGE) } else { RichText::new("Mute") };
                    if ui.button(label).clicked() {
                        r.gain.toggle_mute();
                    }
                    if let Some(err) = r.error() {
                        ui.label(RichText::new(format!("Stopped: {err}")).color(RED));
                    } else if loaded && !view.routable.iter().any(|a| a.pid == r.source.pid) {
                        ui.label(RichText::new(format!("Waiting for {} to reopen", app_label(&r.source.name))).weak());
                    } else {
                        meter(ui, if muted { 0.0 } else { lvl }, 120.0);
                    }
                    if ui.button("Disconnect").clicked() {
                        to_disconnect = Some(r.id);
                    }
                    ui.end_row();
                }
            });
            for (i, w) in self.waiting.iter().enumerate() {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    ui.painter().circle_stroke(dot.center(), 4.5, Stroke::new(1.5_f32, Color32::GRAY));
                    let dest = w.via.as_deref().map(app_label).unwrap_or_else(|| display_device(&names, &w.device_name));
                    ui.label(RichText::new(format!("{}  ->  {}", display_name(&names, &w.app), dest)).strong());
                    ui.add_space(10.0);
                    let is_device = !w.app.to_lowercase().ends_with(".exe") && w.app.contains(" (");
                    let why = if w.find_app(&view.routable).is_none() && is_device {
                        format!("Will connect when {} is plugged in", short_device(&w.app))
                    } else if w.find_app(&view.routable).is_none() {
                        format!("Will connect when {} plays sound", display_name(&names, &w.app))
                    } else {
                        format!("Waiting for {} to be plugged in", short_device(&w.device_name))
                    };
                    ui.label(RichText::new(why).weak());
                    if ui.button("Forget").clicked() {
                        forget = Some(i);
                    }
                });
            }
            ui.add_space(8.0);
        });
        if let Some(id) = to_disconnect {
            self.mixer.disconnect(id);
            self.levels.remove(&id);
            self.via.remove(&id);
        }
        if let Some(i) = forget {
            self.waiting.remove(i);
        }

        // ---------------- Patch bay (central panel) ----------------
        let mut out_anchor: HashMap<u32, Pos2> = HashMap::new(); // right edge of a source/middle card, by pid
        let mut mid_in_anchor: HashMap<String, Pos2> = HashMap::new(); // left edge of a middle card, by exe name (lowercase)
        let mut dev_anchor: HashMap<String, Pos2> = HashMap::new(); // left edge of an output card, by device id
        enum Click {
            Source(AppSource),
            Middle(MiddleApp),
            Output(Device),
            Rack(u64),
        }
        let mut click: Option<Click> = None;
        let mut move_req: Option<(String, bool)> = None;
        let mut rack_actions: Vec<(u64, racks_ui::RackAction)> = Vec::new();
        let mut new_rack = false;
        let mut hide_req: Option<(String, bool)> = None; // (device name, hide?)

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("Magic Mixer").strong());
                ui.add_space(12.0);
                let step = match self.selected.and_then(|p| view.routable.iter().find(|a| a.pid == p)) {
                    None => "Step 1:  click a sound source".to_string(),
                    Some(a) => format!("Step 2:  click where {}\u{2019}s sound should go  (Esc to cancel)", display_name(&names, &a.name)),
                };
                ui.label(RichText::new(step).size(16.0));
            });
            if let Some(msg) = &self.status {
                ui.label(RichText::new(msg).weak());
            }
            if let Some(e) = &poll_error {
                ui.label(RichText::new(format!("Audio error: {e}")).color(RED));
            }
            ui.add_space(8.0);

            let gap = 70.0;
            // Leave room for the gaps plus egui's spacing between items (and the scrollbar).
            let avail = (ui.available_width() - 3.0 * gap - 6.0 * ui.spacing().item_spacing.x - 16.0).max(600.0);
            let col_ws = self.col_frac.map(|f| (avail * f).max(140.0));
            let selected_is_source = self.selected.is_some_and(|p| {
                view.sources.iter().any(|s| s.pid == p) || self.racks.list.iter().any(|r| rack::as_source(r.id).pid == p)
            });
            let routes = self.mixer.routes();
            let color_from = |pid: u32| routes.iter().find(|r| r.source.pid == pid).map(|r| cable_color(r.id));
            let hidden = &self.hidden;
            let show_hidden = &mut self.show_hidden;
            let racks = &mut self.racks;
            let col_frac = &mut self.col_frac;
            let is_hidden = |name: &str| hidden.iter().any(|h| h == name);

            egui::ScrollArea::vertical().show(ui, |ui| {
                // Draw the plugin racks that belong in one lane.
                macro_rules! rack_cards {
                    ($ui:expr, $lane:expr) => {
                        let ids: Vec<u64> = racks
                            .list
                            .iter()
                            .map(|r| r.id)
                            .filter(|id| rack::get(*id).map(|r| r.lane()) == Some($lane))
                            .collect();
                        for id in ids {
                            let src_pid = rack::as_source(id).pid;
                            let rid = format!("rack:{id}");
                            let accent = routes.iter().find(|r| r.output.id == rid).map(|r| cable_color(r.id)).or_else(|| color_from(src_pid));
                            let (title, rect, acts) = racks.card($ui, id, self.selected == Some(src_pid), accent, &midi_ports);
                            let y = title.rect.center().y;
                            dev_anchor.insert(rid, Pos2::new(rect.left(), y));
                            out_anchor.insert(src_pid, Pos2::new(rect.right(), y));
                            if title.clicked() {
                                click = Some(Click::Rack(id));
                            }
                            rack_actions.extend(acts.into_iter().map(|a| (id, a)));
                            $ui.add_space(4.0);
                        }
                    };
                }
                let cols = ui.horizontal_top(|ui| {
                    let col_w = col_ws[0];
                    // ---- Column 1: sources ----
                    ui.vertical(|ui| {
                        ui.set_width(col_w);
                        column_header(ui, "SOURCES", "Apps playing sound, microphones");
                        if loaded && view.sources.is_empty() {
                            ui.label(RichText::new("Nothing is playing yet.").weak());
                        }
                        for a in &view.sources {
                            let hid = a.is_input() && is_hidden(&a.name);
                            if hid && !*show_hidden {
                                continue;
                            }
                            let resp = card(ui, self.selected == Some(a.pid), color_from(a.pid), |ui| {
                                if hid {
                                    ui.set_opacity(0.45);
                                }
                                ui.horizontal(|ui| {
                                    if a.is_input() {
                                        // "Headset (Example Headphones)" -> "Example Headphones" + "Headset mic"
                                        let (kind, dev) = match a.name.split_once(" (") {
                                            Some((k, d)) => (k.to_string(), d.strip_suffix(')').unwrap_or(d).to_string()),
                                            None => ("Input".to_string(), a.name.clone()),
                                        };
                                        let kind = if kind.to_lowercase().contains("mic") { kind } else { format!("{kind} mic") };
                                        ui.add(egui::Label::new(RichText::new(dev).strong().size(15.0)).truncate());
                                        ui.label(RichText::new(kind).small().color(ui.visuals().weak_text_color()));
                                    } else {
                                        ui.add(egui::Label::new(RichText::new(app_label(&a.name)).strong().size(15.0)).truncate());
                                    }
                                    if a.is_input() {
                                    } else if a.active {
                                        ui.label(RichText::new("playing").small().color(GREEN));
                                    }
                                });
                                // Inputs show their level in Connections once routed.
                                meter(ui, if a.is_input() { 0.0 } else { a.peak }, ui.available_width());
                            });
                            out_anchor.insert(a.pid, Pos2::new(resp.rect.right(), resp.rect.center().y));
                            if resp.clicked() {
                                click = Some(Click::Source(a.clone()));
                            }
                            resp.context_menu(|ui| {
                                if a.is_input() {
                                    let label = if hid { "Show this device" } else { "Hide this device" };
                                    if ui.button(label).clicked() {
                                        hide_req = Some((a.name.clone(), !hid));
                                        ui.close_menu();
                                    }
                                } else if ui.button("Move to Recording / effects apps").clicked() {
                                    move_req = Some((a.name.clone(), true));
                                    ui.close_menu();
                                }
                            });
                            ui.add_space(4.0);
                        }
                        ui.add_space(6.0);
                        ui.label(RichText::new("Don\u{2019}t see your app? Play something in it.").small().weak());
                        // Instrument racks make sound, so they live with the sources.
                        ui.add_space(10.0);
                        rack_cards!(ui, rack::Lane::Makes);
                    });

                    ui.add_space(gap);

                    // ---- Column 2: plugins that make AND change sound ----
                    ui.vertical(|ui| {
                        ui.set_width(col_ws[1]);
                        column_header(ui, "MAKES + CHANGES SOUND", "Plugins that play and process");
                        let any = racks.list.iter().any(|r| rack::get(r.id).is_some_and(|r| r.lane() == rack::Lane::MakeAndChange));
                        if !any {
                            ui.label(
                                RichText::new("Racks with a plugin that both plays from MIDI and processes incoming audio (vocoders, synths with an audio input) appear here.")
                                    .small()
                                    .weak(),
                            );
                        }
                        rack_cards!(ui, rack::Lane::MakeAndChange);
                    });

                    ui.add_space(gap);

                    // ---- Column 3: recording / effects apps, effect racks ----
                    ui.vertical(|ui| {
                        ui.set_width(col_ws[2]);
                        column_header(ui, "RECORDING / EFFECTS APPS", "Ableton, Discord, OBS\u{2026}");
                        if loaded && view.middle.is_empty() {
                            ui.label(
                                RichText::new("Apps like Ableton, Discord or OBS appear here when open. Right-click any source to move it here.")
                                    .small()
                                    .weak(),
                            );
                        }
                        for m in &view.middle {
                            let into = routes.iter().find(|r| self.via.get(&r.id).is_some_and(|v| v.eq_ignore_ascii_case(&m.source.name)));
                            let accent = into.map(|r| cable_color(r.id)).or_else(|| color_from(m.source.pid));
                            let highlight = self.selected == Some(m.source.pid) || (selected_is_source && m.feed.is_some());
                            let resp = card(ui, self.selected == Some(m.source.pid), accent, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(app_label(&m.source.name)).strong().size(15.0));
                                    if m.recording {
                                        ui.label(RichText::new("recording").small().color(RED));
                                    }
                                });
                                meter(ui, peak_of(m.source.pid), ui.available_width());
                                let (text, color) = match (&m.listening, &m.feed, &view.cable) {
                                    (Some((_, input)), Some(_), _) => (format!("Hears {} \u{2014} ready", short_device(input)), GREEN),
                                    (Some((_, input)), None, Some(c)) => (
                                        format!("Hears {}. Set its input to \u{201C}{}\u{201D} to feed it from here", short_device(input), short_device(&c.name).replace("Input", "Output")),
                                        ORANGE,
                                    ),
                                    (Some((_, input)), None, None) => (format!("Hears {}", short_device(input)), ORANGE),
                                    (None, Some(f), _) => (
                                        format!("In this app, choose \u{201C}{}\u{201D} as the audio input", short_device(&f.name).replace("Input", "Output")),
                                        ui.visuals().weak_text_color(),
                                    ),
                                    (None, None, _) => ("Install VB-Cable to feed this app".to_string(), RED),
                                };
                                ui.label(RichText::new(text).small().color(color));
                            });
                            if highlight && self.selected != Some(m.source.pid) {
                                ui.painter().rect_stroke(resp.rect.expand(2.0), 9.0, Stroke::new(1.0_f32, ui.visuals().selection.stroke.color.linear_multiply(0.5)));
                            }
                            mid_in_anchor.insert(m.source.name.to_lowercase(), Pos2::new(resp.rect.left(), resp.rect.center().y));
                            out_anchor.insert(m.source.pid, Pos2::new(resp.rect.right(), resp.rect.center().y));
                            if resp.clicked() {
                                click = Some(Click::Middle(m.clone()));
                            }
                            resp.context_menu(|ui| {
                                if ui.button("Move to Sources").clicked() {
                                    move_req = Some((m.source.name.clone(), false));
                                    ui.close_menu();
                                }
                            });
                            ui.add_space(4.0);
                        }

                        // Plugin racks that change sound (new/empty racks start here and
                        // move lanes by themselves once their plugins are known).
                        ui.add_space(10.0);
                        column_header(ui, "PLUGIN RACKS", "VST plugins; racks move to the lane that fits");
                        rack_cards!(ui, rack::Lane::Changes);
                        if ui.button("+ New plugin rack").on_hover_text("A container for VST instruments and effects").clicked() {
                            new_rack = true;
                        }
                    });

                    ui.add_space(gap);

                    // ---- Column 4: outputs ----
                    ui.vertical(|ui| {
                        ui.set_width(col_ws[3]);
                        column_header(ui, "OUTPUTS", "Speakers, headphones, cables");
                        for d in &view.outputs {
                            let hid = is_hidden(&d.name);
                            if hid && !*show_hidden {
                                continue;
                            }
                            let accent = routes.iter().find(|r| r.output.id == d.id && !self.via.contains_key(&r.id)).map(|r| cable_color(r.id));
                            let (title, hint) = output_label(d);
                            let resp = card(ui, false, accent, |ui| {
                                if hid {
                                    ui.set_opacity(0.45);
                                }
                                ui.label(RichText::new(title).strong().size(15.0));
                                if !hint.is_empty() {
                                    ui.label(RichText::new(hint).small().weak());
                                }
                            });
                            dev_anchor.insert(d.id.clone(), Pos2::new(resp.rect.left(), resp.rect.center().y));
                            if resp.clicked() {
                                click = Some(Click::Output(d.clone()));
                            }
                            resp.context_menu(|ui| {
                                let label = if hid { "Show this device" } else { "Hide this device" };
                                if ui.button(label).clicked() {
                                    hide_req = Some((d.name.clone(), !hid));
                                    ui.close_menu();
                                }
                            });
                            ui.add_space(4.0);
                        }
                        if !hidden.is_empty() {
                            ui.add_space(6.0);
                            ui.checkbox(show_hidden, RichText::new(format!("Show hidden devices ({})", hidden.len())).small());
                        }
                        ui.label(RichText::new("Right-click a device to hide it.").small().weak());
                    });
                });

                // ---- Column splitters: drag to resize ----
                let area = cols.response.rect;
                let mut x = area.left();
                for i in 0..3 {
                    x += col_ws[i] + gap / 2.0;
                    let handle = Rect::from_min_max(Pos2::new(x - 5.0, area.top()), Pos2::new(x + 5.0, area.bottom()));
                    let resp = ui.interact(handle, ui.id().with(("split", i)), Sense::drag());
                    if resp.hovered() || resp.dragged() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                        ui.painter().line_segment(
                            [Pos2::new(x, area.top()), Pos2::new(x, area.bottom())],
                            Stroke::new(1.0_f32, ui.visuals().selection.stroke.color.linear_multiply(0.5)),
                        );
                    }
                    if resp.dragged() {
                        let d = resp.drag_delta().x / avail;
                        let (a, b) = (col_frac[i] + d, col_frac[i + 1] - d);
                        if a >= 0.15 && b >= 0.15 {
                            col_frac[i] = a;
                            col_frac[i + 1] = b;
                        }
                    }
                    if resp.double_clicked() {
                        *col_frac = [0.25; 4];
                    }
                    x += gap / 2.0;
                }

                // ---- Cables ----
                let painter = ui.painter();
                let port = ui.visuals().weak_text_color();
                for p in out_anchor.values() {
                    painter.circle_stroke(*p, 4.0, Stroke::new(1.0_f32, port));
                }
                for p in mid_in_anchor.values().chain(dev_anchor.values()) {
                    painter.circle_stroke(*p, 4.0, Stroke::new(1.0_f32, port));
                }
                for r in routes {
                    let Some(&a) = out_anchor.get(&r.source.pid) else { continue };
                    let b = self
                        .via
                        .get(&r.id)
                        .and_then(|v| mid_in_anchor.get(&v.to_lowercase()))
                        .or_else(|| dev_anchor.get(&r.output.id))
                        .or_else(|| {
                            // A cable whose device feeds a recording app ends at that app.
                            view.middle
                                .iter()
                                .find(|m| m.feed.as_ref().is_some_and(|f| f.id == r.output.id))
                                .and_then(|m| mid_in_anchor.get(&m.source.name.to_lowercase()))
                        });
                    let Some(&b) = b else { continue };
                    let color = if r.error().is_some() { Color32::GRAY } else { cable_color(r.id) };
                    let width = if r.gain.is_muted() { 1.5_f32 } else { 3.5 };
                    cable(painter, a, b, Stroke::new(width, color));
                    painter.circle_filled(a, 5.0, color);
                    painter.circle_filled(b, 5.0, color);
                }
                if let (Some(pid), Some(mouse)) = (self.selected, ctx.pointer_hover_pos()) {
                    if let Some(&a) = out_anchor.get(&pid) {
                        cable(painter, a, mouse, Stroke::new(2.0_f32, ui.visuals().selection.stroke.color.linear_multiply(0.7)));
                    }
                }
            });
        });

        // ---------------- Click handling ----------------
        if let Some((name, to_middle)) = move_req {
            self.move_to_middle(&name, to_middle);
        }
        if let Some((name, hide)) = hide_req {
            self.hidden.retain(|h| h != &name);
            if hide {
                self.hidden.push(name);
            }
        }
        if new_rack {
            let id = self.racks.create();
            self.status = Some(format!("Added {}. Use \u{201C}+ Add plugin\u{201D}, then patch sound into it.", self.racks.name(id)));
        }
        for (id, action) in rack_actions {
            let structural = matches!(
                action,
                racks_ui::RackAction::Add(_) | racks_ui::RackAction::Swap(..) | racks_ui::RackAction::Remove(_) | racks_ui::RackAction::Up(_)
            );
            if matches!(action, racks_ui::RackAction::Delete) {
                // Unplug everything going into or out of the rack first.
                let rid = format!("rack:{id}");
                let ids: Vec<u64> = self.mixer.routes().iter().filter(|r| r.source.name == rid || r.output.id == rid).map(|r| r.id).collect();
                for rid in ids {
                    self.mixer.disconnect(rid);
                    self.via.remove(&rid);
                }
                let rid = format!("rack:{id}");
                self.waiting.retain(|w| w.app != rid && w.device_id.as_deref() != Some(rid.as_str()));
                if self.selected == Some(rack::as_source(id).pid) {
                    self.selected = None;
                }
            }
            if let Some(msg) = self.racks.apply(id, action) {
                self.status = Some(msg);
            }
            if structural {
                self.racks.save_states();
            }
        }

        let selected_src = self.selected.and_then(|p| view.routable.iter().find(|a| a.pid == p).cloned());
        let selected_is_source = selected_src
            .as_ref()
            .is_some_and(|s| view.sources.iter().any(|x| x.pid == s.pid) || rack::rack_id(&s.name).is_some());
        match click {
            Some(Click::Rack(id)) => {
                let src_pid = rack::as_source(id).pid;
                let name = self.racks.name(id);
                match selected_src.clone() {
                    Some(src) if src.pid != src_pid => {
                        if rack::get(id).is_some_and(|r| !r.accepts_audio()) {
                            self.status = Some(format!(
                                "{name} is an instrument rack: it\u{2019}s played by MIDI. Click it first to send its sound somewhere."
                            ));
                            self.selected = None;
                        } else {
                            self.toggle(src, rack::as_output(id), None, name);
                        }
                    }
                    _ => {
                        self.selected = if self.selected == Some(src_pid) { None } else { Some(src_pid) };
                        self.status = None;
                    }
                }
            }
            Some(Click::Source(a)) => {
                self.selected = if self.selected == Some(a.pid) { None } else { Some(a.pid) };
                self.status = None;
            }
            Some(Click::Middle(m)) => {
                if let (true, Some(src)) = (selected_is_source, selected_src.clone()) {
                    match m.feed.clone() {
                        Some(feed) => {
                            let label = app_label(&m.source.name);
                            self.toggle(src, feed, Some(m.source.name.clone()), label);
                        }
                        None => {
                            self.status = Some(format!(
                                "{} is listening to a real input. Set its input to \u{201C}CABLE Output\u{201D} first.",
                                app_label(&m.source.name)
                            ))
                        }
                    }
                } else {
                    // Select the app itself, to send its output somewhere.
                    self.selected = if self.selected == Some(m.source.pid) { None } else { Some(m.source.pid) };
                    self.status = None;
                }
            }
            Some(Click::Output(d)) => match selected_src {
                Some(src) => {
                    let label = short_device(&d.name).to_string();
                    self.toggle(src, d, None, label);
                }
                None => self.status = Some("Click a source first.".into()),
            },
            None => {}
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.selected = None;
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Keep plugin settings and the latest layout.
        self.racks.save_states();
        self.last_save_check = Instant::now() - Duration::from_secs(5);
        self.autosave();
        // Then shut everything down in order: connections, then plugins (on
        // this UI thread, as VST3 requires).
        self.mixer = Mixer::new();
        self.racks.shutdown();
    }
}

fn main() -> eframe::Result<()> {
    // Internal: probe one plugin in this child process (used by the plugin scan).
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("probe-plugin") {
        let code = match argv.get(2).map(|p| magic_mixer::plugins::probe_in_process(std::path::Path::new(p))) {
            Some(Ok(())) => 0,
            Some(Err(e)) => {
                eprintln!("{e}");
                1
            }
            None => 2,
        };
        std::process::exit(code);
    }
    if !single_instance() {
        return Ok(());
    }

    let mut extra = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--connect" {
            if let Some((app, dev)) = args.next().and_then(|v| v.split_once('=').map(|(x, y)| (x.to_string(), y.to_string()))) {
                extra.push(SavedConnection { app, device_id: None, device_name: dev, volume: 1.0, muted: false, loose: true, via: None });
            }
        }
    }

    let size = 64;
    let icon = egui::IconData { rgba: magic_mixer::icon::rgba(size), width: size, height: size };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Magic Mixer")
            .with_icon(icon)
            .with_inner_size([1180.0, 680.0])
            .with_min_inner_size([760.0, 460.0]),
        ..Default::default()
    };
    let result = eframe::run_native("Magic Mixer", options, Box::new(move |cc| Ok(Box::new(MagicMixer::new(cc, extra)))));
    // Everything is saved and shut down by now (on_exit). Some plugins hang
    // for a long time while Windows unloads their DLLs at process exit, so
    // end the process directly instead of waiting for them.
    if result.is_ok() {
        unsafe {
            use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
            let _ = TerminateProcess(GetCurrentProcess(), 0);
        }
    }
    result
}

/// Returns false (after bringing the running window to the front) if Magic
/// Mixer is already open, so two copies never route the same audio twice.
fn single_instance() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};
    unsafe {
        // The handle is deliberately leaked: the mutex lives as long as the process.
        let name = windows::core::HSTRING::from(format!("Local\\{}SingleInstance", magic_mixer::saved::folder_name()));
        let _mutex = CreateMutexW(None, false, &name);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(h) = FindWindowW(None, w!("Magic Mixer")) {
                let _ = ShowWindow(h, SW_RESTORE);
                let _ = SetForegroundWindow(h);
            }
            return false;
        }
    }
    true
}

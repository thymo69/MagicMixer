//! Magic Mixer GUI: apps on the left, outputs on the right.
//! Click an app, then click where its sound should go. That's it.
//!
//! Optional start-up connections (handy for testing and shortcuts):
//!   magic-mixer-gui --connect "spotify=CABLE Input" --connect "chrome=Speakers"
#![windows_subsystem = "windows"]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use magic_mixer::com::ComGuard;
use magic_mixer::mixer::Mixer;
use magic_mixer::outputs::{self, Device, Direction};
use magic_mixer::saved::{self, SavedConnection};
use magic_mixer::sources::{self, AppSource};

// ---------------------------------------------------------------------------
// Background poller: all COM/WASAPI enumeration happens off the UI thread.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Snapshot {
    apps: Vec<AppSource>,
    outputs: Vec<Device>,
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
            loop {
                let apps = sources::list_apps();
                let outs = outputs::list_devices(Direction::Output);
                {
                    let mut s = shared.lock().unwrap();
                    s.loaded = true;
                    match (apps, outs) {
                        (Ok(a), Ok(o)) => {
                            s.apps = a;
                            s.outputs = o;
                            s.error = None;
                        }
                        (Err(e), _) | (_, Err(e)) => s.error = Some(e.message().to_string()),
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
// Friendly naming
// ---------------------------------------------------------------------------

fn app_label(name: &str) -> String {
    let base = name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name);
    let mut c = base.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => base.to_string(),
    }
}

/// Split "CABLE Input (VB-Audio Virtual Cable)" into ("CABLE Input", hint).
fn output_label(d: &Device) -> (String, String) {
    let short = d.name.split(" (").next().unwrap_or(&d.name).to_string();
    let lower = d.name.to_lowercase();
    let hint = if lower.contains("cable input") {
        "Virtual cable. In Discord / Ableton pick \u{201C}CABLE Output\u{201D}".to_string()
    } else if lower.contains("cable in 16ch") {
        "16-channel virtual cable".to_string()
    } else if d.is_default {
        "Your default speakers".to_string()
    } else {
        d.name.split(" (").nth(1).map(|s| s.trim_end_matches(')').to_string()).unwrap_or_default()
    };
    (short, hint)
}

const CABLE_COLORS: [Color32; 6] = [
    Color32::from_rgb(0x4c, 0x9a, 0xff),
    Color32::from_rgb(0xff, 0x8a, 0x3d),
    Color32::from_rgb(0x3d, 0xc9, 0x8a),
    Color32::from_rgb(0xd0, 0x5c, 0xe3),
    Color32::from_rgb(0xf2, 0xc9, 0x4c),
    Color32::from_rgb(0xff, 0x5c, 0x7a),
];

fn cable_color(id: u64) -> Color32 {
    CABLE_COLORS[(id as usize) % CABLE_COLORS.len()]
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct MagicMixer {
    snap: Arc<Mutex<Snapshot>>,
    mixer: Mixer,
    selected: Option<u32>,
    status: Option<String>,
    /// Smoothed level per route for the meters.
    levels: HashMap<u64, f32>,
    /// Saved (or --connect) connections whose app or device isn't here yet.
    waiting: Vec<SavedConnection>,
    /// Last JSON written to disk, so we only save when something changed.
    last_saved: String,
    last_save_check: Instant,
}

impl MagicMixer {
    fn new(cc: &eframe::CreationContext<'_>, extra: Vec<SavedConnection>) -> Self {
        cc.egui_ctx.set_zoom_factor(1.15);
        let mut waiting = saved::load();
        let last_saved = saved::to_json(&waiting);
        for c in extra {
            if !waiting.iter().any(|w| w.app.eq_ignore_ascii_case(&c.app) && w.device_name == c.device_name) {
                waiting.push(c);
            }
        }
        MagicMixer {
            snap: spawn_poller(cc.egui_ctx.clone()),
            mixer: Mixer::new(),
            selected: None,
            status: None,
            levels: HashMap::new(),
            waiting,
            last_saved,
            last_save_check: Instant::now(),
        }
    }

    /// Start any waiting connection whose app and device are now present.
    fn start_waiting(&mut self, apps: &[AppSource], outs: &[Device]) {
        let mut still = Vec::new();
        for w in std::mem::take(&mut self.waiting) {
            match (w.find_app(apps), w.find_device(outs)) {
                (Some(a), Some(d)) => {
                    let id = self.mixer.connect(a.clone(), d.clone(), w.volume);
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
        let mut all: Vec<SavedConnection> = self.mixer.routes().iter().map(SavedConnection::from_route).collect();
        all.extend(self.waiting.iter().cloned());
        let json = saved::to_json(&all);
        if json != self.last_saved {
            match saved::save_json(&json) {
                Ok(()) => self.last_saved = json,
                Err(e) => self.status = Some(format!("Couldn\u{2019}t save connections: {e}")),
            }
        }
    }
}

/// A clickable card. Returns the click response and the card's rect.
fn card(ui: &mut egui::Ui, selected: bool, connected: Option<Color32>, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let visuals = ui.visuals().clone();
    let fill = if selected {
        visuals.selection.bg_fill.linear_multiply(0.6)
    } else {
        visuals.faint_bg_color
    };
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
        let fill = if level > 0.9 { Color32::from_rgb(0xff, 0x5c, 0x5c) } else { Color32::from_rgb(0x3d, 0xc9, 0x8a) };
        let r = Rect::from_min_size(rect.min, Vec2::new(rect.width() * l, rect.height()));
        p.rect_filled(r, 3.0, fill);
    }
}

impl eframe::App for MagicMixer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(60));
        let (apps, outs, poll_error, loaded) = {
            let s = self.snap.lock().unwrap();
            (s.apps.clone(), s.outputs.clone(), s.error.clone(), s.loaded)
        };
        // "CABLE In 16ch" is the same VB-Cable as "CABLE Input" (only one can
        // be used at a time), so hide it to keep the list simple.
        let outs: Vec<Device> = outs.into_iter().filter(|d| !d.name.to_lowercase().contains("cable in 16ch")).collect();
        if loaded {
            if !self.waiting.is_empty() {
                self.start_waiting(&apps, &outs);
            }
            self.mixer.reattach(&apps);
            self.autosave();
        }
        // Forget the selection if that app went away.
        if let Some(pid) = self.selected {
            if !apps.iter().any(|a| a.pid == pid) {
                self.selected = None;
            }
        }

        // ---------------- Connections (bottom panel) ----------------
        let mut to_disconnect = None;
        egui::TopBottomPanel::bottom("connections").resizable(false).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Connections");
            ui.add_space(2.0);
            if self.mixer.routes().is_empty() && self.waiting.is_empty() {
                ui.label(RichText::new("No connections yet.").weak());
            }
            // Saved connections whose app (or device) isn't available right now.
            let mut forget = None;
            for (i, w) in self.waiting.iter().enumerate() {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    ui.painter().circle_stroke(dot.center(), 4.5, Stroke::new(1.5_f32, Color32::GRAY));
                    let dev_short = w.device_name.split(" (").next().unwrap_or(&w.device_name);
                    ui.label(RichText::new(format!("{}  ->  {}", app_label(&w.app), dev_short)).strong());
                    ui.add_space(10.0);
                    let why = if w.find_app(&apps).is_none() {
                        format!("Will connect when {} plays sound", app_label(&w.app))
                    } else {
                        format!("Waiting for {dev_short} to be plugged in")
                    };
                    ui.label(RichText::new(why).weak());
                    if ui.button("Forget").clicked() {
                        forget = Some(i);
                    }
                });
            }
            if let Some(i) = forget {
                self.waiting.remove(i);
            }
            egui::Grid::new("routes").num_columns(5).spacing([14.0, 6.0]).show(ui, |ui| {
                for r in self.mixer.routes() {
                    let (out_name, _) = output_label(&r.output);
                    let lvl = self.levels.entry(r.id).or_insert(0.0);
                    *lvl = r.gain.take_peak().max(*lvl * 0.85);
                    let lvl = *lvl;

                    ui.horizontal(|ui| {
                        let (dot, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 5.0, cable_color(r.id));
                        ui.label(RichText::new(format!("{}  ->  {}", app_label(&r.source.name), out_name)).strong());
                    });

                    let mut pct = r.gain.get() * 100.0;
                    if ui
                        .add(egui::Slider::new(&mut pct, 0.0..=200.0).suffix("%").integer().text("volume"))
                        .changed()
                    {
                        r.gain.set(pct / 100.0);
                    }

                    let muted = r.gain.is_muted();
                    let label = if muted { RichText::new("Unmute").color(Color32::from_rgb(0xff, 0x8a, 0x3d)) } else { RichText::new("Mute") };
                    if ui.button(label).clicked() {
                        r.gain.toggle_mute();
                    }

                    if let Some(err) = r.error() {
                        ui.label(RichText::new(format!("Stopped: {err}")).color(Color32::from_rgb(0xff, 0x5c, 0x5c)));
                    } else if loaded && !apps.iter().any(|a| a.pid == r.source.pid) {
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
            ui.add_space(8.0);
        });
        if let Some(id) = to_disconnect {
            self.mixer.disconnect(id);
            self.levels.remove(&id);
        }

        // ---------------- Patch bay (central panel) ----------------
        let mut app_anchors: HashMap<u32, Pos2> = HashMap::new();
        let mut out_anchors: HashMap<String, Pos2> = HashMap::new();
        let mut clicked_app: Option<AppSource> = None;
        let mut clicked_out: Option<Device> = None;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("Magic Mixer").strong());
                ui.add_space(12.0);
                let step = match self.selected.and_then(|p| apps.iter().find(|a| a.pid == p)) {
                    None => "Step 1:  click an app".to_string(),
                    Some(a) => format!("Step 2:  click where {}\u{2019}s sound should go  (Esc to cancel)", app_label(&a.name)),
                };
                ui.label(RichText::new(step).size(16.0));
            });
            if let Some(msg) = &self.status {
                ui.label(RichText::new(msg).weak());
            }
            if let Some(e) = &poll_error {
                ui.label(RichText::new(format!("Audio error: {e}")).color(Color32::from_rgb(0xff, 0x5c, 0x5c)));
            }
            ui.add_space(8.0);

            let gap = 120.0; // room for the cables
            let col_w = ((ui.available_width() - gap) / 2.0).max(160.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    // Apps column
                    ui.vertical(|ui| {
                        ui.set_width(col_w);
                        ui.label(RichText::new("APPS").small().strong().weak());
                        if loaded && apps.is_empty() {
                            ui.label(RichText::new("No apps are making sound yet.").weak());
                        }
                        for a in &apps {
                            let connected = self.mixer.routes().iter().find(|r| r.source.pid == a.pid).map(|r| cable_color(r.id));
                            let resp = card(ui, self.selected == Some(a.pid), connected, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(app_label(&a.name)).strong().size(15.0));
                                    if a.active {
                                        ui.label(RichText::new("playing").small().color(Color32::from_rgb(0x3d, 0xc9, 0x8a)));
                                    }
                                });
                                meter(ui, a.peak, ui.available_width());
                            });
                            app_anchors.insert(a.pid, Pos2::new(resp.rect.right(), resp.rect.center().y));
                            if resp.clicked() {
                                clicked_app = Some(a.clone());
                            }
                            ui.add_space(4.0);
                        }
                        ui.add_space(6.0);
                        ui.label(RichText::new("Don\u{2019}t see your app? Start playing something in it.").small().weak());
                    });

                    ui.add_space(gap);

                    // Outputs column
                    ui.vertical(|ui| {
                        ui.set_width(col_w);
                        ui.label(RichText::new("SEND TO").small().strong().weak());
                        for d in &outs {
                            let connected = self.mixer.routes().iter().find(|r| r.output.id == d.id).map(|r| cable_color(r.id));
                            let (title, hint) = output_label(d);
                            let resp = card(ui, false, connected, |ui| {
                                ui.label(RichText::new(title).strong().size(15.0));
                                if !hint.is_empty() {
                                    ui.label(RichText::new(hint).small().weak());
                                }
                            });
                            out_anchors.insert(d.id.clone(), Pos2::new(resp.rect.left(), resp.rect.center().y));
                            if resp.clicked() {
                                clicked_out = Some(d.clone());
                            }
                            ui.add_space(4.0);
                        }
                    });
                });

                // Cables, drawn over the gap between the columns.
                let painter = ui.painter();
                for r in self.mixer.routes() {
                    if let (Some(&a), Some(&b)) = (app_anchors.get(&r.source.pid), out_anchors.get(&r.output.id)) {
                        let dx = (b.x - a.x) * 0.5;
                        let color = if r.error().is_some() { Color32::GRAY } else { cable_color(r.id) };
                        let width = if r.gain.is_muted() { 1.5_f32 } else { 3.5 };
                        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                            [a, a + Vec2::new(dx, 0.0), b - Vec2::new(dx, 0.0), b],
                            false,
                            Color32::TRANSPARENT,
                            Stroke::new(width, color),
                        ));
                        painter.circle_filled(a, 5.0, color);
                        painter.circle_filled(b, 5.0, color);
                    }
                }
                // A "live" cable following the mouse while an app is selected.
                if let (Some(pid), Some(mouse)) = (self.selected, ctx.pointer_hover_pos()) {
                    if let Some(&a) = app_anchors.get(&pid) {
                        let dx = (mouse.x - a.x) * 0.5;
                        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                            [a, a + Vec2::new(dx, 0.0), mouse - Vec2::new(dx, 0.0), mouse],
                            false,
                            Color32::TRANSPARENT,
                            Stroke::new(2.0_f32, ui.visuals().selection.stroke.color.linear_multiply(0.7)),
                        ));
                    }
                }
            });
        });

        // ---------------- Click handling ----------------
        if let Some(a) = clicked_app {
            self.selected = if self.selected == Some(a.pid) { None } else { Some(a.pid) };
            self.status = None;
        }
        if let Some(d) = clicked_out {
            match self.selected.and_then(|p| apps.iter().find(|a| a.pid == p).cloned()) {
                Some(a) => {
                    let (out_name, _) = output_label(&d);
                    let now = self.mixer.toggle(a.clone(), d, 1.0);
                    self.status = Some(if now {
                        format!("Connected {} -> {}", app_label(&a.name), out_name)
                    } else {
                        format!("Disconnected {} -> {}", app_label(&a.name), out_name)
                    });
                    self.selected = None;
                }
                None => self.status = Some("Pick an app on the left first.".into()),
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.selected = None;
        }
    }
}

fn main() -> eframe::Result<()> {
    // --connect "app=device" (repeatable)
    if !single_instance() {
        return Ok(());
    }

    let mut extra = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--connect" {
            if let Some((app, dev)) = args.next().and_then(|v| v.split_once('=').map(|(x, y)| (x.to_string(), y.to_string()))) {
                extra.push(SavedConnection { app, device_id: None, device_name: dev, volume: 1.0, muted: false, loose: true });
            }
        }
    }

    let size = 64;
    let icon = egui::IconData { rgba: magic_mixer::icon::rgba(size), width: size, height: size };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Magic Mixer")
            .with_icon(icon)
            .with_inner_size([900.0, 640.0])
            .with_min_inner_size([620.0, 440.0]),
        ..Default::default()
    };
    eframe::run_native("Magic Mixer", options, Box::new(move |cc| Ok(Box::new(MagicMixer::new(cc, extra)))))
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
        let _mutex = CreateMutexW(None, false, w!("Local\\MagicMixerSingleInstance"));
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

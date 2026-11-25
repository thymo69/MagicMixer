//! Plugin racks in the GUI: create/delete racks, add / swap / remove / reorder
//! plugins (hot-swap while audio runs), pop-up plugin editors, MIDI input and
//! an on-screen keyboard, and saving everything.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Rect, RichText, Sense, Stroke, Vec2};
use magic_mixer::plugins::{self, PluginInfo};
use magic_mixer::rack;
use magic_mixer::saved::{self, SavedPlugin, SavedRack};
use magic_mixer::vst3host::{Editor, Plugin, PluginDesc};

pub struct RackUi {
    pub id: u64,
    pub midi_in: Option<String>,
    midi_conn: Option<midir::MidiInputConnection<()>>,
    held_key: Option<u8>,
    level: f32,
}

/// Something the user did on a rack card, applied after drawing.
pub enum RackAction {
    Open(usize),
    Bypass(usize, bool),
    Swap(usize, PluginInfo),
    Remove(usize),
    Up(usize),
    Add(PluginInfo),
    SetMidi(Option<String>),
    Delete,
}

pub struct Racks {
    pub list: Vec<RackUi>,
    editors: HashMap<(u64, usize), Editor>,
    /// Scan results (None while scanning).
    scanned: Arc<Mutex<Option<Vec<PluginInfo>>>>,
    next_id: u64,
    last_state_save: Instant,
    /// Last saved bytes per state file, to skip unchanged writes.
    state_cache: HashMap<String, u64>,
}

fn desc_of(p: &PluginInfo) -> PluginDesc {
    PluginDesc { path: p.path.clone(), name: p.name.clone(), kind: p.kind.clone() }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

impl Racks {
    pub fn new(ctx: egui::Context) -> Racks {
        let scanned: Arc<Mutex<Option<Vec<PluginInfo>>>> = Arc::default();
        let s = scanned.clone();
        // Scanning runs each plugin in a child process; keep it off the UI thread.
        std::thread::spawn(move || {
            let found: Vec<PluginInfo> = plugins::scan().into_iter().flatten().flatten().collect();
            *s.lock().unwrap() = Some(found);
            ctx.request_repaint();
        });
        Racks {
            list: Vec::new(),
            editors: HashMap::new(),
            scanned,
            next_id: 1,
            last_state_save: Instant::now(),
            state_cache: HashMap::new(),
        }
    }

    pub fn scanned(&self) -> Option<Vec<PluginInfo>> {
        self.scanned.lock().unwrap().clone()
    }

    /// Recreate saved racks and their plugins (with settings). Returns problems.
    pub fn restore(&mut self, saved: &[SavedRack]) -> Vec<String> {
        let mut errors = Vec::new();
        for sr in saved {
            let r = rack::create(sr.id, &sr.name);
            for (i, sp) in sr.plugins.iter().enumerate() {
                let desc = PluginDesc { path: sp.path.clone().into(), name: sp.name.clone(), kind: sp.kind.clone() };
                let state = sp.state_file.as_deref().and_then(saved::read_state);
                match Plugin::load(&desc, state.as_ref()) {
                    Ok(p) => {
                        r.push(p);
                        r.set_bypass(i.min(r.len().saturating_sub(1)), sp.bypass);
                    }
                    Err(e) => errors.push(format!("{}: {e}", sp.name)),
                }
            }
            let mut ui = RackUi { id: sr.id, midi_in: None, midi_conn: None, held_key: None, level: 0.0 };
            if let Some(port) = &sr.midi_in {
                if let Err(e) = set_midi(&mut ui, Some(port.clone())) {
                    errors.push(e);
                    ui.midi_in = Some(port.clone()); // keep the choice; device may come back
                }
            }
            self.next_id = self.next_id.max(sr.id + 1);
            self.list.push(ui);
        }
        errors
    }

    pub fn create(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        rack::create(id, &format!("Rack {id}"));
        self.list.push(RackUi { id, midi_in: None, midi_conn: None, held_key: None, level: 0.0 });
        id
    }

    pub fn name(&self, id: u64) -> String {
        rack::get(id).map(|r| r.name()).unwrap_or_else(|| format!("Rack {id}"))
    }

    fn close_editors(&mut self, id: u64) {
        self.editors.retain(|(r, _), _| *r != id);
    }

    /// Apply a card action. Returns a status message.
    pub fn apply(&mut self, id: u64, action: RackAction) -> Option<String> {
        let r = rack::get(id)?;
        match action {
            RackAction::Open(i) => {
                if let Some(ed) = self.editors.get(&(id, i)) {
                    ed.focus();
                    return None;
                }
                let title = format!("{} \u{2014} {}", r.with_plugin(i, |p| p.desc.name.clone()).unwrap_or_default(), r.name());
                match r.with_plugin(i, |p| p.open_editor(&title)) {
                    Some(Ok(ed)) => {
                        self.editors.insert((id, i), ed);
                        None
                    }
                    Some(Err(e)) => Some(format!("Couldn\u{2019}t open the plugin window: {e}")),
                    None => None,
                }
            }
            RackAction::Bypass(i, b) => {
                r.set_bypass(i, b);
                None
            }
            RackAction::Swap(i, info) => {
                // Load the new plugin first (audio keeps running), then swap it in.
                match Plugin::load(&desc_of(&info), None) {
                    Ok(p) => {
                        self.editors.remove(&(id, i)); // close the old editor before the old plugin goes
                        let old = r.swap(i, p);
                        drop(old); // shut down here, on the UI thread
                        Some(format!("Swapped in {} (sound kept playing)", info.name))
                    }
                    Err(e) => Some(format!("Couldn\u{2019}t load {}: {e}", info.name)),
                }
            }
            RackAction::Remove(i) => {
                self.close_editors(id);
                drop(r.remove(i));
                None
            }
            RackAction::Up(i) => {
                self.close_editors(id);
                r.move_plugin(i, i - 1);
                None
            }
            RackAction::Add(info) => match Plugin::load(&desc_of(&info), None) {
                Ok(p) => {
                    if p.desc.is_instrument() {
                        // One instrument per rack, always first.
                        self.close_editors(id);
                        if r.is_instrument() {
                            drop(r.swap(0, p));
                        } else {
                            r.insert(0, p);
                        }
                    } else {
                        r.push(p);
                    }
                    None
                }
                Err(e) => Some(format!("Couldn\u{2019}t load {}: {e}", info.name)),
            },
            RackAction::SetMidi(port) => {
                let ui = self.list.iter_mut().find(|u| u.id == id)?;
                set_midi(ui, port).err()
            }
            RackAction::Delete => {
                self.close_editors(id);
                self.list.retain(|u| u.id != id);
                drop(rack::destroy(id));
                None
            }
        }
    }

    /// Close every editor and stop every rack, unloading plugins cleanly.
    pub fn shutdown(&mut self) {
        self.editors.clear();
        for ui in std::mem::take(&mut self.list) {
            drop(rack::destroy(ui.id));
        }
    }

    /// Per frame: drop editors the user closed; save plugin settings now and then.
    pub fn tick(&mut self) {
        self.editors.retain(|_, ed| !ed.is_closed());
        if self.last_state_save.elapsed() > Duration::from_secs(60) {
            self.save_states();
        }
    }

    fn state_file(id: u64, i: usize) -> String {
        format!("rack{id}-{i}.vst3state")
    }

    /// Write every plugin's settings (only files whose contents changed).
    pub fn save_states(&mut self) {
        self.last_state_save = Instant::now();
        for ui in &self.list {
            let Some(r) = rack::get(ui.id) else { continue };
            for i in 0..r.len() {
                if let Some(state) = r.with_plugin(i, |p| p.state()) {
                    let file = Self::state_file(ui.id, i);
                    let h = hash(&state.0) ^ hash(&state.1).rotate_left(1);
                    if self.state_cache.get(&file) != Some(&h) && saved::write_state(&file, &state).is_ok() {
                        self.state_cache.insert(file, h);
                    }
                }
            }
        }
    }

    pub fn saved(&self) -> Vec<SavedRack> {
        self.list
            .iter()
            .filter_map(|ui| {
                let r = rack::get(ui.id)?;
                Some(SavedRack {
                    id: ui.id,
                    name: r.name(),
                    midi_in: ui.midi_in.clone(),
                    plugins: r
                        .chain_descs()
                        .into_iter()
                        .enumerate()
                        .map(|(i, (d, bypass))| SavedPlugin {
                            path: d.path.to_string_lossy().into_owned(),
                            name: d.name,
                            kind: d.kind,
                            bypass,
                            state_file: Some(Self::state_file(ui.id, i)),
                        })
                        .collect(),
                })
            })
            .collect()
    }

    /// Draw one rack card. Returns (title response used for patching, card
    /// rect, actions). `selected` highlights the card.
    pub fn card(
        &mut self,
        ui: &mut egui::Ui,
        id: u64,
        selected: bool,
        accent: Option<Color32>,
        midi_ports: &[String],
    ) -> (egui::Response, Rect, Vec<RackAction>) {
        let mut actions = Vec::new();
        let r = rack::get(id);
        let name = r.as_ref().map(|r| r.name()).unwrap_or_else(|| format!("Rack {id}"));
        let chain = r.as_ref().map(|r| r.chain_info()).unwrap_or_default();
        let is_instrument = chain.first().is_some_and(|c| c.2);
        let scanned = self.scanned();
        let rui = self.list.iter_mut().find(|u| u.id == id);
        let Some(rui) = rui else {
            let resp = ui.label("missing rack");
            return (resp.clone(), resp.rect, actions);
        };
        if let Some(r) = &r {
            rui.level = r.take_peak().max(rui.level * 0.85);
        }

        let visuals = ui.visuals().clone();
        let stroke = if selected {
            Stroke::new(2.0_f32, visuals.selection.stroke.color)
        } else if let Some(c) = accent {
            Stroke::new(1.5_f32, c)
        } else {
            Stroke::new(1.0_f32, visuals.widgets.noninteractive.bg_stroke.color)
        };
        let fill = if selected { visuals.selection.bg_fill.linear_multiply(0.6) } else { visuals.faint_bg_color };
        let mut title_resp: Option<egui::Response> = None;
        let inner = egui::Frame::none()
            .fill(fill)
            .stroke(stroke)
            .rounding(8.0)
            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());

                // Title row: click it to patch (like any other card).
                let title = ui.horizontal(|ui| {
                    let t = ui.add(egui::Label::new(RichText::new(&name).strong().size(15.0)).sense(Sense::click()));
                    let tag = if chain.is_empty() {
                        "empty rack"
                    } else if is_instrument {
                        "instrument rack"
                    } else {
                        "effects rack"
                    };
                    ui.label(RichText::new(tag).small().color(visuals.weak_text_color()));
                    t
                });
                let t = title.inner.union(title.response);
                if t.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                t.context_menu(|ui| {
                    if ui.button("Delete rack").clicked() {
                        actions.push(RackAction::Delete);
                        ui.close_menu();
                    }
                });
                title_resp = Some(t);
                crate::meter(ui, rui.level, ui.available_width());
                ui.add_space(4.0);

                // Plugin chain.
                if chain.is_empty() {
                    ui.label(RichText::new("Empty: sound passes straight through. Add a plugin below.").small().weak());
                }
                for (i, (pname, bypass, inst)) in chain.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{}.", i + 1)).weak());
                        let label = RichText::new(pname).strong().color(if *bypass { visuals.weak_text_color() } else { visuals.text_color() });
                        if ui.add(egui::Button::new(label).frame(false)).on_hover_text("Open the plugin window").clicked() {
                            actions.push(RackAction::Open(i));
                        }
                        if ui.small_button(if *bypass { "Off" } else { "On" }).on_hover_text("Turn this plugin on/off").clicked() {
                            actions.push(RackAction::Bypass(i, !bypass));
                        }
                        ui.menu_button("Swap", |ui| {
                            ui.label(RichText::new("Replace while playing:").small().weak());
                            match &scanned {
                                None => {
                                    ui.label("Scanning plugins\u{2026}");
                                }
                                Some(list) => {
                                    for p in list.iter().filter(|p| (p.kind == "Instrument") == *inst && &p.name != pname) {
                                        if ui.button(&p.name).clicked() {
                                            actions.push(RackAction::Swap(i, p.clone()));
                                            ui.close_menu();
                                        }
                                    }
                                }
                            }
                        });
                        if i > 0 && !(*inst) && !(i == 1 && is_instrument) && ui.small_button("Up").clicked() {
                            actions.push(RackAction::Up(i));
                        }
                        if ui.small_button("Remove").clicked() {
                            actions.push(RackAction::Remove(i));
                        }
                    });
                }

                // Add plugin pop-up.
                ui.menu_button("+ Add plugin", |ui| match &scanned {
                    None => {
                        ui.label("Scanning plugins\u{2026}");
                    }
                    Some(list) if list.is_empty() => {
                        ui.label("No VST3 plugins found in Common Files\\VST3.");
                    }
                    Some(list) => {
                        for (heading, kind) in [("INSTRUMENTS", "Instrument"), ("EFFECTS", "Effect")] {
                            ui.label(RichText::new(heading).small().strong().weak());
                            for p in list.iter().filter(|p| p.kind == kind) {
                                let label = format!("{}   {}", p.name, p.subcategories.replace('|', " \u{00B7} "));
                                if ui.button(label).on_hover_text(&p.vendor).clicked() {
                                    actions.push(RackAction::Add(p.clone()));
                                    ui.close_menu();
                                }
                            }
                            ui.add_space(4.0);
                        }
                    }
                });

                // Instruments: MIDI input and a mini keyboard.
                if is_instrument {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("MIDI from").small());
                        let current = rui.midi_in.clone().unwrap_or_else(|| "Nothing (keyboard below)".into());
                        egui::ComboBox::from_id_salt(("midi", id)).selected_text(current).show_ui(ui, |ui| {
                            if ui.selectable_label(rui.midi_in.is_none(), "Nothing").clicked() {
                                actions.push(RackAction::SetMidi(None));
                            }
                            for p in midi_ports {
                                if ui.selectable_label(rui.midi_in.as_deref() == Some(p), p).clicked() {
                                    actions.push(RackAction::SetMidi(Some(p.clone())));
                                }
                            }
                            if midi_ports.is_empty() {
                                ui.label(RichText::new("No MIDI devices found").small().weak());
                            }
                        });
                    });
                    if let Some(r) = &r {
                        keyboard(ui, rui, r);
                    }
                }
            });
        let title = title_resp.unwrap_or_else(|| inner.response.clone());
        (title, inner.response.rect, actions)
    }
}

/// Connect (or disconnect) a rack's MIDI input.
fn set_midi(ui: &mut RackUi, port: Option<String>) -> Result<(), String> {
    ui.midi_conn = None; // closes the previous connection
    ui.midi_in = port.clone();
    let Some(port) = port else { return Ok(()) };
    let mut input = midir::MidiInput::new("Magic Mixer").map_err(|e| e.to_string())?;
    input.ignore(midir::Ignore::All);
    let ports = input.ports();
    let p = ports
        .iter()
        .find(|p| input.port_name(p).is_ok_and(|n| n == port))
        .ok_or(format!("MIDI device \u{201C}{port}\u{201D} isn\u{2019}t connected"))?
        .clone();
    let id = ui.id;
    let conn = input
        .connect(
            &p,
            "magic-mixer-rack",
            move |_ts, msg, _| {
                if (2..=3).contains(&msg.len()) {
                    if let Some(r) = rack::get(id) {
                        r.send_midi([msg[0], msg[1], *msg.get(2).unwrap_or(&0)]);
                    }
                }
            },
            (),
        )
        .map_err(|e| e.to_string())?;
    ui.midi_conn = Some(conn);
    Ok(())
}

/// A one-octave clickable keyboard (C4..C5) that plays the rack.
fn keyboard(ui: &mut egui::Ui, rui: &mut RackUi, r: &rack::Rack) {
    let keys = 13u8;
    let w = ui.available_width().min(320.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click_and_drag());
    let kw = w / keys as f32;
    let pressed = if resp.is_pointer_button_down_on() {
        resp.interact_pointer_pos().map(|p| ((p.x - rect.left()) / kw).clamp(0.0, keys as f32 - 1.0) as u8)
    } else {
        None
    };
    if pressed != rui.held_key {
        if let Some(k) = rui.held_key {
            r.send_midi([0x80, 60 + k, 0]);
        }
        if let Some(k) = pressed {
            r.send_midi([0x90, 60 + k, 100]);
        }
        rui.held_key = pressed;
    }
    let p = ui.painter();
    for k in 0..keys {
        let black = matches!(k % 12, 1 | 3 | 6 | 8 | 10);
        let key = Rect::from_min_size(rect.min + Vec2::new(k as f32 * kw, 0.0), Vec2::new(kw - 1.0, rect.height()));
        let fill = if Some(k) == pressed {
            crate::GREEN
        } else if black {
            Color32::from_gray(75)
        } else {
            Color32::from_gray(225)
        };
        p.rect_filled(key, 2.0, fill);
    }
    resp.on_hover_text("Click a key to play (middle C to C5)");
}

//! Saved connections: remembered between runs in
//! %APPDATA%\MagicMixer\connections.json.
//!
//! Apps are remembered by executable name (process ids change every run) and
//! outputs by device id, with the device name as a fallback (ids can change if
//! a driver is reinstalled).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::outputs::Device;
use crate::routes::Route;
use crate::sources::AppSource;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedConnection {
    /// Executable name, e.g. "Spotify.exe".
    pub app: String,
    #[serde(default)]
    pub device_id: Option<String>,
    pub device_name: String,
    #[serde(default = "one")]
    pub volume: f32,
    #[serde(default)]
    pub muted: bool,
    /// Loose matching (substring / pid), used for --connect on the command
    /// line. Saved connections always match exactly.
    #[serde(skip)]
    pub loose: bool,
    /// The recording/effects app this connection feeds (e.g. "Ableton Live 12
    /// Lite.exe"), when it goes source -> app rather than straight to a device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

fn one() -> f32 {
    1.0
}

/// Everything Magic Mixer remembers between runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Saved {
    pub connections: Vec<SavedConnection>,
    /// Apps the user moved into the "recording / effects" column.
    #[serde(default)]
    pub middle_apps: Vec<String>,
    /// Apps the user moved out of it (back to sources).
    #[serde(default)]
    pub source_apps: Vec<String>,
    /// Plugin racks (VST containers).
    #[serde(default)]
    pub racks: Vec<SavedRack>,
    /// Device names the user hid from the columns.
    #[serde(default)]
    pub hidden_devices: Vec<String>,
    /// Column widths as fractions (sources, middle, outputs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<[f32; 3]>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SavedRack {
    pub id: u64,
    pub name: String,
    /// MIDI input port name feeding this rack (instrument racks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midi_in: Option<String>,
    pub plugins: Vec<SavedPlugin>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SavedPlugin {
    pub path: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub bypass: bool,
    /// File (in the plugin-states folder) holding the plugin's settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_file: Option<String>,
}

/// Folder for plugin settings (binary blobs, kept out of the JSON).
pub fn state_dir() -> PathBuf {
    path().parent().map(|p| p.join("plugin-states")).unwrap_or_else(|| PathBuf::from("plugin-states"))
}

/// Write a plugin state: [u32 len][component state][controller state].
pub fn write_state(file: &str, state: &(Vec<u8>, Vec<u8>)) -> std::io::Result<()> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;
    let mut buf = Vec::with_capacity(4 + state.0.len() + state.1.len());
    buf.extend_from_slice(&(state.0.len() as u32).to_le_bytes());
    buf.extend_from_slice(&state.0);
    buf.extend_from_slice(&state.1);
    let tmp = dir.join(format!("{file}.tmp"));
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(tmp, dir.join(file))
}

pub fn read_state(file: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let buf = std::fs::read(state_dir().join(file)).ok()?;
    let n = u32::from_le_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let a = buf.get(4..4 + n)?.to_vec();
    let b = buf.get(4 + n..)?.to_vec();
    Some((a, b))
}

impl SavedConnection {
    pub fn from_route(r: &Route) -> Self {
        SavedConnection {
            app: r.source.name.clone(),
            device_id: Some(r.output.id.clone()),
            device_name: r.output.name.clone(),
            volume: r.gain.get(),
            muted: r.gain.is_muted(),
            loose: false,
            via: None,
        }
    }

    pub fn find_app<'a>(&self, apps: &'a [AppSource]) -> Option<&'a AppSource> {
        if self.loose {
            let q = self.app.to_lowercase();
            apps.iter().find(|a| a.pid.to_string() == q || a.name.to_lowercase().contains(&q))
        } else {
            apps.iter().find(|a| a.name.eq_ignore_ascii_case(&self.app))
        }
    }

    pub fn find_device<'a>(&self, outs: &'a [Device]) -> Option<&'a Device> {
        if let Some(id) = &self.device_id {
            if let Some(d) = outs.iter().find(|d| &d.id == id) {
                return Some(d);
            }
        }
        let q = self.device_name.to_lowercase();
        outs.iter()
            .find(|d| d.name.to_lowercase() == q)
            .or_else(|| if self.loose { outs.iter().find(|d| d.name.to_lowercase().contains(&q)) } else { None })
    }
}

/// "MagicMixer", or "MagicMixer-<profile>" when MAGIC_MIXER_PROFILE is set
/// (lets a test copy run beside the real one without sharing settings).
pub fn folder_name() -> String {
    match std::env::var("MAGIC_MIXER_PROFILE") {
        Ok(p) if !p.is_empty() => format!("MagicMixer-{p}"),
        _ => "MagicMixer".to_string(),
    }
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join(folder_name()).join("connections.json")
}

/// Load saved state. A missing file gives an empty state. A file that can't
/// be read (e.g. edited by hand and broken) is copied to
/// `connections.json.unreadable-<time>` first, so the next save can never
/// silently destroy the user's setup.
pub fn load() -> Saved {
    let p = path();
    let Ok(text) = std::fs::read_to_string(&p) else { return Saved::default() };
    // Tolerate the byte-order mark Notepad / PowerShell add.
    let text = text.trim_start_matches('\u{feff}');
    match serde_json::from_str::<Saved>(text) {
        Ok(s) => s,
        Err(_) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = std::fs::copy(&p, p.with_extension(format!("json.unreadable-{stamp}")));
            Saved::default()
        }
    }
}

pub fn to_json(saved: &Saved) -> String {
    serde_json::to_string_pretty(saved).unwrap_or_default()
}

/// Write the file atomically (temp file + rename) so a crash can't corrupt it.
pub fn save_json(json: &str) -> std::io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &p)
}

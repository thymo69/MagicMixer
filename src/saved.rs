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
}

fn one() -> f32 {
    1.0
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    connections: Vec<SavedConnection>,
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

pub fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("MagicMixer").join("connections.json")
}

/// Load saved connections. A missing or unreadable file gives an empty list.
pub fn load() -> Vec<SavedConnection> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str::<File>(&s).ok())
        .map(|f| f.connections)
        .unwrap_or_default()
}

pub fn to_json(conns: &[SavedConnection]) -> String {
    serde_json::to_string_pretty(&File { connections: conns.to_vec() }).unwrap_or_default()
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

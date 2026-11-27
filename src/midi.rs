//! MIDI (spike, midi-vst branch): list ports and prove messages flow through
//! Windows MIDI Services' built-in loopback ports. Uses midir (WinMM backend),
//! which Windows MIDI Services keeps compatible, so loopbacks and real
//! controllers all show up here without loopMIDI.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use midir::{MidiInput, MidiOutput};

pub struct Ports {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

pub fn list() -> Result<Ports, String> {
    let mi = MidiInput::new("magic-mixer").map_err(|e| e.to_string())?;
    let mo = MidiOutput::new("magic-mixer").map_err(|e| e.to_string())?;
    let inputs = mi.ports().iter().filter_map(|p| mi.port_name(p).ok()).collect();
    let outputs = mo.ports().iter().filter_map(|p| mo.port_name(p).ok()).collect();
    Ok(Ports { inputs, outputs })
}

/// Send a note into one loopback port and check it arrives on the other.
/// `send_to` / `listen_on` are case-insensitive name substrings.
/// Returns the raw messages received.
pub fn loopback_test(send_to: &str, listen_on: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut mi = MidiInput::new("magic-mixer-in").map_err(|e| e.to_string())?;
    mi.ignore(midir::Ignore::None);
    let mo = MidiOutput::new("magic-mixer-out").map_err(|e| e.to_string())?;

    let find = |names: Vec<String>, q: &str| names.iter().position(|n| n.to_lowercase().contains(&q.to_lowercase()));
    let in_ports = mi.ports();
    let in_names: Vec<String> = in_ports.iter().filter_map(|p| mi.port_name(p).ok()).collect();
    let in_idx = find(in_names, listen_on).ok_or(format!("no MIDI input matching '{listen_on}'"))?;
    let out_ports = mo.ports();
    let out_names: Vec<String> = out_ports.iter().filter_map(|p| mo.port_name(p).ok()).collect();
    let out_idx = find(out_names, send_to).ok_or(format!("no MIDI output matching '{send_to}'"))?;

    let got: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let sink = got.clone();
    let _in_conn = mi
        .connect(&in_ports[in_idx], "mm-listen", move |_ts, msg, _| sink.lock().unwrap().push(msg.to_vec()), ())
        .map_err(|e| e.to_string())?;
    let mut out_conn = mo.connect(&out_ports[out_idx], "mm-send").map_err(|e| e.to_string())?;

    std::thread::sleep(Duration::from_millis(200));
    out_conn.send(&[0x90, 60, 100]).map_err(|e| e.to_string())?; // note on, middle C
    std::thread::sleep(Duration::from_millis(100));
    out_conn.send(&[0x80, 60, 0]).map_err(|e| e.to_string())?; // note off
    std::thread::sleep(Duration::from_millis(400));
    let received = got.lock().unwrap().clone();
    Ok(received)
}

/// The loopback pair other apps use to play Magic Mixer's instruments:
/// apps send MIDI to `SEND_PORT`, racks listen on `LISTEN_PORT`.
pub const SEND_PORT: &str = "Send to Magic Mixer";
pub const LISTEN_PORT: &str = "Magic Mixer Listen";

/// Make sure the loopback pair exists, creating it with Windows MIDI
/// Services' console tool if needed (no admin rights required; the pair lasts
/// until the MIDI service restarts, so this runs at every start).
/// Returns a short status for the UI.
pub fn ensure_loopback() -> String {
    if list().is_ok_and(|p| p.inputs.iter().any(|n| n == LISTEN_PORT)) {
        return "ready".into();
    }
    let tool = std::path::Path::new(r"C:\Program Files\Windows MIDI Services\Tools\midi.exe");
    if !tool.exists() {
        return "Windows MIDI Services Tools not installed".into();
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let ok = std::process::Command::new(tool)
        .args(["loopback", "create", "--name-a", SEND_PORT, "--name-b", LISTEN_PORT])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .is_ok_and(|o| o.status.success());
    std::thread::sleep(std::time::Duration::from_millis(1500));
    if ok && list().is_ok_and(|p| p.inputs.iter().any(|n| n == LISTEN_PORT)) {
        "created".into()
    } else {
        "couldn't create the loopback".into()
    }
}
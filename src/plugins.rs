//! VST3 plugin discovery (spike, midi-vst branch).
//!
//! Each plugin is probed in a *child process* (`magic-mixer probe-plugin
//! <path>`), so a plugin that crashes or hangs while loading can never take
//! Magic Mixer down. That isolation is the model for hosting too.
//!
//! The probe talks to the plugin's factory through the stable VST3 COM ABI
//! (IPluginFactory / IPluginFactory2) with hand-written vtables: no SDK or
//! bindings crate needed just to read names and categories.

use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub path: PathBuf,
    pub name: String,
    pub vendor: String,
    /// "Instrument" or "Effect".
    pub kind: String,
    /// VST3 sub-categories, e.g. "Instrument|Synth" or "Fx|Delay".
    pub subcategories: String,
}

/// All VST3 plugins in the standard folder (%CommonProgramFiles%\VST3).
pub fn vst3_paths() -> Vec<PathBuf> {
    let root = std::env::var_os("CommonProgramFiles").map(PathBuf::from).unwrap_or_default().join("VST3");
    let mut out = Vec::new();
    collect(&root, &mut out);
    out.sort();
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let is_vst3 = p.extension().is_some_and(|x| x.eq_ignore_ascii_case("vst3"));
        if is_vst3 {
            out.push(p);
        } else if p.is_dir() {
            collect(&p, out);
        }
    }
}

/// The DLL inside a plugin: either the .vst3 file itself, or for a bundle
/// folder, Contents\x86_64-win\<name>.vst3.
pub fn binary_of(path: &Path) -> PathBuf {
    if path.is_dir() {
        let name = path.file_name().unwrap_or_default();
        path.join("Contents").join("x86_64-win").join(name)
    } else {
        path.to_path_buf()
    }
}

/// Probe every plugin, each in its own child process with a timeout.
pub fn scan() -> Vec<Result<Vec<PluginInfo>, (PathBuf, String)>> {
    let exe = std::env::current_exe().expect("current exe");
    vst3_paths()
        .into_iter()
        .map(|p| {
            let mut child = Command::new(&exe)
                .arg("probe-plugin")
                .arg(&p)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| (p.clone(), e.to_string()))?;
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() > deadline => {
                        let _ = child.kill();
                        return Err((p.clone(), "timed out while loading".into()));
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(e) => return Err((p.clone(), e.to_string())),
                }
            }
            let out = child.wait_with_output().map_err(|e| (p.clone(), e.to_string()))?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let why = if err.is_empty() { format!("plugin crashed while loading ({})", out.status) } else { err };
                return Err((p.clone(), why));
            }
            Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| {
                    let f: Vec<&str> = l.split('\t').collect();
                    (f.len() == 4).then(|| PluginInfo {
                        path: p.clone(),
                        name: f[0].into(),
                        vendor: f[1].into(),
                        kind: f[2].into(),
                        subcategories: f[3].into(),
                    })
                })
                .collect())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// In-process probe (only ever run inside the child process)
// ---------------------------------------------------------------------------

#[repr(C)]
struct PFactoryInfo {
    vendor: [c_char; 64],
    url: [c_char; 256],
    email: [c_char; 128],
    flags: i32,
}

#[repr(C)]
struct PClassInfo2 {
    cid: [u8; 16],
    cardinality: i32,
    category: [c_char; 32],
    name: [c_char; 64],
    class_flags: u32,
    sub_categories: [c_char; 128],
    vendor: [c_char; 64],
    version: [c_char; 64],
    sdk_version: [c_char; 64],
}

#[repr(C)]
struct PClassInfo {
    cid: [u8; 16],
    cardinality: i32,
    category: [c_char; 32],
    name: [c_char; 64],
}

/// IPluginFactory (+ IPluginFactory2) vtable, in declaration order.
#[repr(C)]
struct FactoryVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const [u8; 16], *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_factory_info: unsafe extern "system" fn(*mut c_void, *mut PFactoryInfo) -> i32,
    count_classes: unsafe extern "system" fn(*mut c_void) -> i32,
    get_class_info: unsafe extern "system" fn(*mut c_void, i32, *mut PClassInfo) -> i32,
    create_instance: unsafe extern "system" fn(*mut c_void, *const c_char, *const c_char, *mut *mut c_void) -> i32,
    get_class_info2: unsafe extern "system" fn(*mut c_void, i32, *mut PClassInfo2) -> i32,
}

/// VST3 INLINE_UID(l1,l2,l3,l4) as laid out on Windows (COM-compatible).
fn inline_uid(l1: u32, l2: u32, l3: u32, l4: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&l1.to_le_bytes());
    b[4..6].copy_from_slice(&((l2 >> 16) as u16).to_le_bytes());
    b[6..8].copy_from_slice(&(l2 as u16).to_le_bytes());
    b[8..12].copy_from_slice(&l3.to_be_bytes());
    b[12..16].copy_from_slice(&l4.to_be_bytes());
    b
}

fn cstr(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Load the plugin DLL here and print one line per audio class:
/// name \t vendor \t Instrument|Effect \t subcategories.
pub fn probe_in_process(path: &Path) -> Result<(), String> {
    use windows::core::{s, HSTRING};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};

    let bin = binary_of(path);
    unsafe {
        let module = LoadLibraryExW(&HSTRING::from(bin.as_os_str()), None, LOAD_WITH_ALTERED_SEARCH_PATH)
            .map_err(|e| format!("can't load {}: {}", bin.display(), e.message()))?;
        if let Some(init) = GetProcAddress(module, s!("InitDll")) {
            let init: unsafe extern "system" fn() -> bool = std::mem::transmute(init);
            init();
        }
        let get = GetProcAddress(module, s!("GetPluginFactory")).ok_or("not a VST3 plugin (no GetPluginFactory)")?;
        let get: unsafe extern "system" fn() -> *mut c_void = std::mem::transmute(get);
        let factory = get();
        if factory.is_null() {
            return Err("plugin returned no factory".into());
        }
        let vt = &**(factory as *const *const FactoryVtbl);

        let mut finfo: PFactoryInfo = std::mem::zeroed();
        (vt.get_factory_info)(factory, &mut finfo);
        let factory_vendor = cstr(&finfo.vendor);

        // IPluginFactory2 gives sub-categories (Instrument vs Fx).
        let iid2 = inline_uid(0x0007B650, 0xF24B4C0B, 0xA464EDB9, 0xF00B2ABB);
        let mut f2: *mut c_void = std::ptr::null_mut();
        let has2 = (vt.query_interface)(factory, &iid2, &mut f2) == 0 && !f2.is_null();

        for i in 0..(vt.count_classes)(factory) {
            let (category, name, subcats, vendor) = if has2 {
                let vt2 = &**(f2 as *const *const FactoryVtbl);
                let mut ci: PClassInfo2 = std::mem::zeroed();
                if (vt2.get_class_info2)(f2, i, &mut ci) != 0 {
                    continue;
                }
                (cstr(&ci.category), cstr(&ci.name), cstr(&ci.sub_categories), cstr(&ci.vendor))
            } else {
                let mut ci: PClassInfo = std::mem::zeroed();
                if (vt.get_class_info)(factory, i, &mut ci) != 0 {
                    continue;
                }
                (cstr(&ci.category), cstr(&ci.name), String::new(), String::new())
            };
            if category != "Audio Module Class" {
                continue; // controllers and other helper classes
            }
            let kind = if subcats.to_lowercase().contains("instrument") { "Instrument" } else { "Effect" };
            let vendor = if vendor.is_empty() { factory_vendor.clone() } else { vendor };
            println!("{name}\t{vendor}\t{kind}\t{subcats}");
        }
        if has2 {
            let vt2 = &**(f2 as *const *const FactoryVtbl);
            (vt2.release)(f2);
        }
        (vt.release)(factory);
        if let Some(exit) = GetProcAddress(module, s!("ExitDll")) {
            let exit: unsafe extern "system" fn() -> bool = std::mem::transmute(exit);
            exit();
        }
        // Deliberately not unloading: some plugins crash on FreeLibrary, and
        // this process exits right after anyway.
    }
    Ok(())
}

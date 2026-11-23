//! Sources: apps that are (or could be) producing audio, and capturing one
//! app's audio with the WASAPI per-process loopback API (Windows 10 2004+).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::{implement, Interface, Result, HRESULT, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::*;
use windows::core::PROPVARIANT;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, QueryFullProcessImageNameW, SetEvent, WaitForSingleObject,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::com::mix_format;
use crate::ring::Ring;

/// One app that has an audio session open on some output device.
#[derive(Debug, Clone)]
pub struct AppSource {
    pub pid: u32,
    pub name: String,
    /// True if the session is currently in the "active" (playing) state.
    pub active: bool,
    /// Current peak level 0.0..1.0 (max across devices).
    pub peak: f32,
}

/// List every app with an audio session on any active render device.
pub fn list_apps() -> Result<Vec<AppSource>> {
    let mut by_pid: BTreeMap<u32, AppSource> = BTreeMap::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..devices.GetCount()? {
            let device = devices.Item(i)?;
            let manager: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let sessions = manager.GetSessionEnumerator()?;
            for j in 0..sessions.GetCount()? {
                let ctl = sessions.GetSession(j)?;
                let ctl2: IAudioSessionControl2 = ctl.cast()?;
                // S_OK (0) means "this is the system sounds session" - skip it.
                if ctl2.IsSystemSoundsSession() == HRESULT(0) {
                    continue;
                }
                let pid = ctl2.GetProcessId().unwrap_or(0);
                if pid == 0 {
                    continue;
                }
                let active = ctl.GetState().map(|s| s == AudioSessionStateActive).unwrap_or(false);
                let peak = ctl
                    .cast::<IAudioMeterInformation>()
                    .and_then(|m| m.GetPeakValue())
                    .unwrap_or(0.0);
                let entry = by_pid.entry(pid).or_insert_with(|| AppSource {
                    pid,
                    name: process_name(pid).unwrap_or_else(|| format!("pid {pid}")),
                    active: false,
                    peak: 0.0,
                });
                entry.active |= active;
                entry.peak = entry.peak.max(peak);
            }
        }
    }
    let mut apps: Vec<AppSource> = by_pid.into_values().collect();
    apps.sort_by(|a, b| b.active.cmp(&a.active).then(a.name.cmp(&b.name)));
    Ok(apps)
}

/// Resolve user input ("spotify", "spotify.exe" or "1234") to a process id.
/// Prefers an app that is actively playing.
pub fn find_app(query: &str) -> Result<Option<AppSource>> {
    let apps = list_apps()?;
    if let Ok(pid) = query.parse::<u32>() {
        let name = process_name(pid).unwrap_or_else(|| format!("pid {pid}"));
        return Ok(Some(apps.into_iter().find(|a| a.pid == pid).unwrap_or(AppSource {
            pid,
            name,
            active: false,
            peak: 0.0,
        })));
    }
    let q = query.to_lowercase();
    let q = q.trim_end_matches(".exe");
    let mut matches: Vec<AppSource> = apps
        .into_iter()
        .filter(|a| a.name.to_lowercase().trim_end_matches(".exe").contains(q))
        .collect();
    // Active first, then loudest.
    matches.sort_by(|a, b| {
        b.active.cmp(&a.active).then(b.peak.partial_cmp(&a.peak).unwrap_or(std::cmp::Ordering::Equal))
    });
    Ok(matches.into_iter().next())
}

fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        ok.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(path.rsplit('\\').next().unwrap_or(&path).to_string())
    }
}

// ---------------------------------------------------------------------------
// Per-process loopback capture
// ---------------------------------------------------------------------------

/// Completion handler for ActivateAudioInterfaceAsync: just signals an event.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivateHandler {
    done: HANDLE,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateHandler_Impl {
    fn ActivateCompleted(
        &self,
        _op: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> Result<()> {
        unsafe { SetEvent(self.done) }
    }
}

/// PROPVARIANT laid out as VT_BLOB. Defined by hand because the windows
/// crate's PROPVARIANT wrapper has no safe VT_BLOB constructor.
#[repr(C)]
struct BlobPropVariant {
    vt: u16,
    _r1: u16,
    _r2: u16,
    _r3: u16,
    cb_size: u32,
    p_blob_data: *mut u8,
}
const VT_BLOB: u16 = 65;

/// Open a loopback IAudioClient that hears only `pid` (and its child processes).
fn activate_process_loopback(pid: u32) -> Result<IAudioClient> {
    unsafe {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        let pv = BlobPropVariant {
            vt: VT_BLOB,
            _r1: 0,
            _r2: 0,
            _r3: 0,
            cb_size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            p_blob_data: &mut params as *mut _ as *mut u8,
        };

        let done = CreateEventW(None, false, false, None)?;
        let handler: IActivateAudioInterfaceCompletionHandler = ActivateHandler { done }.into();
        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&pv as *const BlobPropVariant as *const PROPVARIANT),
            &handler,
        )?;
        WaitForSingleObject(done, 10_000);
        let _ = CloseHandle(done);

        let mut hr = HRESULT(0);
        let mut unk = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok()?;
        unk.ok_or_else(|| windows::core::Error::from(HRESULT(0x80004005u32 as i32)))?.cast()
    }
}

/// Capture `pid`'s audio until `stop` is set, pushing float stereo frames
/// (see `com::mix_format`) into `ring`. Blocks the calling thread.
pub fn capture_app(pid: u32, ring: Arc<Ring>, stop: Arc<AtomicBool>) -> Result<()> {
    let _com = crate::com::ComGuard::new()?;
    unsafe {
        let client = activate_process_loopback(pid)?;
        let fmt = mix_format();
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            200_000, // 20 ms, in 100 ns units
            0,
            &fmt,
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;

        let channels = fmt.nChannels as usize;
        let mut scratch: Vec<f32> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                let packet = capture.GetNextPacketSize()?;
                if packet == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize * channels;
                scratch.clear();
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                    scratch.resize(n, 0.0);
                } else {
                    scratch.extend_from_slice(std::slice::from_raw_parts(data as *const f32, n));
                }
                capture.ReleaseBuffer(frames)?;
                ring.push(&scratch);
            }
        }
        let _ = client.Stop();
        let _ = CloseHandle(event);
    }
    Ok(())
}

//! Outputs: audio devices (speakers, headsets, virtual cables) that a route
//! can play into, plus a level meter for recording devices (handy for
//! checking what arrives at e.g. "CABLE Output").

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::{Result, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::com::mix_format;
use crate::ring::Ring;
use crate::routes::Gain;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Playback device (speakers, "CABLE Input").
    Output,
    /// Recording device (microphones, "CABLE Output").
    Input,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // direction is for the upcoming GUI
pub struct Device {
    pub id: String,
    pub name: String,
    pub direction: Direction,
    pub is_default: bool,
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

fn device_id(d: &IMMDevice) -> Result<String> {
    unsafe {
        let p: PWSTR = d.GetId()?;
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        Ok(s)
    }
}

fn device_name(d: &IMMDevice) -> Result<String> {
    unsafe {
        let store = d.OpenPropertyStore(STGM_READ)?;
        let v = store.GetValue(&PKEY_Device_FriendlyName)?;
        Ok(v.to_string())
    }
}

pub fn list_devices(direction: Direction) -> Result<Vec<Device>> {
    let flow = match direction {
        Direction::Output => eRender,
        Direction::Input => eCapture,
    };
    unsafe {
        let e = enumerator()?;
        let default_id = e
            .GetDefaultAudioEndpoint(flow, eConsole)
            .and_then(|d| device_id(&d))
            .unwrap_or_default();
        let coll = e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..coll.GetCount()? {
            let d = coll.Item(i)?;
            let id = device_id(&d)?;
            out.push(Device {
                is_default: id == default_id,
                name: device_name(&d).unwrap_or_else(|_| id.clone()),
                id,
                direction,
            });
        }
        Ok(out)
    }
}

/// Find a device by (case-insensitive) name substring; "default" picks the default.
pub fn find_device(query: &str, direction: Direction) -> Result<Option<Device>> {
    let devs = list_devices(direction)?;
    let q = query.to_lowercase();
    if q == "default" {
        return Ok(devs.into_iter().find(|d| d.is_default));
    }
    // Exact match wins over substring match.
    if let Some(d) = devs.iter().find(|d| d.name.to_lowercase() == q) {
        return Ok(Some(d.clone()));
    }
    Ok(devs.into_iter().find(|d| d.name.to_lowercase().contains(&q)))
}

/// For a recording device that is the far end of a virtual cable, find the
/// playback device that feeds it: "CABLE Output (VB-Audio Virtual Cable)"
/// is fed by "CABLE Input (VB-Audio Virtual Cable)"; likewise CABLE-A/B and
/// other "X Output" / "X Input" pairs from the same driver.
/// Returns None for real inputs like microphones.
pub fn feeding_output<'a>(input_name: &str, outputs: &'a [Device]) -> Option<&'a Device> {
    let (short, driver) = match input_name.split_once(" (") {
        Some((s, d)) => (s, format!(" ({d}")),
        None => (input_name, String::new()),
    };
    let lower = short.to_lowercase();
    let pos = lower.rfind("output")?;
    let want = format!("{}Input{}{}", &short[..pos], &short[pos + "output".len()..], driver).to_lowercase();
    outputs.iter().find(|d| d.name.to_lowercase() == want)
}

fn open_device(id: &str) -> Result<IMMDevice> {
    unsafe { enumerator()?.GetDevice(&windows::core::HSTRING::from(id)) }
}

/// The masking layer, applied to every stream Magic Mixer plays: mark it as
/// plain "Other" audio in RAW mode, so Windows applies no enhancements,
/// effects or voice processing on the way into the device. The receiving app
/// gets bare samples. Must be called before Initialize. Returns false if the
/// device doesn't support RAW (the stream still plays, just not raw).
pub fn mask_stream(client: &IAudioClient) -> bool {
    use windows::core::Interface;
    unsafe {
        let Ok(c2) = client.cast::<IAudioClient2>() else { return false };
        let props = AudioClientProperties {
            cbSize: std::mem::size_of::<AudioClientProperties>() as u32,
            bIsOffload: false.into(),
            eCategory: AudioCategory_Other,
            Options: AUDCLNT_STREAMOPTIONS_RAW,
        };
        if c2.SetClientProperties(&props).is_ok() {
            return true;
        }
        // RAW not supported here: still set the neutral category.
        let props = AudioClientProperties { Options: AUDCLNT_STREAMOPTIONS_NONE, ..props };
        let _ = c2.SetClientProperties(&props);
        false
    }
}

/// True if `device_id` accepts RAW (unprocessed) streams from Magic Mixer.
pub fn supports_raw(device_id: &str) -> bool {
    unsafe {
        let Ok(device) = open_device(device_id) else { return false };
        let Ok(client) = device.Activate::<IAudioClient>(CLSCTX_ALL, None) else { return false };
        if !mask_stream(&client) {
            return false;
        }
        let fmt = mix_format();
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, 300_000, 0, &fmt, None)
            .is_ok()
    }
}

/// Play audio from `ring` to output device `device_id` until `stop` is set,
/// applying `gain`. Blocks the calling thread.
pub fn play_to_device(device_id: &str, ring: Arc<Ring>, gain: Arc<Gain>, stop: Arc<AtomicBool>) -> Result<()> {
    use windows::core::Interface;
    let _com = crate::com::ComGuard::new()?;
    unsafe {
        let device = open_device(device_id)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let raw = mask_stream(&client);
        let fmt = mix_format();
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            300_000, // 30 ms
            0,
            &fmt,
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let buffer_frames = client.GetBufferSize()?;
        let render: IAudioRenderClient = client.GetService()?;
        // Never let Windows turn routed audio down when a call starts.
        if let Ok(session) = client.GetService::<IAudioSessionControl>() {
            if let Ok(s2) = session.cast::<IAudioSessionControl2>() {
                let _ = s2.SetDuckingPreference(true);
            }
        }
        let _ = raw;
        let channels = fmt.nChannels as usize;
        let rate = fmt.nSamplesPerSec as usize;

        // Pre-roll ~20 ms so the first callbacks don't starve.
        let preroll = rate / 50 * channels;
        while ring.len() < preroll && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        client.Start()?;

        let mut tmp: Vec<f32> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                continue;
            }
            let padding = client.GetCurrentPadding()?;
            let free = (buffer_frames - padding) as usize;
            if free == 0 {
                continue;
            }
            tmp.clear();
            tmp.resize(free * channels, 0.0);
            let got = ring.pop(&mut tmp);
            // If the source is late, fill with silence rather than glitch-looping.
            let g = gain.effective();
            let mut peak = 0f32;
            for s in &mut tmp[..got] {
                *s = (*s * g).clamp(-1.0, 1.0);
                peak = peak.max(s.abs());
            }
            gain.report_peak(peak);
            let ptr = render.GetBuffer(free as u32)?;
            std::ptr::copy_nonoverlapping(tmp.as_ptr(), ptr as *mut f32, free * channels);
            render.ReleaseBuffer(free as u32, 0)?;
        }
        let _ = client.Stop();
        let _ = CloseHandle(event);
    }
    Ok(())
}

/// Measure the level at a device for `seconds`: what arrives at a recording
/// device, or (with `loopback`) what is being played into a playback device.
/// Returns (peak, rms) over the whole window.
pub fn meter_input(device_id: &str, loopback: bool, seconds: f32) -> Result<(f32, f32)> {
    let _com = crate::com::ComGuard::new()?;
    unsafe {
        let device = open_device(device_id)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let fmt = mix_format();
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
                | if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 },
            1_000_000,
            0,
            &fmt,
            None,
        )?;
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        let channels = fmt.nChannels as usize;
        let (mut peak, mut sum_sq, mut count) = (0f32, 0f64, 0usize);
        let end = std::time::Instant::now() + std::time::Duration::from_secs_f32(seconds);
        while std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(10));
            while cap.GetNextPacketSize()? > 0 {
                let mut data: *mut u8 = std::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize * channels;
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) == 0 && !data.is_null() {
                    for &s in std::slice::from_raw_parts(data as *const f32, n) {
                        peak = peak.max(s.abs());
                        sum_sq += (s as f64) * (s as f64);
                    }
                }
                count += n;
                cap.ReleaseBuffer(frames)?;
            }
        }
        let _ = client.Stop();
        let rms = if count > 0 { (sum_sq / count as f64).sqrt() as f32 } else { 0.0 };
        Ok((peak, rms))
    }
}


/// Make `device_id` the Windows default device for all roles (console,
/// multimedia, communications). Uses IPolicyConfig: undocumented but stable
/// since Windows 7 and used by the Sound control panel itself.
pub fn set_default_device(device_id: &str) -> Result<()> {
    use windows::core::{GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR};
    const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
    const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
    // Vtable slot of SetDefaultEndpoint: 3 IUnknown methods + 10 before it.
    const SET_DEFAULT_ENDPOINT: usize = 13;
    type SetDefaultFn = unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, ERole) -> HRESULT;
    unsafe {
        let unk: IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)?;
        let mut cfg: *mut std::ffi::c_void = std::ptr::null_mut();
        unk.query(&IID_POLICY_CONFIG, &mut cfg).ok()?;
        let unk_cfg = IUnknown::from_raw(cfg); // releases on drop
        let vtbl = *(cfg as *const *const usize);
        let set_default: SetDefaultFn = std::mem::transmute(*vtbl.add(SET_DEFAULT_ENDPOINT));
        let id = HSTRING::from(device_id);
        for role in [eConsole, eMultimedia, eCommunications] {
            set_default(cfg, PCWSTR(id.as_ptr()), role).ok()?;
        }
        drop(unk_cfg);
    }
    Ok(())
}

//! Small COM helpers shared by the other modules.

use windows::core::Result;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// The one audio format used on every route: 48 kHz, stereo, 32-bit float.
/// WASAPI converts to/from each device's native format for us
/// (AUTOCONVERTPCM), so routes never have to resample themselves.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

pub fn mix_format() -> windows::Win32::Media::Audio::WAVEFORMATEX {
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    let block_align = CHANNELS * 4;
    windows::Win32::Media::Audio::WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: CHANNELS,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// RAII guard: initialises COM (multithreaded apartment) for the current thread.
pub struct ComGuard;

impl ComGuard {
    pub fn new() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(ComGuard)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

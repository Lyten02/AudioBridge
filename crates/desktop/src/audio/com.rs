//! Small COM/Win32 RAII helpers for the audio threads.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, SetEvent,
};

/// MTA membership for the current thread.
pub struct Com {
    initialized: bool,
}

impl Com {
    pub fn init() -> Self {
        // SAFETY: balanced by CoUninitialize in Drop when it succeeded.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self { initialized: hr.is_ok() }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: matches the successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

pub fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    // SAFETY: COM is initialised on the calling thread.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// Auto-reset Win32 event that can be shared between threads.
pub struct Event(HANDLE);
// SAFETY: event handles are usable from any thread.
unsafe impl Send for Event {}
// SAFETY: SetEvent/Wait are thread-safe kernel calls.
unsafe impl Sync for Event {}

impl Event {
    pub fn new() -> windows::core::Result<Self> {
        // SAFETY: anonymous auto-reset event.
        unsafe { CreateEventW(None, false, false, PCWSTR::null()).map(Self) }
    }
    pub fn handle(&self) -> HANDLE {
        self.0
    }
    pub fn set(&self) {
        // SAFETY: valid handle owned by self.
        unsafe {
            let _ = SetEvent(self.0);
        }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Registers the current thread with MMCSS ("Pro Audio") for glitch-free scheduling.
pub struct MmcssGuard(Option<HANDLE>);

impl MmcssGuard {
    pub fn pro_audio() -> Self {
        let mut index = 0u32;
        // SAFETY: valid task name and out pointer.
        let h = unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut index) };
        Self(h.ok())
    }
}

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            // SAFETY: handle returned by AvSetMmThreadCharacteristicsW on this thread.
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(h);
            }
        }
    }
}

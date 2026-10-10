//! Single instance: a named mutex plus a named auto-reset event the primary waits on.
//! A second launch signals the event (after granting foreground rights) and exits.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
};
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

fn object_name(kind: &str) -> HSTRING {
    #[cfg(debug_assertions)]
    if let Some(dir) = crate::paths::dev_dir() {
        use std::hash::{Hash, Hasher};
        let canonical = std::fs::canonicalize(&dir).unwrap_or(dir);
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        canonical.to_string_lossy().to_lowercase().hash(&mut hash);
        return HSTRING::from(format!("Local\\AudioBridge.Dev.{:016x}.{kind}", hash.finish()));
    }
    HSTRING::from(format!("Local\\AudioBridge.{kind}"))
}

/// An owned kernel handle that may be moved across threads.
pub struct OwnedHandle(pub HANDLE);
// SAFETY: kernel object handles are process-global and usable from any thread.
unsafe impl Send for OwnedHandle {}
// SAFETY: see above; we only pass the handle value to thread-safe kernel APIs.
unsafe impl Sync for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Held by the primary instance for its whole lifetime.
pub struct Primary {
    _mutex: OwnedHandle,
    show_event: OwnedHandle,
    quit_event: OwnedHandle,
}

/// Returns `Some(Primary)` if this is the only instance, `None` if another one runs.
pub fn acquire() -> anyhow::Result<Option<Primary>> {
    // SAFETY: plain kernel object creation with names valid for each call.
    unsafe {
        let mutex = OwnedHandle(CreateMutexW(None, false, &object_name("Instance"))?);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(None);
        }
        let show_event = OwnedHandle(CreateEventW(None, false, false, &object_name("Show"))?);
        let quit_event = OwnedHandle(CreateEventW(None, false, false, &object_name("Quit"))?);
        Ok(Some(Primary { _mutex: mutex, show_event, quit_event }))
    }
}

/// True if a primary instance currently exists.
pub fn another_running() -> bool {
    // SAFETY: as in `acquire`; the handle is closed right away.
    unsafe {
        match CreateMutexW(None, false, &object_name("Instance")) {
            Ok(h) => {
                let exists = GetLastError() == ERROR_ALREADY_EXISTS;
                let _ = CloseHandle(h);
                exists
            }
            Err(_) => false,
        }
    }
}

fn signal(name: PCWSTR) {
    // SAFETY: plain kernel calls; the opened handle is closed by OwnedHandle.
    unsafe {
        if let Ok(h) = OpenEventW(EVENT_MODIFY_STATE, false, name) {
            let h = OwnedHandle(h);
            let _ = SetEvent(h.0);
        }
    }
}

/// Asks the running instance to show its window.
pub fn signal_existing() {
    // SAFETY: plain user32 call.
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    signal(PCWSTR(object_name("Show").as_ptr()));
}

/// Asks the running instance to exit (used before replacing its exe).
pub fn signal_quit() {
    signal(PCWSTR(object_name("Quit").as_ptr()));
}

fn listen(name: &str, event: &OwnedHandle, on_fire: impl Fn() + Send + 'static) {
    let event = event.0 .0 as usize;
    let spawned = std::thread::Builder::new().name(name.into()).spawn(move || loop {
        // SAFETY: the event handle lives as long as `Primary`, which lives until process exit.
        let r = unsafe { WaitForSingleObject(HANDLE(event as *mut _), INFINITE) };
        if r.0 != 0 {
            return;
        }
        on_fire();
    });
    if let Err(e) = spawned {
        tracing::error!("cannot spawn {name} thread: {e}");
    }
}

impl Primary {
    /// Calls `on_show` (on a dedicated thread) each time another launch asks to show the window.
    pub fn listen_for_show(&self, on_show: impl Fn() + Send + 'static) {
        listen("instance-show", &self.show_event, on_show);
    }

    /// Calls `on_quit` when a newer copy asks this instance to exit.
    pub fn listen_for_quit(&self, on_quit: impl Fn() + Send + 'static) {
        listen("instance-quit", &self.quit_event, on_quit);
    }
}

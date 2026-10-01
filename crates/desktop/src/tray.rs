//! Notification-area icon on its own thread with its own message loop, so it keeps working
//! whether or not the egui window exists. Blocks in GetMessage: zero CPU while idle.

use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::thread::JoinHandle;

use anyhow::{anyhow, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem, TrackPopupMenu,
    TranslateMessage, HICON, ICONINFO, MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG, SM_CXSMICON,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_CONTEXTMENU, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
};

use crate::icon;
use crate::shared::shared;

const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
pub const WM_TRAY_REFRESH: u32 = WM_APP + 2;
const WM_TRAY_QUIT: u32 = WM_APP + 3;
const TRAY_ID: u32 = 1;

const ID_OPEN: usize = 1;
const ID_PC: usize = 2;
const ID_MIC: usize = 3;
const ID_EXIT: usize = 4;

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static ICON_ON: AtomicIsize = AtomicIsize::new(0);
static ICON_OFF: AtomicIsize = AtomicIsize::new(0);

pub struct Tray {
    hwnd: isize,
    join: JoinHandle<()>,
}

impl Tray {
    /// Removes the icon and ends the tray thread.
    pub fn quit(self) {
        // SAFETY: posting to our own window.
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_TRAY_QUIT, WPARAM(0), LPARAM(0));
        }
        let _ = self.join.join();
    }
}

pub fn spawn() -> Result<Tray> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<isize>>();
    let join = std::thread::Builder::new().name("tray".into()).spawn(move || {
        // SAFETY: window creation and message loop on this thread.
        let hwnd = unsafe { create_window() };
        let hwnd = match hwnd {
            Ok(h) => h,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        shared().set_tray_hwnd(hwnd.0 as isize);
        // SAFETY: creating icons and adding the notification icon for our own window.
        unsafe {
            let size = GetSystemMetrics(SM_CXSMICON).max(16) as u32;
            ICON_ON.store(make_icon(size, true).map_or(0, |h| h.0 as isize), Ordering::SeqCst);
            ICON_OFF.store(make_icon(size, false).map_or(0, |h| h.0 as isize), Ordering::SeqCst);
            notify(hwnd, NIM_ADD);
        }
        let _ = tx.send(Ok(hwnd.0 as isize));
        let mut msg = MSG::default();
        // SAFETY: standard message loop.
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            for icon in [&ICON_ON, &ICON_OFF] {
                let h = icon.swap(0, Ordering::SeqCst);
                if h != 0 {
                    let _ = DestroyIcon(HICON(h as *mut _));
                }
            }
        }
    })?;
    let hwnd = rx.recv().map_err(|_| anyhow!("tray thread exited"))??;
    Ok(Tray { hwnd, join })
}

unsafe fn create_window() -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("AudioBridgeTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(anyhow!("RegisterClassW failed"));
        }
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::SeqCst);
        // A hidden top-level (not message-only) window, so it receives the TaskbarCreated broadcast.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("AudioBridgeTray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        Ok(hwnd)
    }
}

/// Builds an HICON from the code-drawn RGBA image.
unsafe fn make_icon(size: u32, connected: bool) -> Result<HICON> {
    let rgba = icon::render(size, connected);
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let dst = std::slice::from_raw_parts_mut(bits.cast::<u8>(), rgba.len());
        for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(rgba.as_chunks::<4>().0) {
            *d = [s[2], s[1], s[0], s[3]];
        }
        let stride = size.div_ceil(16) as usize * 2;
        let mask_bits = vec![0u8; stride * size as usize];
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, Some(mask_bits.as_ptr().cast()));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        Ok(icon?)
    }
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let cap = dst.len() - 1;
    let mut n = 0;
    for (d, c) in dst.iter_mut().take(cap).zip(s.encode_utf16()) {
        *d = c;
        n += 1;
    }
    dst[n] = 0;
}

unsafe fn notify(hwnd: HWND, op: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE) {
    let (connected, tip) = shared().tray_state();
    let icon = if connected { &ICON_ON } else { &ICON_OFF };
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        hIcon: HICON(icon.load(Ordering::SeqCst) as *mut _),
        ..Default::default()
    };
    copy_wide(&mut data.szTip, &tip);
    // SAFETY: `data` is fully initialised for the call.
    unsafe {
        let _ = Shell_NotifyIconW(op, &data);
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let s = shared().settings();
    let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, w!("Открыть"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING | check(s.pc_audio_enabled), ID_PC, w!("Звук компьютера"));
        let _ = AppendMenuW(menu, MF_STRING | check(s.mic_enabled), ID_MIC, w!("Микрофон"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Выход"));
        let _ = SetMenuDefaultItem(menu, ID_OPEN as u32, 0);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match cmd.0 as usize {
            ID_OPEN => shared().show_window(),
            ID_PC => shared().set_pc_audio(!s.pc_audio_enabled),
            ID_MIC => shared().set_mic(!s.mic_enabled),
            ID_EXIT => shared().request_exit(),
            _ => {}
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: all calls operate on our own window on its owning thread.
    unsafe {
        match msg {
            WM_TRAY_CALLBACK => {
                match (lparam.0 as u32) & 0xFFFF {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => shared().show_window(),
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TRAY_REFRESH => {
                notify(hwnd, NIM_MODIFY);
                LRESULT(0)
            }
            WM_TRAY_QUIT => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                notify(hwnd, NIM_DELETE);
                PostQuitMessage(0);
                LRESULT(0)
            }
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::SeqCst) => {
                notify(hwnd, NIM_ADD);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

//! Windows notification-area ("system tray") icon.
//!
//! Mirrors the macOS menu bar status item: closing the main window hides
//! it instead of quitting, and the tray icon brings it back or quits the
//! app. Left-click (or double-click) on the icon opens the window, and
//! right-click shows a small menu (Open, "Stay here during fullscreen games", Quit).
//!
//! The icon is owned by a hidden top-level Win32 window created on the
//! GTK thread. GDK's Win32 event source pumps every message queued on
//! that thread (`PeekMessage` with a null `HWND`), so our window
//! procedure runs from the regular GLib main loop without a separate
//! message loop.

use std::cell::RefCell;

use adw::prelude::*;
use gtk::{gdk_pixbuf::Pixbuf, gio, glib};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
            DeleteObject,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
                DestroyIcon, DestroyMenu, DestroyWindow, GetCursorPos, GetSystemMetrics, HICON,
                ICONINFO, IDI_APPLICATION, LoadIconW, MF_CHECKED, MF_SEPARATOR, MF_STRING,
                MF_UNCHECKED, PostMessageW, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON,
                SM_CYSMICON, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
                TrackPopupMenu, WINDOW_EX_STYLE, WM_APP, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL,
                WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
            },
        },
    },
    core::{PCWSTR, w},
};

use crate::window::Window;

const ICON_RESOURCE: &str = "/de/feschber/LanMouse/icons/de.feschber.LanMouse.svg";
const TRAY_ICON_ID: u32 = 1;
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
const MENU_OPEN: usize = 1;
const MENU_QUIT: usize = 2;
const MENU_FULLSCREEN: usize = 3;

struct Tray {
    app: glib::WeakRef<adw::Application>,
    window: glib::WeakRef<Window>,
    _hold: gio::ApplicationHoldGuard,
    hwnd: HWND,
    icon: HICON,
    /// Icon was rendered by us (vs. a shared system icon) and must be
    /// released with `DestroyIcon`.
    owns_icon: bool,
    /// Broadcast by Explorer after it (re)starts; the icon must be
    /// re-added or it silently disappears.
    taskbar_created: u32,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

/// Create the tray icon. Must be called on the GTK thread.
pub fn setup(app: &adw::Application, window: &Window) {
    let initialized = TRAY.with(|tray| {
        let mut tray = tray.borrow_mut();
        if let Some(existing) = tray.as_mut() {
            existing.app.set(Some(app));
            existing.window.set(Some(window));
            true
        } else {
            false
        }
    });
    if initialized {
        return;
    }

    let hwnd = match unsafe { create_message_window() } {
        Ok(hwnd) => hwnd,
        Err(e) => {
            log::error!("could not create tray window: {e}");
            return;
        }
    };

    let (icon, owns_icon) = match unsafe { load_tray_icon() } {
        Some(icon) => (icon, true),
        None => {
            log::warn!("could not render tray icon from {ICON_RESOURCE}, using default");
            let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();
            (icon, false)
        }
    };

    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };

    let tray = Tray {
        app: app.downgrade(),
        window: window.downgrade(),
        _hold: app.hold(),
        hwnd,
        icon,
        owns_icon,
        taskbar_created,
    };
    tray.add_icon();
    TRAY.with(|t| t.replace(Some(tray)));

    // Remove the icon on exit; otherwise it lingers in the notification
    // area until the user hovers over it.
    app.connect_shutdown(|_| teardown());

    log::debug!("windows tray icon ready");
}

fn teardown() {
    let Some(tray) = TRAY.with(|t| t.take()) else {
        return;
    };
    unsafe {
        let data = tray.notify_data();
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        if tray.owns_icon {
            let _ = DestroyIcon(tray.icon);
        }
        let _ = DestroyWindow(tray.hwnd);
    }
}

impl Tray {
    fn notify_data(&self) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY_CALLBACK,
            hIcon: self.icon,
            ..Default::default()
        };
        let tip: Vec<u16> = "Lan Mouse".encode_utf16().collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);
        data
    }

    fn add_icon(&self) {
        let data = self.notify_data();
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            log::warn!("Shell_NotifyIconW(NIM_ADD) failed");
        }
    }
}

unsafe fn create_message_window() -> windows::core::Result<HWND> {
    let class_name = w!("LanMouseTrayWindow");
    let instance = unsafe { GetModuleHandleW(None)? };
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    // Returns 0 if the class already exists, which is fine: CreateWindowExW
    // below fails loudly if registration genuinely did not happen.
    unsafe { RegisterClassW(&class) };
    // A real (never shown) top-level window rather than an HWND_MESSAGE
    // one, because message-only windows do not receive the
    // "TaskbarCreated" broadcast.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("Lan Mouse"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_TRAY_CALLBACK {
        // Legacy (pre NOTIFYICON_VERSION_4) callback: the mouse message
        // is in the low word of lparam.
        match (lparam.0 & 0xffff) as u32 {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK => defer(present_window),
            WM_RBUTTONUP => unsafe { show_menu(hwnd) },
            _ => {}
        }
        return LRESULT(0);
    }

    let taskbar_created = TRAY.with(|t| t.borrow().as_ref().map(|t| t.taskbar_created));
    if taskbar_created.is_some_and(|m| m != 0 && m == msg) {
        log::debug!("taskbar recreated, re-adding tray icon");
        TRAY.with(|t| {
            if let Some(tray) = t.borrow().as_ref() {
                tray.add_icon();
            }
        });
        return LRESULT(0);
    }

    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    unsafe {
        let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN, w!("Open Lan Mouse"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let check = if crate::windows_fullscreen::is_enabled() {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
        let _ = AppendMenuW(
            menu,
            MF_STRING | check,
            MENU_FULLSCREEN,
            w!("Stay here during fullscreen games"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Quit Lan Mouse"));

        let mut pos = POINT::default();
        let _ = GetCursorPos(&mut pos);
        // Required so the menu closes when the user clicks elsewhere
        // (see the Remarks section of the TrackPopupMenu docs).
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            pos.x,
            pos.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);

        match cmd.0 as usize {
            MENU_OPEN => defer(present_window),
            MENU_QUIT => defer(quit),
            MENU_FULLSCREEN => defer(crate::windows_fullscreen::toggle),
            _ => {}
        }
    }
}

/// Run GTK work from the main loop rather than from inside the window
/// procedure, which may be running in a nested modal loop.
fn defer(f: fn()) {
    glib::idle_add_local_once(f);
}

fn present_window() {
    let window = TRAY.with(|t| t.borrow().as_ref().and_then(|t| t.window.upgrade()));
    if let Some(window) = window {
        window.present();
    }
}

fn quit() {
    let app = TRAY.with(|t| t.borrow().as_ref().and_then(|t| t.app.upgrade()));
    if let Some(app) = app {
        app.quit();
    }
}

/// Render the app's SVG icon at the small-icon size for the current DPI
/// and turn it into an `HICON`.
unsafe fn load_tray_icon() -> Option<HICON> {
    let width = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    let height = unsafe { GetSystemMetrics(SM_CYSMICON) }.max(16);
    let pixbuf = Pixbuf::from_resource_at_scale(ICON_RESOURCE, width, height, true)
        .inspect_err(|e| log::debug!("tray icon: {e}"))
        .ok()?;
    let pixbuf = if pixbuf.has_alpha() {
        pixbuf
    } else {
        pixbuf.add_alpha(false, 0, 0, 0).ok()?
    };
    unsafe { hicon_from_rgba(&pixbuf) }
}

unsafe fn hicon_from_rgba(pixbuf: &Pixbuf) -> Option<HICON> {
    let width = pixbuf.width();
    let height = pixbuf.height();
    let stride = pixbuf.rowstride() as usize;
    let channels = pixbuf.n_channels() as usize;
    let src = pixbuf.read_pixel_bytes();

    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative height: top-down rows, matching the pixbuf layout.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits = std::ptr::null_mut();
    let color =
        unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
    let len = (width * height) as usize;
    // SAFETY: CreateDIBSection allocated width*height 32-bit pixels.
    let dst = unsafe { std::slice::from_raw_parts_mut(bits as *mut u8, len * 4) };
    for y in 0..height as usize {
        for x in 0..width as usize {
            let s = y * stride + x * channels;
            let d = (y * width as usize + x) * 4;
            // RGBA -> BGRA
            dst[d] = src[s + 2];
            dst[d + 1] = src[s + 1];
            dst[d + 2] = src[s];
            dst[d + 3] = src[s + 3];
        }
    }

    // With a 32-bit color bitmap the alpha channel drives transparency;
    // the monochrome mask only needs to exist.
    let mask = unsafe { CreateBitmap(width, height, 1, 1, None) };
    let icon_info = ICONINFO {
        fIcon: true.into(),
        hbmMask: mask,
        hbmColor: color,
        ..Default::default()
    };
    let icon = unsafe { CreateIconIndirect(&icon_info) };
    unsafe {
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
    }
    icon.inspect_err(|e| log::debug!("CreateIconIndirect: {e}"))
        .ok()
}

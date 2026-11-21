//! "Start with Windows" support.
//!
//! Autostart is the per-user `Run` registry key, which is what the
//! Settings > Apps > Startup page lists and lets the user toggle. The
//! entry launches the current executable with `--hidden`, so a login
//! start goes straight to the notification area (see `windows_tray`).

use std::{cell::Cell, rc::Rc};

use adw::prelude::*;
use gtk::glib;
use windows::{
    Win32::{
        Foundation::ERROR_FILE_NOT_FOUND,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey,
            RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
        },
    },
    core::w,
};

use crate::window::Window;

const RUN_KEY: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE_NAME: windows::core::PCWSTR = w!("Lan Mouse");

/// Whether a "Lan Mouse" entry exists in the current user's Run key.
pub fn is_enabled() -> bool {
    let res = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
    };
    res.is_ok()
}

/// Add or remove the Run key entry.
pub fn set_enabled(enabled: bool) -> windows::core::Result<()> {
    let mut key = HKEY::default();
    unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key) }.ok()?;
    let res = if enabled {
        command_line().and_then(|cmd| {
            // REG_SZ data is UTF-16 including the terminating NUL.
            let bytes: Vec<u8> = cmd
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(|c| c.to_le_bytes())
                .collect();
            unsafe { RegSetValueExW(key, VALUE_NAME, None, REG_SZ, Some(&bytes)) }.ok()
        })
    } else {
        match unsafe { RegDeleteValueW(key, VALUE_NAME) } {
            ERROR_FILE_NOT_FOUND => Ok(()),
            e => e.ok(),
        }
    };
    let _ = unsafe { RegCloseKey(key) };
    res
}

fn command_line() -> windows::core::Result<String> {
    let exe = std::env::current_exe()
        .map_err(|_| windows::core::Error::from(ERROR_FILE_NOT_FOUND.to_hresult()))?;
    Ok(format!("\"{}\" --hidden", exe.display()))
}

/// Add the "Start with Windows" switch to the General settings group.
pub fn add_settings_row(window: &Window) {
    // Point an existing entry at this executable, so autostart keeps
    // working after an update moves the install.
    if is_enabled() {
        if let Err(e) = set_enabled(true) {
            log::warn!("could not refresh autostart entry: {e}");
        }
    }

    let switch = gtk::Switch::builder()
        .valign(gtk::Align::Center)
        .active(is_enabled())
        .build();
    let row = adw::ActionRow::builder()
        .title("Start with Windows")
        .subtitle("Open Lan Mouse in the notification area when you sign in")
        .activatable_widget(&switch)
        .build();
    row.add_suffix(&switch);

    let window_weak = window.downgrade();
    // Set while we put the switch back after a failed registry write, so
    // the resulting nested `state-set` is not treated as a user toggle.
    let reverting = Rc::new(Cell::new(false));
    switch.connect_state_set(move |switch, enabled| {
        if reverting.get() {
            return glib::Propagation::Proceed;
        }
        if let Err(e) = set_enabled(enabled) {
            log::warn!("could not update autostart: {e}");
            if let Some(window) = window_weak.upgrade() {
                window.show_toast(&format!("could not change autostart: {e}"));
            }
            reverting.set(true);
            switch.set_state(!enabled);
            reverting.set(false);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });

    window.add_general_row(&row);
}

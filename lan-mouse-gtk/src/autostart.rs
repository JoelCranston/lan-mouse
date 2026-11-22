//! "Start at login" switch in the General settings group.
//!
//! The platform backend registers the app to launch with `--hidden` at
//! login, so it starts in the tray / menu bar without opening a window.

use std::{cell::Cell, rc::Rc};

use adw::prelude::*;
use gtk::glib;

#[cfg(target_os = "macos")]
use crate::macos_autostart as backend;
use crate::window::Window;
#[cfg(windows)]
use crate::windows_autostart as backend;

pub fn add_settings_row(window: &Window) {
    // Point an existing entry at this executable, so autostart keeps
    // working after an update moves the install.
    if backend::is_enabled() {
        if let Err(e) = backend::set_enabled(true) {
            log::warn!("could not refresh autostart entry: {e}");
        }
    }

    let switch = gtk::Switch::builder()
        .valign(gtk::Align::Center)
        .active(backend::is_enabled())
        .build();
    let row = adw::ActionRow::builder()
        .title(backend::TITLE)
        .subtitle(backend::SUBTITLE)
        .activatable_widget(&switch)
        .build();
    row.add_suffix(&switch);

    let window_weak = window.downgrade();
    // Set while we put the switch back after a failed write, so the
    // resulting nested `state-set` is not treated as a user toggle.
    let reverting = Rc::new(Cell::new(false));
    switch.connect_state_set(move |switch, enabled| {
        if reverting.get() {
            return glib::Propagation::Proceed;
        }
        if let Err(e) = backend::set_enabled(enabled) {
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

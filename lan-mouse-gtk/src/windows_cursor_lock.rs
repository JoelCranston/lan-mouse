//! "Stay here while a game locks the cursor" setting.
//!
//! When enabled, the service does not hand the cursor to another device
//! while an application (typically a fullscreen game) has confined the
//! cursor to its window with `ClipCursor`. The setting lives in the
//! service's config file; this module shows it as a switch in the General
//! settings group and as a check item in the tray menu.

use std::cell::{Cell, RefCell};

use adw::prelude::*;
use gtk::glib;
use lan_mouse_ipc::FrontendRequest;

use crate::window::Window;

const TITLE: &str = "Stay here when the cursor is locked";
const SUBTITLE: &str = "Don't move to another device while an app, like a fullscreen game, locks the cursor to its window";

thread_local! {
    /// Last state reported by the service (on by default).
    static ENABLED: Cell<bool> = const { Cell::new(true) };
    /// Set while we update the switch from a service event, so that
    /// `state-set` is not sent back as a user toggle.
    static SYNCING: Cell<bool> = const { Cell::new(false) };
    static SWITCH: RefCell<glib::WeakRef<gtk::Switch>> = RefCell::new(glib::WeakRef::new());
    static WINDOW: RefCell<glib::WeakRef<Window>> = RefCell::new(glib::WeakRef::new());
}

pub fn add_settings_row(window: &Window) {
    let switch = gtk::Switch::builder()
        .valign(gtk::Align::Center)
        .active(ENABLED.get())
        .build();
    let row = adw::ActionRow::builder()
        .title(TITLE)
        .subtitle(SUBTITLE)
        .activatable_widget(&switch)
        .build();
    row.add_suffix(&switch);

    switch.connect_state_set(|_, enabled| {
        if !SYNCING.get() {
            request(enabled);
        }
        glib::Propagation::Proceed
    });

    SWITCH.with(|s| s.borrow().set(Some(&switch)));
    WINDOW.with(|w| w.borrow().set(Some(window)));
    window.add_general_row(&row);
}

/// The service reported the current setting.
pub fn set_state(enabled: bool) {
    ENABLED.set(enabled);
    if let Some(switch) = SWITCH.with(|s| s.borrow().upgrade()) {
        if switch.is_active() != enabled {
            SYNCING.set(true);
            switch.set_active(enabled);
            SYNCING.set(false);
        }
    }
}

pub fn is_enabled() -> bool {
    ENABLED.get()
}

/// Flip the setting (used by the tray menu).
pub fn toggle() {
    request(!ENABLED.get());
}

fn request(enabled: bool) {
    // Update right away so the tray menu reflects the choice even before
    // the service confirms it.
    ENABLED.set(enabled);
    if let Some(window) = WINDOW.with(|w| w.borrow().upgrade()) {
        window.request(FrontendRequest::SetBlockWhenCursorLocked(enabled));
    }
}

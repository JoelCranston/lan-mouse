//! Stable identity for the Windows tray icon.
//!
//! Without a GUID, Windows 11 recognises a notification icon only by
//! heuristics (executable path plus some mix of `uID`, tooltip and icon
//! pixels). When those heuristics miss, every new build shows up as yet
//! another "Lan Mouse" entry under Settings > Personalization > Taskbar >
//! Other system tray icons, and the user's show/hide choice is lost.
//!
//! Registering the icon with `NIF_GUID` fixes that, but Windows binds a
//! GUID to the path of the executable that first used it: the same GUID
//! from a second location (an installed copy and a `target\debug` build,
//! say) fails to register. So instead of one hard-coded GUID, the GUID is
//! derived from the executable's path. Each install location gets its
//! own GUID, which stays the same across updates to that location.

use std::path::Path;

use windows::{
    Win32::{
        Foundation::{ERROR_SUCCESS, WIN32_ERROR},
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, RRF_RT_ANY, RRF_RT_REG_SZ, RegCloseKey,
            RegDeleteTreeW, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
        },
    },
    core::{GUID, HSTRING, PCWSTR, PWSTR, w},
};

/// Bumping this changes every derived GUID (and so resets the user's
/// show/hide choice once). Only do that if the old GUIDs become unusable.
const GUID_NAMESPACE: &str = "de.feschber.LanMouse/tray-icon/v1";

/// The executable name whose stale, GUID-less tray entries get removed.
const EXE_NAME: &str = "lan-mouse.exe";

/// GUID for the tray icon of the running executable, or `None` if the
/// executable path is unknown.
pub fn tray_guid() -> Option<GUID> {
    let exe = std::env::current_exe()
        .inspect_err(|e| log::warn!("tray icon: cannot resolve executable path: {e}"))
        .ok()?;
    Some(guid_for_path(&exe))
}

/// Derive a GUID from `path`. Paths on Windows are case-insensitive, so
/// the path is lowercased first. The hash (128-bit FNV-1a) must never
/// change, or every user's tray entry is duplicated once more.
fn guid_for_path(path: &Path) -> GUID {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let key = format!(
        "{GUID_NAMESPACE}\0{}",
        path.to_string_lossy().to_lowercase()
    );
    let mut hash = OFFSET;
    for byte in key.bytes() {
        hash ^= byte as u128;
        hash = hash.wrapping_mul(PRIME);
    }
    // Mark it as an RFC 9562 version 8 ("custom") UUID so it is well formed.
    hash = (hash & !(0xf << 76)) | (0x8 << 76);
    hash = (hash & !(0x3 << 62)) | (0x2 << 62);
    GUID::from_u128(hash)
}

/// Remove the duplicate "Other system tray icons" entries left behind by
/// earlier, GUID-less builds of Lan Mouse.
///
/// Windows keeps one subkey per icon it has seen under
/// `HKCU\Control Panel\NotifyIconSettings` and never removes them. Entries
/// for an icon registered with a GUID carry an `IconGuid` value, so a
/// `lan-mouse.exe` entry without one is a leftover from before this fix.
/// The key is undocumented, so any failure is logged and ignored.
///
/// Only call this once the GUID icon was added successfully; otherwise
/// the live, GUID-less icon's own entry would be removed too.
pub fn remove_stale_entries() {
    let root = match open_key(HKEY_CURRENT_USER, w!("Control Panel\\NotifyIconSettings")) {
        Ok(root) => root,
        Err(e) => {
            log::debug!("tray cleanup: NotifyIconSettings not available ({e:?})");
            return;
        }
    };

    let stale: Vec<String> = subkeys(root)
        .into_iter()
        .filter(|name| is_stale_entry(root, name))
        .collect();

    for name in &stale {
        let err = unsafe { RegDeleteTreeW(root, &HSTRING::from(name.as_str())) };
        if err == ERROR_SUCCESS {
            log::info!("removed stale tray icon entry NotifyIconSettings\\{name}");
        } else {
            log::warn!("could not remove stale tray icon entry {name}: {err:?}");
        }
    }

    unsafe {
        let _ = RegCloseKey(root);
    }
}

fn open_key(parent: HKEY, path: PCWSTR) -> Result<HKEY, WIN32_ERROR> {
    let mut key = HKEY::default();
    let err = unsafe { RegOpenKeyExW(parent, path, Some(0), KEY_READ | KEY_WRITE, &mut key) };
    if err == ERROR_SUCCESS {
        Ok(key)
    } else {
        Err(err)
    }
}

/// Names of all direct subkeys of `key`, collected up front so that
/// deleting entries afterwards does not upset the enumeration.
fn subkeys(key: HKEY) -> Vec<String> {
    let mut names = Vec::new();
    // Registry key names are at most 255 characters.
    let mut buf = [0u16; 256];
    for index in 0.. {
        let mut len = buf.len() as u32;
        let err = unsafe {
            RegEnumKeyExW(
                key,
                index,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if err != ERROR_SUCCESS {
            // ERROR_NO_MORE_ITEMS, or something we cannot recover from.
            break;
        }
        names.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    names
}

fn is_stale_entry(root: HKEY, name: &str) -> bool {
    let subkey = HSTRING::from(name);
    let Some(path) = string_value(root, &subkey, w!("ExecutablePath")) else {
        return false;
    };
    // ExecutablePath may start with a known-folder GUID instead of a
    // drive, e.g. `{6D809377-...}\Lan Mouse\lan-mouse.exe`.
    let file_name = path.rsplit(['\\', '/']).next().unwrap_or_default();
    if !file_name.eq_ignore_ascii_case(EXE_NAME) {
        return false;
    }
    !value_exists(root, &subkey, w!("IconGuid"))
}

fn string_value(root: HKEY, subkey: &HSTRING, value: PCWSTR) -> Option<String> {
    let mut size = 0u32;
    let err = unsafe {
        RegGetValueW(
            root,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if err != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    let err = unsafe {
        RegGetValueW(
            root,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if err != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn value_exists(root: HKEY, subkey: &HSTRING, value: PCWSTR) -> bool {
    let err = unsafe { RegGetValueW(root, subkey, value, RRF_RT_ANY, None, None, None) };
    err == ERROR_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_is_stable_and_case_insensitive() {
        let a = guid_for_path(Path::new(r"C:\Program Files\Lan Mouse\lan-mouse.exe"));
        let b = guid_for_path(Path::new(r"c:\program files\lan mouse\LAN-MOUSE.EXE"));
        assert_eq!(a, b);
        // Pinned: if this changes, every user gets a duplicate tray entry.
        assert_eq!(a, GUID::from_u128(PINNED_INSTALLED_GUID));
    }

    #[test]
    fn guid_differs_per_location() {
        let installed = guid_for_path(Path::new(r"C:\Program Files\Lan Mouse\lan-mouse.exe"));
        let debug = guid_for_path(Path::new(r"C:\dev\lan-mouse\target\debug\lan-mouse.exe"));
        assert_ne!(installed, debug);
    }

    const PINNED_INSTALLED_GUID: u128 = 0xb3e73a5c_df12_8314_9182_53bae38bd3a6;
}

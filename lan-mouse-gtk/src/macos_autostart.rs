//! macOS autostart backend for [`crate::autostart`].
//!
//! A login item cannot pass arguments, so autostart is a per-user
//! LaunchAgent that launchd runs once at login. It goes through
//! `open -a` for a bundled app, so the launch is a normal LaunchServices
//! launch and keeps the app's Accessibility grant.

use std::{fs, io, path::PathBuf};

use crate::macos_privacy;

pub const TITLE: &str = "Start at login";
pub const SUBTITLE: &str = "Open Lan Mouse in the menu bar when you log in";

const LABEL: &str = "de.feschber.LanMouse.autostart";

fn plist_path() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

pub fn is_enabled() -> bool {
    plist_path().is_ok_and(|p| p.exists())
}

pub fn set_enabled(enabled: bool) -> io::Result<()> {
    let path = plist_path()?;
    if !enabled {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        };
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, plist(&program_arguments()?))
}

fn program_arguments() -> io::Result<Vec<String>> {
    let exe = std::env::current_exe()?;
    Ok(match macos_privacy::bundle_path(&exe) {
        Ok(bundle) => vec![
            "/usr/bin/open".into(),
            "-a".into(),
            bundle.display().to_string(),
            "--args".into(),
            "--hidden".into(),
        ],
        // Not running from a .app (e.g. `cargo run`): start the binary.
        Err(_) => vec![exe.display().to_string(), "--hidden".into()],
    })
}

fn plist(args: &[String]) -> String {
    let args: String = args
        .iter()
        .map(|a| format!("        <string>{}</string>\n", xml_escape(a)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_escapes_arguments() {
        let p = plist(&["/Apps/A & B.app".into(), "--hidden".into()]);
        assert!(p.contains("<string>/Apps/A &amp; B.app</string>"));
        assert!(p.contains("<string>--hidden</string>"));
        assert!(p.contains("<true/>"));
    }
}

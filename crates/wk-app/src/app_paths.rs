//! Per-user settings path for the standalone W1VE Keyer.
//!
//! The Keyer has no network identity (no WireGuard key, no pairing), so it persists only the
//! operator's keyer/sidetone/port preferences to a single `settings.json`:
//!
//! ```text
//! Windows: %APPDATA%\W1VE Keyer\settings.json
//! other:   ~/.config/w1ve-keyer/settings.json
//! ```

use std::path::PathBuf;

const APP_DIR_WINDOWS: &str = "W1VE Keyer";
const APP_DIR_UNIX: &str = "w1ve-keyer";
const SETTINGS_FILE: &str = "settings.json";

/// The settings-file path, creating the parent directory if needed.
#[must_use]
pub fn settings_path() -> PathBuf {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join(SETTINGS_FILE)
}

fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join(APP_DIR_WINDOWS);
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(xdg).join(APP_DIR_UNIX);
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config").join(APP_DIR_UNIX);
        }
    }
    PathBuf::from(APP_DIR_UNIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_path_ends_with_the_settings_file() {
        assert!(settings_path().ends_with(SETTINGS_FILE));
    }
}

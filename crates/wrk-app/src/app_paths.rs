//! Per-user config paths for the Remote Keyer apps.
//!
//! Each role keeps a `config.json` holding its stable WireGuard identity, so pairing reuses
//! the same key across restarts. One file per role lets the Server and Client run on the same
//! machine during testing without clobbering each other:
//!
//! ```text
//! Windows: %APPDATA%\W1VE Remote Keyer\client.json  (or server.json)
//! other:   ~/.config/w1ve-remote-keyer/client.json  (or server.json)
//! ```

use std::path::PathBuf;

use rwk_core::config::Role;

const APP_DIR_WINDOWS: &str = "W1VE Remote Keyer";
const APP_DIR_UNIX: &str = "w1ve-remote-keyer";

/// The config-file path for `role`, creating the parent directory if needed.
#[must_use]
pub fn config_path(role: Role) -> PathBuf {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = match role {
        Role::Client => "client.json",
        Role::Station => "server.json",
    };
    dir.join(file)
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
    fn roles_resolve_distinct_files() {
        assert_ne!(config_path(Role::Client), config_path(Role::Station));
        assert!(config_path(Role::Client).ends_with("client.json"));
        assert!(config_path(Role::Station).ends_with("server.json"));
    }
}

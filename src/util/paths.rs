//! Locations of persistent files.

use std::env;
use std::path::PathBuf;

const APP_DIR: &str = "kenkurs";

/// Directory that holds `library.json` and `settings.json`.
///
/// Resolution order: `$KENKURS_CONFIG_DIR`, `$XDG_CONFIG_HOME/kenkurs`, `$HOME/.config/kenkurs`.
/// Relative values are ignored, as the XDG specification requires.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("KENKURS_CONFIG_DIR").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let absolute = |name: &str| env::var_os(name).map(PathBuf::from).filter(|path| path.is_absolute());
    if let Some(xdg) = absolute("XDG_CONFIG_HOME") {
        return Some(xdg.join(APP_DIR));
    }
    absolute("HOME").map(|home| home.join(".config").join(APP_DIR))
}

//! A small append-only file logger.
//!
//! Bitwig hides the plugin's stderr, so everything goes to
//! `$XDG_STATE_HOME/skoll/plugin.log`. Never call this from `process()`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

/// The log file path, from `XDG_STATE_HOME` or `HOME`.
pub fn log_path() -> Option<PathBuf> {
    state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref().map(Path::new),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )
    .map(|dir| dir.join("skoll").join("plugin.log"))
}

/// Resolves the XDG state directory. An empty or relative `XDG_STATE_HOME` is ignored, as the XDG
/// spec requires.
fn state_dir(xdg_state_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    match xdg_state_home {
        Some(dir) if dir.is_absolute() => Some(dir.to_path_buf()),
        _ => home.map(|home| home.join(".local").join("state")),
    }
}

fn open() -> Option<File> {
    let path = log_path()?;
    fs::create_dir_all(path.parent()?).ok()?;
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Appends one line to the log. Failures are ignored: logging must never break the plugin.
pub fn write(instance: u32, message: &str) {
    if cfg!(test) {
        return;
    }
    let file = LOG_FILE.get_or_init(|| Mutex::new(open()));
    let Ok(mut file) = file.lock() else { return };
    let Some(file) = file.as_mut() else { return };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // One `write` call per line: with O_APPEND, lines from several host processes don't interleave.
    let line = format!(
        "{}.{:03} [{}/{instance}] {message}\n",
        now.as_secs(),
        now.subsec_millis(),
        std::process::id()
    );
    let _ = file.write_all(line.as_bytes());
}

#[macro_export]
macro_rules! log {
    ($instance:expr, $($arg:tt)*) => {
        $crate::log::write($instance, &format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_absolute_xdg_state_home() {
        assert_eq!(
            state_dir(Some(Path::new("/x/state")), Some(Path::new("/home/a"))),
            Some(PathBuf::from("/x/state"))
        );
    }

    #[test]
    fn falls_back_to_home() {
        let expected = Some(PathBuf::from("/home/a/.local/state"));
        assert_eq!(state_dir(None, Some(Path::new("/home/a"))), expected);
        assert_eq!(
            state_dir(Some(Path::new("relative")), Some(Path::new("/home/a"))),
            expected
        );
        assert_eq!(
            state_dir(Some(Path::new("")), Some(Path::new("/home/a"))),
            expected
        );
    }

    #[test]
    fn no_home_means_no_path() {
        assert_eq!(state_dir(None, None), None);
    }
}

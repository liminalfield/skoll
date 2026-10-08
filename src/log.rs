//! A small append-only file logger.
//!
//! Bitwig hides the plugin's stderr, so everything goes to
//! `$XDG_STATE_HOME/skoll/plugin.log`. Never call this from `process()`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths;

static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

fn open() -> Option<File> {
    let path = paths::log_path()?;
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

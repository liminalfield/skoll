//! File locations: the config file, the log file and the mpv socket.

use std::env;
use std::hash::{BuildHasher, RandomState};
use std::path::{Path, PathBuf};

/// `$XDG_CONFIG_HOME/skoll/config.toml`, falling back to `~/.config/skoll/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config").map(|dir| dir.join("skoll").join("config.toml"))
}

/// `$XDG_STATE_HOME/skoll/plugin.log`, falling back to `~/.local/state/skoll/plugin.log`.
pub fn log_path() -> Option<PathBuf> {
    xdg_dir("XDG_STATE_HOME", ".local/state").map(|dir| dir.join("skoll").join("plugin.log"))
}

/// `$XDG_RUNTIME_DIR/skoll-<pid>-<random suffix>.sock`, falling back to the temp directory.
///
/// The random suffix keeps several plugin instances in one host process apart.
pub fn socket_path() -> PathBuf {
    let dir = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(env::temp_dir);
    // `RandomState` is seeded randomly per instance, so this needs no extra dependency.
    let suffix = RandomState::new().hash_one(std::process::id()) & 0xffff_ffff;
    dir.join(format!("skoll-{}-{suffix:08x}.sock", std::process::id()))
}

fn xdg_dir(var: &str, home_fallback: &str) -> Option<PathBuf> {
    resolve_xdg_dir(
        env::var_os(var).as_deref().map(Path::new),
        env::var_os("HOME").as_deref().map(Path::new),
        home_fallback,
    )
}

/// Resolves an XDG base directory. An empty or relative value is ignored, as the XDG spec
/// requires.
fn resolve_xdg_dir(
    xdg_value: Option<&Path>,
    home: Option<&Path>,
    home_fallback: &str,
) -> Option<PathBuf> {
    match xdg_value {
        Some(dir) if dir.is_absolute() => Some(dir.to_path_buf()),
        _ => home.map(|home| home.join(home_fallback)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_absolute_xdg_value() {
        assert_eq!(
            resolve_xdg_dir(
                Some(Path::new("/x/state")),
                Some(Path::new("/home/a")),
                ".s"
            ),
            Some(PathBuf::from("/x/state"))
        );
    }

    #[test]
    fn falls_back_to_home() {
        let home = Some(Path::new("/home/a"));
        let expected = Some(PathBuf::from("/home/a/.local/state"));
        assert_eq!(resolve_xdg_dir(None, home, ".local/state"), expected);
        assert_eq!(
            resolve_xdg_dir(Some(Path::new("relative")), home, ".local/state"),
            expected
        );
        assert_eq!(
            resolve_xdg_dir(Some(Path::new("")), home, ".local/state"),
            expected
        );
    }

    #[test]
    fn no_home_means_no_path() {
        assert_eq!(resolve_xdg_dir(None, None, ".config"), None);
    }

    #[test]
    fn socket_paths_are_unique() {
        let a = socket_path();
        let b = socket_path();
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(&format!("skoll-{}-", std::process::id())));
        assert!(name.ends_with(".sock"));
    }
}

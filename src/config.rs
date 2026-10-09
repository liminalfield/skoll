//! The optional config file, `$XDG_CONFIG_HOME/skoll/config.toml`.
//!
//! The file is read each time mpv launches, so edits take effect without a rebuild:
//! close the mpv window and Skoll relaunches it with the new settings.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::window::WindowGeometry;
use crate::{log, paths};

/// Flags Skoll needs for sync. The config file cannot remove these.
const CORE_FLAGS: &[&str] = &[
    "--idle=yes",
    "--force-window=yes",
    "--keep-open=yes",
    "--no-audio",
    "--hr-seek=yes",
    "--pause",
    "--no-terminal",
];

/// Window flags, tuned for X11 nested in Hyprland. `window_flags` in the config file replaces
/// these.
pub const DEFAULT_WINDOW_FLAGS: &[&str] = &[
    "--gpu-context=x11egl",
    "--ontop",
    "--no-border",
    "--geometry=480x270-0+0",
];

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The mpv binary. Defaults to `mpv` on `PATH`.
    pub mpv_path: Option<PathBuf>,
    /// Replaces [`DEFAULT_WINDOW_FLAGS`] when set.
    pub window_flags: Option<Vec<String>>,
    /// Added after the window flags.
    pub extra_flags: Vec<String>,
    /// A file dialog program and its arguments, run on right-click. It must print the chosen
    /// path. Without it, zenity, kdialog and yad are tried in turn.
    pub file_dialog: Option<Vec<String>>,
}

impl Config {
    /// Reads the config file. A missing file means the defaults. An unreadable or invalid file is
    /// logged and also means the defaults.
    pub fn load(instance: u32) -> Self {
        let Some(path) = paths::config_path() else {
            return Self::default();
        };
        match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(config) => {
                    log!(instance, "read config from {}", path.display());
                    config
                }
                Err(err) => {
                    log!(
                        instance,
                        "ignoring invalid config {}: {err}",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(err) if err.kind() == ErrorKind::NotFound => Self::default(),
            Err(err) => {
                log!(instance, "could not read config {}: {err}", path.display());
                Self::default()
            }
        }
    }

    pub fn mpv_path(&self) -> &Path {
        self.mpv_path.as_deref().unwrap_or(Path::new("mpv"))
    }

    /// Lua that configures the embedded mpv script: sets `configured_dialog`.
    pub fn script_prelude(&self) -> String {
        let dialog = match &self.file_dialog {
            Some(args) if !args.is_empty() => {
                let args: Vec<String> = args.iter().map(|a| lua_string(a)).collect();
                format!("{{ {} }}", args.join(", "))
            }
            _ => "nil".to_owned(),
        };
        format!("local configured_dialog = {dialog}")
    }

    /// The full mpv command line, without the binary. `geometry`, where the window last was,
    /// overrides any `--geometry` in the window flags.
    pub fn mpv_args(&self, socket: &Path, geometry: Option<WindowGeometry>) -> Vec<String> {
        let window_flags = match &self.window_flags {
            Some(flags) => flags.clone(),
            None => DEFAULT_WINDOW_FLAGS.iter().map(|&f| f.to_owned()).collect(),
        };
        CORE_FLAGS
            .iter()
            .map(|&f| f.to_owned())
            .chain(window_flags)
            .chain(geometry.map(|g| format!("--geometry={g}")))
            .chain(self.extra_flags.iter().cloned())
            // Last, so no other flag can move the socket.
            .chain([format!("--input-ipc-server={}", socket.display())])
            .collect()
    }
}

/// A Lua string literal.
fn lua_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn empty_file_means_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn default_args() {
        let args = Config::default().mpv_args(Path::new("/run/s.sock"), None);
        assert_eq!(
            args,
            [
                "--idle=yes",
                "--force-window=yes",
                "--keep-open=yes",
                "--no-audio",
                "--hr-seek=yes",
                "--pause",
                "--no-terminal",
                "--gpu-context=x11egl",
                "--ontop",
                "--no-border",
                "--geometry=480x270-0+0",
                "--input-ipc-server=/run/s.sock",
            ]
        );
        assert_eq!(Config::default().mpv_path(), Path::new("mpv"));
    }

    #[test]
    fn window_flags_replace_defaults_and_extra_flags_follow() {
        let config = parse(
            r#"
            mpv_path = "/opt/mpv/bin/mpv"
            window_flags = ["--geometry=640x360+0+0"]
            extra_flags = ["--osd-level=3"]
            "#,
        )
        .unwrap();
        let args = config.mpv_args(Path::new("/run/s.sock"), None);
        assert_eq!(
            &args[CORE_FLAGS.len()..],
            [
                "--geometry=640x360+0+0",
                "--osd-level=3",
                "--input-ipc-server=/run/s.sock",
            ]
        );
        // A remembered window position comes after the configured one, so it wins.
        let remembered = WindowGeometry {
            x: 100,
            y: 50,
            width: 800,
            height: 450,
        };
        let args = config.mpv_args(Path::new("/run/s.sock"), Some(remembered));
        assert_eq!(
            &args[CORE_FLAGS.len()..CORE_FLAGS.len() + 2],
            ["--geometry=640x360+0+0", "--geometry=800x450+100+50"]
        );
        assert_eq!(config.mpv_path(), Path::new("/opt/mpv/bin/mpv"));
    }

    #[test]
    fn script_prelude_quotes_the_dialog() {
        assert_eq!(
            Config::default().script_prelude(),
            "local configured_dialog = nil"
        );
        let config = parse(r#"file_dialog = ["my picker", "--title=\"Open\"", "C:\\x"]"#).unwrap();
        assert_eq!(
            config.script_prelude(),
            r#"local configured_dialog = { "my picker", "--title=\"Open\"", "C:\\x" }"#
        );
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(parse("window_flag = []").is_err());
    }
}

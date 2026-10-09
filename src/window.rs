//! Remembers where the mpv window is, so a relaunched mpv opens in the same place at the same
//! size.
//!
//! mpv reports its X11 window ID but not its position, so this asks the X server directly. Only
//! X11 windows are tracked: on Wayland a client cannot learn its own position.

use std::fmt;
use std::sync::RwLock;

use nih_plug::params::persist::PersistentField;
use serde::{Deserialize, Serialize};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
use x11rb::rust_connection::RustConnection;

use crate::log;

/// A window's outer position and size, in X11 root-window pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl fmt::Display for WindowGeometry {
    /// mpv's `--geometry` syntax: `WxH+X+Y`. A negative position would mean "from the right or
    /// bottom edge" there, so a window partly off the left or top edge is moved back on.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{}+{}+{}",
            self.width,
            self.height,
            self.x.max(0),
            self.y.max(0)
        )
    }
}

/// The last known window geometry: plugin state, saved with the project. Moving the window does
/// not mark the project as changed; the geometry is saved with the next save.
#[derive(Default)]
pub struct StoredWindow(pub RwLock<Option<WindowGeometry>>);

impl StoredWindow {
    pub fn get(&self) -> Option<WindowGeometry> {
        *self
            .0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn remember(&self, geometry: WindowGeometry) {
        *self
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(geometry);
    }
}

impl PersistentField<'_, Option<WindowGeometry>> for StoredWindow {
    fn set(&self, new_value: Option<WindowGeometry>) {
        *self
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = new_value;
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&Option<WindowGeometry>) -> R,
    {
        f(&self
            .0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
}

/// Queries window geometry from the X server named by `DISPLAY`, mpv's display too.
pub struct WindowTracker {
    instance: u32,
    connection: Option<RustConnection>,
    /// The `_NET_FRAME_EXTENTS` atom, on the current connection.
    frame_extents_atom: u32,
    /// Set after a failed connection, so it is logged and tried once.
    unavailable: bool,
}

impl WindowTracker {
    pub fn new(instance: u32) -> Self {
        Self {
            instance,
            connection: None,
            frame_extents_atom: 0,
            unavailable: false,
        }
    }

    /// The window's position on the root window and its size, or `None` if unknown.
    pub fn geometry(&mut self, window: u32) -> Option<WindowGeometry> {
        if self.connection.is_none() && !self.unavailable {
            match x11rb::connect(None) {
                Ok((connection, _screen)) => {
                    self.frame_extents_atom = connection
                        .intern_atom(false, b"_NET_FRAME_EXTENTS")
                        .ok()
                        .and_then(|cookie| cookie.reply().ok())
                        .map_or(0, |reply| reply.atom);
                    self.connection = Some(connection);
                }
                Err(err) => {
                    log!(self.instance, "cannot track the mpv window: {err}");
                    self.unavailable = true;
                }
            }
        }
        let connection = self.connection.as_ref()?;
        let frame_extents_atom = self.frame_extents_atom;

        let result = (|| -> Result<WindowGeometry, Box<dyn std::error::Error>> {
            let geometry = connection.get_geometry(window)?.reply()?;
            let origin = connection
                .translate_coordinates(window, geometry.root, 0, 0)?
                .reply()?;
            // `--geometry` places the window's frame; the X server reports the client inside it.
            // The window manager publishes the frame's border widths: left, right, top, bottom.
            let (left, top) = if frame_extents_atom == 0 {
                (0, 0)
            } else {
                let extents = connection
                    .get_property(false, window, frame_extents_atom, AtomEnum::CARDINAL, 0, 4)?
                    .reply()?;
                let values: Vec<u32> = extents.value32().map(Iterator::collect).unwrap_or_default();
                match values.as_slice() {
                    [left, _right, top, _bottom] => (*left as i32, *top as i32),
                    _ => (0, 0),
                }
            };
            Ok(WindowGeometry {
                x: i32::from(origin.dst_x) - left,
                y: i32::from(origin.dst_y) - top,
                width: u32::from(geometry.width),
                height: u32::from(geometry.height),
            })
        })();
        match result {
            Ok(geometry) => Some(geometry),
            Err(_) => {
                // The window was just closed, or the server went away. Reconnect next time.
                if connection.flush().is_err() {
                    self.connection = None;
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_as_mpv_geometry() {
        let at = |x, y| WindowGeometry {
            x,
            y,
            width: 640,
            height: 360,
        };
        assert_eq!(at(1200, 40).to_string(), "640x360+1200+40");
        assert_eq!(at(-20, -5).to_string(), "640x360+0+0");
    }

    #[test]
    fn round_trips_through_saved_state() {
        let stored = StoredWindow::default();
        let geometry = WindowGeometry {
            x: 10,
            y: 20,
            width: 300,
            height: 200,
        };
        stored.remember(geometry);
        let text = stored.map(|value| serde_json::to_string(value).unwrap());
        let restored = StoredWindow::default();
        PersistentField::set(&restored, serde_json::from_str(&text).unwrap());
        assert_eq!(restored.get(), Some(geometry));
    }
}

/// Reads a real window's geometry: `SKOLL_TEST_WINDOW=<id> DISPLAY=:N cargo test -- --ignored`.
#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    #[ignore = "needs an X display and a window ID"]
    fn reads_a_live_window() {
        let id: u32 = std::env::var("SKOLL_TEST_WINDOW").unwrap().parse().unwrap();
        let geometry = WindowTracker::new(0).geometry(id).expect("no geometry");
        println!("{geometry:?} -> --geometry={geometry}");
        assert!(geometry.width > 0 && geometry.height > 0);
    }
}

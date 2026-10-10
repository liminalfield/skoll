//! The plugin's editor: the window the host opens for the plugin, with mpv drawing inside it.
//!
//! There is no toolkit and nothing to draw. Opening the editor hands the host's X11 window to
//! the background thread, which launches mpv with `--wid`; closing it takes the window back, and
//! the background thread quits mpv.

use std::any::Any;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use nih_plug::prelude::{Editor, GuiContext, ParentWindowHandle};

use crate::log;

/// The editor's size until it can be resized (milestone 8).
const SIZE: (u32, u32) = (640, 360);

/// The X11 window mpv should draw in, shared with the background thread. 0 while the editor is
/// closed.
#[derive(Default)]
pub struct EmbedTarget(AtomicU32);

impl EmbedTarget {
    pub fn window(&self) -> Option<u32> {
        match self.0.load(Ordering::Relaxed) {
            0 => None,
            window => Some(window),
        }
    }
}

pub struct SkollEditor {
    instance: u32,
    target: Arc<EmbedTarget>,
}

impl SkollEditor {
    pub fn new(instance: u32, target: Arc<EmbedTarget>) -> Self {
        Self { instance, target }
    }
}

/// Dropped by the host when it closes the editor.
struct OpenEditor {
    instance: u32,
    target: Arc<EmbedTarget>,
    window: u32,
}

impl Drop for OpenEditor {
    fn drop(&mut self) {
        log!(self.instance, "plugin window closed");
        // Only clear it if no newer window has replaced it.
        let _ =
            self.target
                .0
                .compare_exchange(self.window, 0, Ordering::Relaxed, Ordering::Relaxed);
    }
}

impl Editor for SkollEditor {
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        _context: Arc<dyn GuiContext>,
    ) -> Box<dyn Any + Send> {
        let window = match parent {
            ParentWindowHandle::X11Window(window) => {
                log!(self.instance, "plugin window opened (X11 window {window})");
                window
            }
            other => {
                log!(self.instance, "unsupported plugin window type: {other:?}");
                0
            }
        };
        self.target.0.store(window, Ordering::Relaxed);
        Box::new(OpenEditor {
            instance: self.instance,
            target: self.target.clone(),
            window,
        })
    }

    fn size(&self) -> (u32, u32) {
        SIZE
    }

    fn set_scale_factor(&self, _factor: f32) -> bool {
        // mpv scales the video to whatever size the window has.
        true
    }

    fn param_value_changed(&self, _id: &str, _normalized_value: f32) {}

    fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {}

    fn param_values_changed(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_clears_only_its_own_window() {
        let target = Arc::new(EmbedTarget::default());
        assert_eq!(target.window(), None);

        let first = OpenEditor {
            instance: 0,
            target: target.clone(),
            window: 11,
        };
        target.0.store(11, Ordering::Relaxed);
        // The host opens a new window before dropping the old handle.
        target.0.store(22, Ordering::Relaxed);
        drop(first);
        assert_eq!(target.window(), Some(22));
    }
}

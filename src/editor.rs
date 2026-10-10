//! The plugin's editor: the window the host opens for the plugin, with mpv drawing inside it.
//!
//! There is no toolkit and nothing to draw. Opening the editor hands the host's X11 window to
//! the background thread, which launches mpv with `--wid`; closing it takes the window back, and
//! the background thread quits mpv.

use std::any::Any;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use nih_plug::prelude::{Editor, GuiContext, ParentWindowHandle};

use crate::{log, SkollParams};

/// The editor's size before the user first resizes it.
pub const DEFAULT_SIZE: (u32, u32) = (640, 360);

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
    /// Holds the editor's size, which is saved with the project.
    params: Arc<SkollParams>,
}

impl SkollEditor {
    pub fn new(instance: u32, target: Arc<EmbedTarget>, params: Arc<SkollParams>) -> Self {
        Self {
            instance,
            target,
            params,
        }
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
        let size = *self
            .params
            .editor_size
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        log!(self.instance, "host asked for the window size: {}x{}", size.0, size.1);
        size
    }

    fn can_resize(&self) -> bool {
        true
    }

    /// The host resized the window. mpv follows the window by itself.
    fn set_size(&self, width: u32, height: u32) -> bool {
        log!(self.instance, "host set the window size: {width}x{height}");
        if width == 0 || height == 0 {
            return false;
        }
        *self
            .params
            .editor_size
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = (width, height);
        true
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
    use nih_plug::prelude::Params;

    #[test]
    fn remembers_the_size_the_host_sets() {
        let params = Arc::new(SkollParams::default());
        let editor = SkollEditor::new(0, Arc::new(EmbedTarget::default()), params.clone());
        assert!(editor.can_resize());
        assert_eq!(editor.size(), DEFAULT_SIZE);

        assert!(editor.set_size(1280, 720));
        assert_eq!(editor.size(), (1280, 720));
        assert!(!editor.set_size(0, 720));
        assert_eq!(editor.size(), (1280, 720));

        // The size is plugin state: it survives a save and restore.
        let saved = params.serialize_fields();
        let restored = Arc::new(SkollParams::default());
        restored.deserialize_fields(&saved);
        let editor = SkollEditor::new(0, Arc::new(EmbedTarget::default()), restored);
        assert_eq!(editor.size(), (1280, 720));
    }

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

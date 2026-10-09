//! Keeps the stored video path (plugin state, saved with the project) and mpv's open file in
//! step, as a pure state machine.
//!
//! Whichever side changed last wins: a file opened in mpv is stored, and a stored path that
//! changes, such as on project load, is loaded into mpv.

use std::sync::RwLock;

use nih_plug::params::persist::PersistentField;

use crate::log;

/// The stored video path: plugin state, saved with the project.
///
/// A plain `RwLock` would do, but this logs every save and restore, since the host decides when
/// to ask for the state and nothing else shows it.
pub struct StoredPath {
    instance: u32,
    pub path: RwLock<Option<String>>,
}

impl StoredPath {
    pub fn new(instance: u32) -> Self {
        Self {
            instance,
            path: RwLock::new(None),
        }
    }
}

impl PersistentField<'_, Option<String>> for StoredPath {
    /// Called when the host restores the plugin's state.
    fn set(&self, new_value: Option<String>) {
        log!(self.instance, "state restored, video: {new_value:?}");
        if let Ok(mut path) = self.path.write() {
            *path = new_value;
        }
    }

    /// Called when the host saves the plugin's state.
    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&Option<String>) -> R,
    {
        let path = self
            .path
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        log!(self.instance, "state saved, video: {:?}", *path);
        f(&path)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PathAction {
    /// mpv opened a new file: store its path.
    Store(String),
    /// Load the stored path into mpv, if the file exists.
    Load(String),
}

#[derive(Debug, Default)]
pub struct PathSync {
    last_mpv_path: Option<String>,
    last_stored: Option<String>,
    /// The path last sent with `loadfile` to this mpv, so it is sent once.
    requested: Option<String>,
}

impl PathSync {
    /// Forgets what this mpv was asked to load. Call after mpv relaunches.
    pub fn reset_mpv(&mut self) {
        self.last_mpv_path = None;
        self.requested = None;
    }

    pub fn tick(&mut self, stored: Option<&str>, mpv_path: Option<&str>) -> Option<PathAction> {
        if mpv_path != self.last_mpv_path.as_deref() {
            self.last_mpv_path = mpv_path.map(str::to_owned);
            // mpv going idle, for example after a relaunch, keeps the stored path.
            if let Some(path) = mpv_path {
                if stored != Some(path) {
                    self.last_stored = Some(path.to_owned());
                    self.requested = Some(path.to_owned());
                    return Some(PathAction::Store(path.to_owned()));
                }
            }
        } else if stored != self.last_stored.as_deref() {
            self.last_stored = stored.map(str::to_owned);
            self.requested = None;
        }

        let stored = stored?;
        if mpv_path != Some(stored) && self.requested.as_deref() != Some(stored) {
            self.requested = Some(stored.to_owned());
            return Some(PathAction::Load(stored.to_owned()));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(path: &str) -> Option<PathAction> {
        Some(PathAction::Store(path.to_owned()))
    }

    fn load(path: &str) -> Option<PathAction> {
        Some(PathAction::Load(path.to_owned()))
    }

    #[test]
    fn stores_a_file_opened_in_mpv() {
        let mut sync = PathSync::default();
        assert_eq!(sync.tick(None, None), None);
        assert_eq!(sync.tick(None, Some("/v/a.mkv")), store("/v/a.mkv"));
        // Next tick the stored path matches: nothing to do.
        assert_eq!(sync.tick(Some("/v/a.mkv"), Some("/v/a.mkv")), None);
        // Another file replaces it.
        assert_eq!(
            sync.tick(Some("/v/a.mkv"), Some("/v/b.mkv")),
            store("/v/b.mkv")
        );
    }

    #[test]
    fn loads_the_stored_path_on_project_load() {
        let mut sync = PathSync::default();
        assert_eq!(sync.tick(None, None), None);
        // The host restores the state after the plugin was created.
        assert_eq!(sync.tick(Some("/v/a.mkv"), None), load("/v/a.mkv"));
        // Sent once, even while mpv is still loading.
        assert_eq!(sync.tick(Some("/v/a.mkv"), None), None);
        assert_eq!(sync.tick(Some("/v/a.mkv"), Some("/v/a.mkv")), None);
    }

    #[test]
    fn a_restored_project_replaces_the_open_file() {
        let mut sync = PathSync::default();
        sync.tick(None, Some("/v/a.mkv"));
        sync.tick(Some("/v/a.mkv"), Some("/v/a.mkv"));
        // Loading a project with another video must not store mpv's old file over it.
        assert_eq!(
            sync.tick(Some("/v/b.mkv"), Some("/v/a.mkv")),
            load("/v/b.mkv")
        );
        assert_eq!(sync.tick(Some("/v/b.mkv"), Some("/v/a.mkv")), None);
        assert_eq!(sync.tick(Some("/v/b.mkv"), Some("/v/b.mkv")), None);
    }

    #[test]
    fn reloads_after_mpv_relaunches() {
        let mut sync = PathSync::default();
        sync.tick(None, Some("/v/a.mkv"));
        sync.tick(Some("/v/a.mkv"), Some("/v/a.mkv"));
        // The window was closed: the new mpv starts idle and gets the file again.
        sync.reset_mpv();
        assert_eq!(sync.tick(Some("/v/a.mkv"), None), load("/v/a.mkv"));
    }

    #[test]
    fn a_file_that_fails_to_load_is_not_retried() {
        let mut sync = PathSync::default();
        assert_eq!(
            sync.tick(Some("/v/broken.mkv"), None),
            load("/v/broken.mkv")
        );
        // mpv reports the path, then gives up and goes idle.
        assert_eq!(
            sync.tick(Some("/v/broken.mkv"), Some("/v/broken.mkv")),
            None
        );
        assert_eq!(sync.tick(Some("/v/broken.mkv"), None), None);
        assert_eq!(sync.tick(Some("/v/broken.mkv"), None), None);
    }
}

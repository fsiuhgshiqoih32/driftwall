//! The library (imported wallpapers) and the settings (what plays where, pause rules), stored
//! as JSON. The window writes both; the background process only reads them and reloads when
//! the window tells it to.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Video,
    Image,
}

/// How a wallpaper is sized to a display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    /// Cover the whole display, cropping what doesn't fit.
    #[default]
    Fill,
    /// Show all of it, with bars where the shapes differ.
    Fit,
    /// Cover the display exactly, distorting if the shapes differ.
    Stretch,
    /// Original size, centred.
    Center,
}

impl Fit {
    pub const ALL: [Fit; 4] = [Fit::Fill, Fit::Fit, Fit::Stretch, Fit::Center];

    pub fn label(self) -> &'static str {
        match self {
            Fit::Fill => "Fill",
            Fit::Fit => "Fit",
            Fit::Stretch => "Stretch",
            Fit::Center => "Center",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub kind: Kind,
    /// File name inside the media folder.
    pub file: String,
    /// Where it was imported from.
    pub original: String,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub fps: f32,
    pub codec: String,
    pub has_audio: bool,
    pub bytes: u64,
    /// Seconds since 1970.
    pub added: u64,
    pub fit: Fit,
    /// 0 = muted (audio is then not decoded at all), 100 = full.
    pub volume: u8,
    /// Playback speed, 0.25 to 2.
    pub speed: f32,
}

impl Item {
    pub fn path(&self) -> PathBuf {
        paths::media_dir().join(&self.file)
    }

    pub fn thumb_path(&self) -> PathBuf {
        paths::thumbs_dir().join(format!("{}.jpg", self.id))
    }

    pub fn speed(&self) -> f32 {
        if self.speed > 0.0 {
            self.speed.clamp(0.25, 2.0)
        } else {
            1.0
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    pub items: Vec<Item>,
}

impl Library {
    pub fn load() -> Self {
        load_json(&paths::library_file())
    }

    pub fn save(&self) -> std::io::Result<()> {
        save_json(&paths::library_file(), self)
    }

    pub fn get(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Item> {
        self.items.iter_mut().find(|i| i.id == id)
    }
}

/// What to do with a display's wallpaper while an app or game runs full screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullscreenRule {
    /// Pause every display (frees the GPU for games).
    #[default]
    PauseAll,
    /// Pause only the display the app covers.
    PauseDisplay,
    KeepPlaying,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Display id (Windows' monitor device path) -> library item id.
    pub displays: BTreeMap<String, String>,
    /// One wallpaper stretched across all displays; overrides `displays` while set.
    pub span: Option<String>,
    /// Pause a display's video while windows cover it completely.
    pub pause_when_covered: bool,
    pub fullscreen: FullscreenRule,
    pub pause_on_battery: bool,
    pub pause_on_battery_saver: bool,
    /// Run the background process at below-normal CPU priority.
    pub low_priority: bool,
    /// Give Windows a still frame of each video as the desktop picture, so the desktop looks
    /// right before Driftwall starts and after it quits.
    pub still_frame: bool,
    pub start_with_windows: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            displays: BTreeMap::new(),
            span: None,
            pause_when_covered: true,
            fullscreen: FullscreenRule::PauseAll,
            pause_on_battery: true,
            pause_on_battery_saver: true,
            low_priority: true,
            still_frame: true,
            start_with_windows: true,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        load_json(&paths::settings_file())
    }

    pub fn save(&self) -> std::io::Result<()> {
        save_json(&paths::settings_file(), self)
    }

    /// Whether any wallpaper is assigned at all.
    pub fn is_empty(&self) -> bool {
        self.displays.is_empty() && self.span.is_none()
    }

    /// Drops assignments to items that no longer exist.
    pub fn prune(&mut self, lib: &Library) {
        self.displays.retain(|_, id| lib.get(id).is_some());
        if self.span.as_ref().is_some_and(|id| lib.get(id).is_none()) {
            self.span = None;
        }
    }
}

/// The desktop pictures Driftwall replaced, so it can put them back. Written only by the
/// background process.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Display id -> the picture Windows showed before Driftwall set one.
    pub originals: BTreeMap<String, String>,
    /// Display id -> the rendered picture Driftwall set.
    pub applied: BTreeMap<String, String>,
    /// Windows' picture position before Driftwall changed it (DESKTOP_WALLPAPER_POSITION).
    pub original_position: Option<i32>,
    /// The spanning picture Driftwall set, if any (also the combined picture, see below).
    pub applied_span: Option<String>,
    /// Windows' single desktop picture before Driftwall set a combined one. Used on PCs where
    /// Windows can't address displays one by one (monitors without a device path).
    pub original_single: Option<String>,
}

impl State {
    pub fn load() -> Self {
        load_json(&paths::state_file())
    }

    pub fn save(&self) -> std::io::Result<()> {
        save_json(&paths::state_file(), self)
    }
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            log::warn!("{} is unreadable ({e}); using defaults", path.display());
            T::default()
        }),
        Err(_) => T::default(),
    }
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    paths::write_atomic(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_files_fill_in_defaults() {
        let s: Settings = serde_json::from_str(r#"{"pause_on_battery": false}"#).unwrap();
        assert!(!s.pause_on_battery);
        assert!(s.pause_when_covered);
        assert_eq!(s.fullscreen, FullscreenRule::PauseAll);
        let i: Item =
            serde_json::from_str(r#"{"id": "a", "kind": "image", "fit": "center"}"#).unwrap();
        assert_eq!((i.kind, i.fit, i.speed()), (Kind::Image, Fit::Center, 1.0));
    }

    #[test]
    fn prune_drops_missing_items() {
        let lib = Library {
            items: vec![Item {
                id: "a".into(),
                ..Default::default()
            }],
        };
        let mut s = Settings::default();
        s.displays.insert("d1".into(), "a".into());
        s.displays.insert("d2".into(), "gone".into());
        s.span = Some("gone".into());
        s.prune(&lib);
        assert_eq!(s.displays.len(), 1);
        assert!(s.span.is_none());
    }
}

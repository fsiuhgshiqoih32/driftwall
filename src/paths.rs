//! Where Driftwall keeps its files: `%LOCALAPPDATA%\Driftwall`, or `DRIFTWALL_HOME` when set
//! (tests and portable use).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        if let Some(h) = std::env::var_os("DRIFTWALL_HOME") {
            return PathBuf::from(h);
        }
        std::env::var_os("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("Driftwall"))
            .unwrap_or_else(|| PathBuf::from("Driftwall"))
    })
}

/// Imported videos and pictures (hard links or copies of the originals).
pub fn media_dir() -> PathBuf {
    home().join("media")
}

/// Library thumbnails.
pub fn thumbs_dir() -> PathBuf {
    home().join("thumbs")
}

/// Pictures rendered at each display's exact size, handed to Windows as the desktop picture.
pub fn rendered_dir() -> PathBuf {
    home().join("rendered")
}

pub fn library_file() -> PathBuf {
    home().join("library.json")
}

pub fn settings_file() -> PathBuf {
    home().join("settings.json")
}

/// Owned by the background process: the desktop pictures it replaced, to put them back.
pub fn state_file() -> PathBuf {
    home().join("state.json")
}

pub fn log_file() -> PathBuf {
    home().join("driftwall.log")
}

/// Writes `bytes` to `path` through a temporary file and a rename, so readers never see a
/// half-written file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

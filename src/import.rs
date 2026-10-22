//! Adding files to the library: check Windows can show them, put a hard link (or a copy, on
//! another drive) in the media folder, read their properties and make a thumbnail.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::model::{Fit, Item, Kind};
use crate::{paths, probe, wic};

pub const VIDEO_EXTS: &[&str] = &["mp4", "m4v", "mov", "mkv", "webm", "wmv", "avi"];
pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "bmp", "gif", "webp", "tif", "tiff", "heic", "heif", "avif", "jxr",
];

pub const THUMB_W: u32 = 384;
pub const THUMB_H: u32 = 216;

pub fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    if VIDEO_EXTS.contains(&ext.as_str()) {
        Some(Kind::Video)
    } else if IMAGE_EXTS.contains(&ext.as_str()) {
        Some(Kind::Image)
    } else {
        None
    }
}

/// A short unique id: time in milliseconds plus a counter, in base 36.
pub fn new_id() -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let n = N.fetch_add(1, Ordering::Relaxed) as u64 ^ (std::process::id() as u64 & 0xff) << 8;
    let mut v = ms.wrapping_mul(0x1_0000) ^ n;
    let mut s = String::new();
    while v > 0 {
        let d = (v % 36) as u8;
        s.insert(
            0,
            if d < 10 {
                (b'0' + d) as char
            } else {
                (b'a' + d - 10) as char
            },
        );
        v /= 36;
    }
    s
}

/// Hard link when on the same drive (instant, no extra space), copy otherwise.
fn link_or_copy(from: &Path, to: &Path) -> Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::hard_link(from, to).is_ok() {
        return Ok(());
    }
    let tmp = to.with_extension("part");
    std::fs::copy(from, &tmp).with_context(|| format!("copy {}", from.display()))?;
    std::fs::rename(&tmp, to)?;
    Ok(())
}

pub fn make_thumbnail(item: &Item) -> Result<()> {
    let bmp = match item.kind {
        Kind::Image => wic::render_file(&item.path(), Fit::Fill, THUMB_W, THUMB_H)?,
        Kind::Video => {
            // A frame a little way in: first frames are often black.
            let at = (item.duration_ms / 10).min(2000);
            let frame =
                probe::frame_at(&item.path(), at).or_else(|_| probe::frame_at(&item.path(), 0))?;
            let f = wic::factory()?;
            let src = wic::source_from_bgra(
                &f,
                frame.width,
                frame.height,
                frame.width * 4,
                &frame.pixels,
            )?;
            wic::render_source(&f, &src, Fit::Fill, THUMB_W, THUMB_H)?
        }
    };
    wic::save_jpeg(&bmp, &item.thumb_path())
}

/// Imports one file. COM must be initialised on this thread.
pub fn import(path: &Path) -> Result<Item> {
    let kind = kind_of(path).context("not a video or picture Driftwall knows")?;
    let meta = std::fs::metadata(path).with_context(|| format!("read {}", path.display()))?;
    if !meta.is_file() {
        bail!("not a file");
    }
    let mut item = Item {
        id: new_id(),
        title: path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into()),
        kind,
        original: path.to_string_lossy().into_owned(),
        bytes: meta.len(),
        added: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        fit: Fit::Fill,
        volume: 0,
        speed: 1.0,
        ..Default::default()
    };
    match kind {
        Kind::Video => {
            let info = probe::probe(path)?;
            item.width = info.width;
            item.height = info.height;
            item.duration_ms = info.duration_ms;
            item.fps = info.fps;
            item.codec = info.codec;
            item.has_audio = info.has_audio;
        }
        Kind::Image => {
            let (w, h) = wic::dimensions(path)?;
            item.width = w;
            item.height = h;
        }
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    item.file = format!("{}.{ext}", item.id);
    link_or_copy(path, &paths::media_dir().join(&item.file))?;
    if let Err(e) = make_thumbnail(&item) {
        log::warn!("thumbnail for {}: {e:#}", path.display());
    }
    Ok(item)
}

/// Deletes an item's files (the library entry is the caller's).
pub fn remove_files(item: &Item) {
    let _ = std::fs::remove_file(item.path());
    let _ = std::fs::remove_file(item.thumb_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_by_extension() {
        assert_eq!(kind_of(Path::new("a/B.MP4")), Some(Kind::Video));
        assert_eq!(kind_of(Path::new("x.webm")), Some(Kind::Video));
        assert_eq!(kind_of(Path::new("x.JPeG")), Some(Kind::Image));
        assert_eq!(kind_of(Path::new("x.txt")), None);
        assert_eq!(kind_of(Path::new("noext")), None);
    }

    #[test]
    fn ids_are_unique() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}

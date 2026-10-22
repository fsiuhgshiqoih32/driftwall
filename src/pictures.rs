//! Windows' own desktop picture, per display. Still wallpapers are shown this way (no running
//! renderer, so they cost nothing once set), and each video's first frame is set the same way
//! so the desktop already looks right before Driftwall starts and after it quits.
//!
//! Pictures are rendered at each display's exact pixel size and fit, so Windows never
//! rescales them and the global "picture position" setting doesn't matter. The pictures
//! Driftwall replaced are remembered in state.json and put back when a display is cleared.
//!
//! Where Windows can't address displays one by one (a monitor whose driver failed has no
//! device path), every display's picture is drawn into one image the size of the whole
//! desktop and set in "span" mode, with Windows' previous picture filling the other displays.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::Result;
use windows::Win32::Graphics::Gdi::{COLOR_DESKTOP, GetSysColor};
use windows::Win32::UI::Shell::{
    DWPOS_CENTER, DWPOS_FILL, DWPOS_FIT, DWPOS_SPAN, DWPOS_STRETCH, IDesktopWallpaper,
};

use crate::desktop::{self, Display};
use crate::layout::Rect;
use crate::model::{Fit, State};
use crate::{mf, paths, probe, wic};

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Image(PathBuf),
    /// The first frame of a video.
    VideoFrame(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    /// `None` = one picture across all displays.
    pub display: Option<String>,
    pub item: String,
    pub source: Source,
    pub fit: Fit,
    pub width: u32,
    pub height: u32,
}

impl Picture {
    pub fn file_name(&self) -> String {
        let kind = match self.source {
            Source::Image(_) => "img",
            Source::VideoFrame(_) => "frame",
        };
        format!(
            "{}-{}-{}x{}-{}.png",
            self.item,
            kind,
            self.width,
            self.height,
            self.fit.label().to_lowercase()
        )
    }
}

struct Job {
    pictures: Vec<Picture>,
    displays: Vec<Display>,
}

pub struct PictureWorker {
    tx: Sender<Job>,
}

impl PictureWorker {
    pub fn start() -> Self {
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("driftwall-pictures".into())
            .spawn(move || run(rx))
            .expect("spawn picture thread");
        Self { tx }
    }

    /// Makes these the pictures Driftwall shows (replacing the previous set).
    pub fn apply(&self, pictures: Vec<Picture>, displays: Vec<Display>) {
        let _ = self.tx.send(Job { pictures, displays });
    }
}

/// Puts back the pictures Driftwall replaced, on this thread (used by `--reset`).
pub fn restore_now() {
    let displays = desktop::displays().unwrap_or_default();
    if let Err(e) = apply(&[], &displays) {
        log::warn!("restoring desktop pictures: {e:#}");
    }
}

fn run(rx: Receiver<Job>) {
    mf::init_com_sta();
    let mut last: Option<Vec<Picture>> = None;
    while let Ok(mut job) = rx.recv() {
        // Only the newest request matters.
        for j in rx.try_iter() {
            job = j;
        }
        if last.as_ref() == Some(&job.pictures) {
            continue;
        }
        match apply(&job.pictures, &job.displays) {
            Ok(()) => {
                last = Some(job.pictures);
                // Decoding and scaling pictures leaves big buffers behind; an idle process
                // shouldn't keep them resident.
                if !crate::engine::VIDEOS_RUNNING.load(std::sync::atomic::Ordering::SeqCst) {
                    crate::engine::trim_memory();
                }
            }
            Err(e) => {
                log::warn!("desktop pictures: {e:#}");
                last = None;
            }
        }
    }
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn is_ours(path: &str) -> bool {
    let dir = paths::rendered_dir().to_string_lossy().to_lowercase();
    path.to_lowercase().starts_with(&dir)
}

fn render(p: &Picture, out: &Path) -> Result<()> {
    if out.exists() {
        return Ok(());
    }
    let bmp = match &p.source {
        Source::Image(path) => wic::render_file(path, p.fit, p.width, p.height)?,
        Source::VideoFrame(path) => {
            let frame = probe::frame_at(path, 0)?;
            let f = wic::factory()?;
            let src = wic::source_from_bgra(
                &f,
                frame.width,
                frame.height,
                frame.width * 4,
                &frame.pixels,
            )?;
            wic::render_source(&f, &src, p.fit, p.width, p.height)?
        }
    };
    wic::save_png(&bmp, out)?;
    log::info!("rendered {}", out.display());
    Ok(())
}

fn remember_original(api: &IDesktopWallpaper, state: &mut State, display: &str, wid: &str) {
    if state.originals.contains_key(display) {
        return;
    }
    let current = desktop::get_wallpaper(api, Some(wid)).unwrap_or_default();
    if !is_ours(&current) {
        state.originals.insert(display.to_string(), current);
    }
}

fn restore(api: &IDesktopWallpaper, state: &mut State, display: &str, wid: &str) {
    if let Some(orig) = state.originals.remove(display)
        && (orig.is_empty() || Path::new(&orig).exists())
        && let Err(e) = desktop::set_wallpaper(api, Some(wid), &orig)
    {
        log::warn!("{e:#}");
    }
}

fn apply(pictures: &[Picture], displays: &[Display]) -> Result<()> {
    let api = desktop::wallpaper_api()?;
    let mut state = State::load();
    let per_display = !displays.is_empty() && displays.iter().all(|d| d.wallpaper_id.is_some());
    if !per_display {
        let keep = apply_combined(&api, &mut state, pictures, displays)?;
        state.save()?;
        clean_rendered(&state, &keep);
        return Ok(());
    }
    // Windows' ids for our display ids.
    let wid = |id: &str| -> String {
        displays
            .iter()
            .find(|d| d.id == id)
            .and_then(|d| d.wallpaper_id.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let dir = paths::rendered_dir();
    let mut keep: Vec<PathBuf> = Vec::new();
    for p in pictures {
        let out = dir.join(p.file_name());
        if let Err(e) = render(p, &out) {
            log::warn!("could not render the picture for {}: {e:#}", p.item);
            continue;
        }
        keep.push(out);
    }
    let rendered = |p: &Picture| {
        let out = dir.join(p.file_name());
        out.exists().then(|| out.to_string_lossy().into_owned())
    };

    if let Some(span) = pictures.iter().find(|p| p.display.is_none()) {
        if let Some(path) = rendered(span) {
            let current = desktop::get_wallpaper(&api, None).unwrap_or_default();
            if state.applied_span.as_deref() != Some(path.as_str()) || !same(&current, &path) {
                if state.original_position.is_none() {
                    state.original_position = desktop::get_position(&api);
                }
                for d in displays {
                    remember_original(&api, &mut state, &d.id, &wid(&d.id));
                }
                desktop::set_wallpaper(&api, None, &path)?;
                desktop::set_position(&api, DWPOS_SPAN.0)?;
            }
            state.applied_span = Some(path);
            state.applied.clear();
        }
    } else {
        if let Some(span) = state.applied_span.take() {
            let current = desktop::get_wallpaper(&api, None).unwrap_or_default();
            if same(&current, &span) {
                if let Some(pos) = state.original_position.take() {
                    let _ = desktop::set_position(&api, pos);
                }
                if let Some(single) = state.original_single.take() {
                    let _ = desktop::set_wallpaper(&api, None, &single);
                }
                let ids: Vec<String> = state.originals.keys().cloned().collect();
                for id in ids {
                    restore(&api, &mut state, &id, &wid(&id));
                }
            }
            state.originals.clear();
            state.original_single = None;
        }
        let wanted: Vec<(&str, String)> = pictures
            .iter()
            .filter_map(|p| Some((p.display.as_deref()?, rendered(p)?)))
            .collect();
        if !wanted.is_empty() && desktop::get_position(&api) == Some(DWPOS_SPAN.0) {
            if state.original_position.is_none() {
                state.original_position = Some(DWPOS_SPAN.0);
            }
            desktop::set_position(&api, DWPOS_FILL.0)?;
        }
        for (display, path) in &wanted {
            let w = wid(display);
            let current = desktop::get_wallpaper(&api, Some(&w)).unwrap_or_default();
            if state.applied.get(*display).is_some_and(|a| same(a, path)) && same(&current, path) {
                continue;
            }
            remember_original(&api, &mut state, display, &w);
            desktop::set_wallpaper(&api, Some(&w), path)?;
            state.applied.insert(display.to_string(), path.clone());
        }
        let stale: Vec<String> = state
            .applied
            .keys()
            .filter(|d| !wanted.iter().any(|(w, _)| w == d))
            .cloned()
            .collect();
        for display in stale {
            let applied = state.applied.remove(&display).unwrap_or_default();
            let w = wid(&display);
            let current = desktop::get_wallpaper(&api, Some(&w)).unwrap_or_default();
            if same(&current, &applied) {
                restore(&api, &mut state, &display, &w);
            } else {
                // Someone changed the picture since; leave theirs alone.
                state.originals.remove(&display);
            }
        }
        if state.applied.is_empty()
            && let Some(pos) = state.original_position.take()
        {
            let _ = desktop::set_position(&api, pos);
        }
    }
    state.save()?;
    clean_rendered(&state, &keep);
    Ok(())
}

/// Deletes rendered pictures nothing uses any more.
fn clean_rendered(state: &State, keep: &[PathBuf]) {
    let Ok(entries) = std::fs::read_dir(paths::rendered_dir()) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = p.to_string_lossy();
        let used = keep.iter().any(|k| k == &p)
            || state.applied.values().any(|a| same(a, &name))
            || state
                .applied_span
                .as_deref()
                .is_some_and(|a| same(a, &name));
        if !used {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Windows' picture position as a fit mode (tile is drawn centred).
fn fit_of_position(pos: Option<i32>) -> Fit {
    match pos {
        Some(p) if p == DWPOS_CENTER.0 || p == 1 => Fit::Center,
        Some(p) if p == DWPOS_STRETCH.0 => Fit::Stretch,
        Some(p) if p == DWPOS_FIT.0 => Fit::Fit,
        _ => Fit::Fill,
    }
}

fn desktop_color() -> [u8; 3] {
    let c = unsafe { GetSysColor(COLOR_DESKTOP) };
    [
        (c & 0xff) as u8,
        ((c >> 8) & 0xff) as u8,
        ((c >> 16) & 0xff) as u8,
    ]
}

/// Copies `src` into `dst` with its top-left at (x, y), clipped.
fn blit(dst: &mut wic::Bitmap, src: &wic::Bitmap, x: i32, y: i32) {
    for sy in 0..src.height as i32 {
        let dy = y + sy;
        if dy < 0 || dy >= dst.height as i32 {
            continue;
        }
        let x0 = x.max(0);
        let x1 = (x + src.width as i32).min(dst.width as i32);
        if x1 <= x0 {
            continue;
        }
        let s = ((sy * src.width as i32 + (x0 - x)) * 4) as usize;
        let d = ((dy * dst.width as i32 + x0) * 4) as usize;
        let n = ((x1 - x0) * 4) as usize;
        dst.pixels[d..d + n].copy_from_slice(&src.pixels[s..s + n]);
    }
}

fn hash(parts: &[String]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain([0]) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// The single-picture fallback: one desktop-sized image in span mode.
fn apply_combined(
    api: &IDesktopWallpaper,
    state: &mut State,
    pictures: &[Picture],
    displays: &[Display],
) -> Result<Vec<PathBuf>> {
    let current = desktop::get_wallpaper(api, None).unwrap_or_default();
    if pictures.is_empty() {
        if let Some(applied) = state.applied_span.take()
            && same(&current, &applied)
        {
            let orig = state.original_single.take().unwrap_or_default();
            if orig.is_empty() || Path::new(&orig).exists() {
                desktop::set_wallpaper(api, None, &orig)?;
            }
            desktop::set_position(api, state.original_position.take().unwrap_or(DWPOS_FILL.0))?;
        }
        state.original_single = None;
        state.original_position = None;
        return Ok(Vec::new());
    }
    if state.applied_span.is_none() {
        state.original_single = Some(current.clone()).filter(|c| !is_ours(c));
        state.original_position = desktop::get_position(api);
    }
    let bounds = desktop::bounds(displays);
    let dir = paths::rendered_dir();
    let out = if let Some(span) = pictures.iter().find(|p| p.display.is_none()) {
        let out = dir.join(span.file_name());
        render(span, &out)?;
        out
    } else {
        let original = state.original_single.clone().unwrap_or_default();
        let original_fit = fit_of_position(state.original_position);
        let mut key: Vec<String> = pictures
            .iter()
            .map(|p| format!("{}@{:?}", p.file_name(), p.display))
            .collect();
        key.push(original.clone());
        key.push(format!("{:?}", state.original_position));
        key.extend(displays.iter().map(|d| format!("{}={:?}", d.id, d.rect)));
        let out = dir.join(format!("combined-{:016x}.png", hash(&key)));
        if !out.exists() {
            let (w, h) = (bounds.width().max(1) as u32, bounds.height().max(1) as u32);
            // Base: the previous picture if it spanned, else the desktop colour.
            let spanned =
                state.original_position == Some(DWPOS_SPAN.0) && Path::new(&original).exists();
            let mut canvas = if spanned {
                wic::render_file(Path::new(&original), Fit::Fill, w, h)?
            } else {
                let [r, g, b] = desktop_color();
                let mut c = wic::Bitmap::black(w, h);
                for px in c.pixels.as_chunks_mut::<4>().0.iter_mut() {
                    px.copy_from_slice(&[b, g, r, 255]);
                }
                c
            };
            for d in displays {
                let at: Rect = d.rect.offset(-bounds.left, -bounds.top);
                let (dw, dh) = (d.rect.width() as u32, d.rect.height() as u32);
                let picture = pictures
                    .iter()
                    .find(|p| p.display.as_deref() == Some(d.id.as_str()));
                let bmp = match picture {
                    Some(p) => {
                        let tmp = dir.join(p.file_name());
                        render(p, &tmp)?;
                        wic::render_file(&tmp, Fit::Stretch, dw, dh)?
                    }
                    // The previous picture, the way Windows drew it on each display.
                    None if !spanned && Path::new(&original).exists() => {
                        wic::render_file(Path::new(&original), original_fit, dw, dh)?
                    }
                    None => continue,
                };
                blit(&mut canvas, &bmp, at.left, at.top);
            }
            wic::save_png(&canvas, &out)?;
            log::info!("rendered combined picture {}", out.display());
        }
        out
    };
    let path = out.to_string_lossy().into_owned();
    if state.applied_span.as_deref() != Some(path.as_str()) || !same(&current, &path) {
        desktop::set_wallpaper(api, None, &path)?;
        desktop::set_position(api, DWPOS_SPAN.0)?;
    }
    state.applied_span = Some(path);
    let mut keep = vec![out];
    keep.extend(pictures.iter().map(|p| dir.join(p.file_name())));
    Ok(keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_clips() {
        let mut dst = wic::Bitmap::black(4, 4);
        let mut src = wic::Bitmap::black(3, 3);
        for px in src.pixels.as_chunks_mut::<4>().0.iter_mut() {
            px[0] = 200;
        }
        blit(&mut dst, &src, 2, -1);
        assert_eq!(dst.pixel(2, 0)[0], 200);
        assert_eq!(dst.pixel(3, 1)[0], 200);
        assert_eq!(dst.pixel(1, 0)[0], 0);
        assert_eq!(dst.pixel(2, 2)[0], 0);
    }

    #[test]
    fn positions_map_to_fits() {
        assert_eq!(fit_of_position(Some(DWPOS_FIT.0)), Fit::Fit);
        assert_eq!(fit_of_position(Some(DWPOS_FILL.0)), Fit::Fill);
        assert_eq!(fit_of_position(Some(1)), Fit::Center);
        assert_eq!(fit_of_position(None), Fit::Fill);
    }
}

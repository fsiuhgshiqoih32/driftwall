//! Turns the settings into running wallpapers: a player per video (shared by every display
//! showing it), a window per display inside the desktop, and Windows desktop pictures for
//! stills. Re-runs whenever settings, displays or Explorer change, touching only what
//! differs, and pauses players according to the pause rules.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::desktop::{self, Display, Host};
use crate::gpu::{self, Gpu};
use crate::host::{self, SurfaceHost};
use crate::layout::Rect;
use crate::model::{FullscreenRule, Kind, Library, Settings};
use crate::pictures::{Picture, PictureWorker, Source};
use crate::player::{Player, PlayerConfig, Target};
use crate::watch::{self, Snapshot};

/// Output id used for a wallpaper spanning all displays.
pub const SPAN: &str = "span";

/// Whether any video player exists; the picture thread only trims memory when none does.
pub static VIDEOS_RUNNING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Hands the pages of an idle process back to Windows. Only called with no player running:
/// nothing then touches most of them, and the few that are needed fault back cheaply.
pub fn trim_memory() {
    use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}

/// A video paused this long is shut down completely (decoder and GPU memory released) and
/// started again from its first frame when it can be seen. The still frame Windows shows
/// underneath is that same first frame, so the restart doesn't show.
pub const SUSPEND_AFTER: Duration = Duration::from_secs(15);

fn output_on(active: &[bool], displays: &[Display], id: &str) -> bool {
    if id == SPAN {
        active.iter().any(|a| *a)
    } else {
        displays
            .iter()
            .position(|d| d.id == id)
            .is_some_and(|i| active.get(i).copied().unwrap_or(false))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoPlan {
    pub item: String,
    pub config: PlayerConfig,
    /// (display id or SPAN, screen rectangle)
    pub outputs: Vec<(String, Rect)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub videos: Vec<VideoPlan>,
    pub pictures: Vec<Picture>,
}

/// What should be on screen. Pure, so it can be tested without a desktop.
pub fn plan(settings: &Settings, lib: &Library, displays: &[Display]) -> Plan {
    let mut out = Plan::default();
    let config = |item: &crate::model::Item| PlayerConfig {
        file: item.path(),
        fit: item.fit,
        volume: if item.has_audio {
            item.volume.min(100) as f32 / 100.0
        } else {
            0.0
        },
        speed: item.speed(),
        fps: item.fps,
    };
    if let Some(item) = settings.span.as_deref().and_then(|id| lib.get(id)) {
        let bounds = desktop::bounds(displays);
        if bounds.is_empty() {
            return out;
        }
        let source = match item.kind {
            Kind::Image => Some(Source::Image(item.path())),
            Kind::Video => {
                out.videos.push(VideoPlan {
                    item: item.id.clone(),
                    config: config(item),
                    outputs: vec![(SPAN.to_string(), bounds)],
                });
                settings
                    .still_frame
                    .then(|| Source::VideoFrame(item.path()))
            }
        };
        if let Some(source) = source {
            out.pictures.push(Picture {
                display: None,
                item: item.id.clone(),
                source,
                fit: item.fit,
                width: bounds.width() as u32,
                height: bounds.height() as u32,
            });
        }
        return out;
    }
    for d in displays {
        let Some(item) = settings.displays.get(&d.id).and_then(|id| lib.get(id)) else {
            continue;
        };
        let source = match item.kind {
            Kind::Image => Some(Source::Image(item.path())),
            Kind::Video => {
                match out.videos.iter_mut().find(|v| v.item == item.id) {
                    Some(v) => v.outputs.push((d.id.clone(), d.rect)),
                    None => out.videos.push(VideoPlan {
                        item: item.id.clone(),
                        config: config(item),
                        outputs: vec![(d.id.clone(), d.rect)],
                    }),
                }
                settings
                    .still_frame
                    .then(|| Source::VideoFrame(item.path()))
            }
        };
        if let Some(source) = source {
            out.pictures.push(Picture {
                display: Some(d.id.clone()),
                item: item.id.clone(),
                source,
                fit: item.fit,
                width: d.rect.width() as u32,
                height: d.rect.height() as u32,
            });
        }
    }
    out
}

/// Things that pause everything, besides the per-display rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Conditions {
    pub user_paused: bool,
    pub locked: bool,
    pub display_off: bool,
}

/// Which displays should be drawing right now (same order as `displays`).
pub fn active(settings: &Settings, snap: &Snapshot, cond: Conditions, n: usize) -> Vec<bool> {
    let all_off = cond.user_paused
        || cond.locked
        || cond.display_off
        || (settings.pause_on_battery && snap.on_battery)
        || (settings.pause_on_battery_saver && snap.battery_saver)
        || (settings.fullscreen == FullscreenRule::PauseAll && snap.fullscreen.is_some());
    (0..n)
        .map(|i| {
            !all_off
                && !(settings.pause_when_covered && snap.covered.get(i).copied().unwrap_or(false))
                && !(settings.fullscreen == FullscreenRule::PauseDisplay
                    && snap.fullscreen == Some(i))
        })
        .collect()
}

struct Output {
    display: String,
    /// Screen rectangle.
    rect: Rect,
    hwnd: isize,
    size: (u32, u32),
}

struct Running {
    item: String,
    config: PlayerConfig,
    outputs: Vec<Output>,
    player: Player,
    gpu: Arc<Gpu>,
    /// When every output last went inactive.
    inactive_since: Option<Instant>,
}

impl Running {
    fn targets(&self) -> Vec<Target> {
        self.outputs
            .iter()
            .map(|o| Target {
                hwnd: o.hwnd,
                width: o.size.0,
                height: o.size.1,
                display: o.display.clone(),
            })
            .collect()
    }
}

pub struct Engine {
    pub settings: Settings,
    pub library: Library,
    pub displays: Vec<Display>,
    pub cond: Conditions,
    host: Option<Host>,
    windows: SurfaceHost,
    running: Vec<Running>,
    pictures: PictureWorker,
    snapshot: Snapshot,
    last_active: Vec<bool>,
    /// Item id -> when to try again after it failed to play.
    retry: HashMap<String, Instant>,
    /// Item id -> the outputs of a video shut down after a long pause.
    suspended: HashMap<String, Vec<String>>,
    /// Memory was trimmed since the last player stopped.
    trimmed: bool,
}

impl Engine {
    pub fn new() -> Result<Self> {
        Ok(Self {
            settings: Settings::load(),
            library: Library::load(),
            displays: Vec::new(),
            cond: Conditions::default(),
            host: None,
            windows: SurfaceHost::start()?,
            running: Vec::new(),
            pictures: PictureWorker::start(),
            snapshot: Snapshot::default(),
            last_active: Vec::new(),
            retry: HashMap::new(),
            suspended: HashMap::new(),
            trimmed: false,
        })
    }

    /// Whether any video is playing, paused or suspended (the tick then keeps running).
    pub fn has_videos(&self) -> bool {
        !self.running.is_empty() || !self.suspended.is_empty()
    }

    pub fn has_pending_retries(&self) -> bool {
        !self.retry.is_empty()
    }

    /// Re-reads settings and library from disk and applies them.
    pub fn reload(&mut self) {
        self.settings = Settings::load();
        self.library = Library::load();
        self.retry.clear();
        self.suspended.clear();
        self.refresh_displays();
        self.apply();
    }

    pub fn refresh_displays(&mut self) {
        match desktop::displays() {
            Ok(d) if !d.is_empty() => self.displays = d,
            Ok(_) => log::warn!("no displays reported"),
            Err(e) => log::warn!("listing displays: {e:#}"),
        }
    }

    fn stop_all(&mut self) {
        self.suspended.clear();
        for r in self.running.drain(..) {
            r.player.stop();
            for o in &r.outputs {
                self.windows.destroy(o.hwnd);
            }
        }
        self.last_active.clear();
    }

    /// Throws away every window and player and builds them again (Explorer restarted, the
    /// desktop window was replaced, or the GPU was reset).
    pub fn rebuild(&mut self) {
        log::info!("rebuilding wallpapers");
        self.stop_all();
        self.host = None;
        gpu::reset_pool();
        self.refresh_displays();
        self.apply();
    }

    fn ensure_host(&mut self) -> Option<Host> {
        if let Some(h) = self.host
            && h.alive()
        {
            return Some(h);
        }
        if self.host.is_some() {
            // The old desktop window is gone, and our windows with it.
            self.stop_all();
        }
        match desktop::find_host() {
            Ok(h) => {
                log::info!("desktop window {:#x} ({:?})", h.hwnd, h.kind);
                self.host = Some(h);
                Some(h)
            }
            Err(e) => {
                log::warn!("{e:#}");
                self.host = None;
                None
            }
        }
    }

    /// Brings players, windows and desktop pictures in line with the settings.
    pub fn apply(&mut self) {
        crate::daemon::set_priority(self.settings.low_priority);
        let plan = plan(&self.settings, &self.library, &self.displays);
        self.pictures
            .apply(plan.pictures.clone(), self.displays.clone());

        let now = Instant::now();
        self.retry.retain(|_, at| *at > now);
        // Suspended videos stay off until one of their displays can show them.
        let active = active(
            &self.settings,
            &self.snapshot,
            self.cond,
            self.displays.len(),
        );
        self.suspended.retain(|item, outputs| {
            plan.videos.iter().any(|v| &v.item == item)
                && !outputs
                    .iter()
                    .any(|o| output_on(&active, &self.displays, o))
        });
        let videos: Vec<VideoPlan> = plan
            .videos
            .into_iter()
            .filter(|v| !self.retry.contains_key(&v.item) && !self.suspended.contains_key(&v.item))
            .collect();

        // Stop players that are no longer wanted, or whose file or audio on/off changed.
        let mut keep = Vec::new();
        for r in self.running.drain(..) {
            let wanted = videos.iter().find(|v| v.item == r.item).is_some_and(|v| {
                v.config.file == r.config.file && (v.config.volume > 0.0) == (r.config.volume > 0.0)
            });
            if wanted {
                keep.push(r);
            } else {
                r.player.stop();
                for o in &r.outputs {
                    self.windows.destroy(o.hwnd);
                }
            }
        }
        self.running = keep;
        if videos.is_empty() {
            self.last_active.clear();
            return;
        }
        if !crate::mf::available() {
            log::error!("{}", crate::mf::MISSING);
            return;
        }
        let Some(host) = self.ensure_host() else {
            // Explorer isn't up yet (Driftwall started first at sign-in): try again shortly.
            for v in &videos {
                self.retry
                    .insert(v.item.clone(), Instant::now() + Duration::from_secs(5));
            }
            return;
        };

        for v in videos {
            if let Some(i) = self.running.iter().position(|r| r.item == v.item) {
                self.update_running(i, &host, &v);
            } else if let Err(e) = self.start_running(&host, &v) {
                log::warn!("could not start {}: {e:#}", v.item);
                self.retry
                    .insert(v.item.clone(), Instant::now() + Duration::from_secs(60));
            }
        }
        self.last_active.clear();
        self.update_activity();
        VIDEOS_RUNNING.store(
            !self.running.is_empty(),
            std::sync::atomic::Ordering::SeqCst,
        );
        if !self.running.is_empty() {
            self.trimmed = false;
        }
    }

    fn output_for(&self, host: &Host, display: &str, rect: Rect) -> Result<Output> {
        let client = host.to_client(&rect);
        let hwnd = self.windows.create(host.hwnd, client)?;
        Ok(Output {
            display: display.to_string(),
            rect,
            hwnd,
            size: (rect.width() as u32, rect.height() as u32),
        })
    }

    fn start_running(&mut self, host: &Host, v: &VideoPlan) -> Result<()> {
        let first = v.outputs.first().map(|(d, _)| d.as_str()).unwrap_or("");
        let monitor = self
            .displays
            .iter()
            .find(|d| d.id == first)
            .or_else(|| self.displays.iter().find(|d| d.primary))
            .map_or(0, |d| d.hmonitor);
        let gpu = gpu::for_monitor(monitor)?;
        let mut outputs = Vec::new();
        for (display, rect) in &v.outputs {
            match self.output_for(host, display, *rect) {
                Ok(o) => outputs.push(o),
                Err(e) => {
                    for o in &outputs {
                        self.windows.destroy(o.hwnd);
                    }
                    return Err(e);
                }
            }
        }
        let targets = outputs.iter().map(|o| Target {
            hwnd: o.hwnd,
            width: o.size.0,
            height: o.size.1,
            display: o.display.clone(),
        });
        let player = Player::start(gpu.clone(), v.config.clone(), targets.collect());
        self.running.push(Running {
            item: v.item.clone(),
            config: v.config.clone(),
            outputs,
            player,
            gpu,
            inactive_since: None,
        });
        Ok(())
    }

    fn update_running(&mut self, i: usize, host: &Host, v: &VideoPlan) {
        let mut changed = false;
        // Outputs: drop the ones no longer wanted or moved, add new ones.
        let mut old = std::mem::take(&mut self.running[i].outputs);
        old.retain(|o| {
            let keep = v
                .outputs
                .iter()
                .any(|(d, r)| *d == o.display && *r == o.rect)
                && host::alive(o.hwnd);
            if !keep {
                self.windows.destroy(o.hwnd);
                changed = true;
            }
            keep
        });
        for (display, rect) in &v.outputs {
            if old.iter().any(|o| o.display == *display) {
                continue;
            }
            match self.output_for(host, display, *rect) {
                Ok(o) => {
                    old.push(o);
                    changed = true;
                }
                Err(e) => log::warn!("{e:#}"),
            }
        }
        let r = &mut self.running[i];
        r.outputs = old;
        if changed {
            r.player.set_targets(r.targets());
        }
        if r.config.fit != v.config.fit {
            r.player.set_fit(v.config.fit);
        }
        if r.config.volume != v.config.volume {
            r.player.set_volume(v.config.volume);
        }
        if r.config.speed != v.config.speed {
            r.player.set_speed(v.config.speed);
        }
        r.config = v.config.clone();
    }

    fn update_activity(&mut self) {
        let active = active(
            &self.settings,
            &self.snapshot,
            self.cond,
            self.displays.len(),
        );
        let now = Instant::now();
        let changed = active != self.last_active;
        for r in &mut self.running {
            let ids: Vec<String> = r
                .outputs
                .iter()
                .filter(|o| output_on(&active, &self.displays, &o.display))
                .map(|o| o.display.clone())
                .collect();
            if ids.is_empty() {
                r.inactive_since.get_or_insert(now);
            } else {
                r.inactive_since = None;
            }
            if changed {
                r.player.set_active(ids);
            }
        }
        if changed {
            log::debug!("active displays: {active:?}");
            self.last_active = active;
        }
    }

    /// Shuts down videos that have been paused for a while, and brings suspended ones back
    /// once they can be seen. Only with still frames on: without one, Windows' own picture
    /// would flash while a video starts again.
    fn suspend_or_resume(&mut self) {
        if !self.settings.still_frame {
            return;
        }
        let mut i = 0;
        while i < self.running.len() {
            if self.running[i]
                .inactive_since
                .is_some_and(|t| t.elapsed() >= SUSPEND_AFTER)
            {
                let r = self.running.remove(i);
                log::info!(
                    "suspending {} (paused for {} s)",
                    r.item,
                    SUSPEND_AFTER.as_secs()
                );
                r.player.stop();
                for o in &r.outputs {
                    self.windows.destroy(o.hwnd);
                }
                self.suspended.insert(
                    r.item.clone(),
                    r.outputs.iter().map(|o| o.display.clone()).collect(),
                );
            } else {
                i += 1;
            }
        }
        let active = active(
            &self.settings,
            &self.snapshot,
            self.cond,
            self.displays.len(),
        );
        VIDEOS_RUNNING.store(
            !self.running.is_empty(),
            std::sync::atomic::Ordering::SeqCst,
        );
        if self.running.is_empty() && !self.suspended.is_empty() && !self.trimmed {
            trim_memory();
            self.trimmed = true;
        }
        let wake = self
            .suspended
            .values()
            .any(|outs| outs.iter().any(|o| output_on(&active, &self.displays, o)));
        if wake {
            log::info!("resuming suspended video");
            self.apply();
        }
    }

    /// Periodic work while videos play: pause rules, failed players, vanished windows.
    pub fn tick(&mut self) {
        if self.has_videos() {
            self.snapshot = watch::scan(&self.displays);
        }
        // Players that stopped on an error.
        let mut device_lost = false;
        let mut failed = Vec::new();
        for (i, r) in self.running.iter().enumerate() {
            if let Some(err) = r.player.failure() {
                log::warn!("{} stopped: {err}", r.item);
                device_lost |= err.contains("device lost");
                failed.push(i);
            }
        }
        if device_lost {
            log::warn!(
                "graphics device lost (adapter {}); restarting",
                self.running[failed[0]].gpu.adapter_name
            );
            self.rebuild();
            return;
        }
        for i in failed.into_iter().rev() {
            let r = self.running.remove(i);
            for o in &r.outputs {
                self.windows.destroy(o.hwnd);
            }
            self.retry
                .insert(r.item.clone(), Instant::now() + Duration::from_secs(60));
            r.player.stop();
        }
        // Explorer replaced the desktop window (restart, some wallpaper changes).
        let host_dead = self.host.is_some_and(|h| !h.alive());
        let window_dead = self
            .running
            .iter()
            .any(|r| r.outputs.iter().any(|o| !host::alive(o.hwnd)));
        if host_dead || window_dead {
            self.rebuild();
            return;
        }
        if self.retry.values().any(|at| *at <= Instant::now()) {
            self.apply();
            return;
        }
        self.update_activity();
        self.suspend_or_resume();
    }

    pub fn shutdown(&mut self) {
        self.stop_all();
    }

    /// Paths of everything that's playing (for the log and the self-test).
    pub fn playing(&self) -> Vec<PathBuf> {
        self.running.iter().map(|r| r.config.file.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Fit, Item};

    fn display(id: &str, l: i32, w: i32, h: i32) -> Display {
        Display {
            id: id.into(),
            wallpaper_id: Some(id.into()),
            name: id.into(),
            rect: Rect::new(l, 0, l + w, h),
            work: Rect::new(l, 0, l + w, h - 48),
            primary: l == 0,
            hmonitor: 0,
        }
    }

    fn lib() -> Library {
        let item = |id: &str, kind| Item {
            id: id.into(),
            kind,
            file: format!("{id}.x"),
            has_audio: true,
            volume: 30,
            ..Default::default()
        };
        Library {
            items: vec![
                item("v", Kind::Video),
                item("i", Kind::Image),
                item("w", Kind::Video),
            ],
        }
    }

    #[test]
    fn same_video_on_two_displays_shares_one_player() {
        let displays = [display("a", 0, 2560, 1440), display("b", -1600, 1600, 900)];
        let mut s = Settings::default();
        s.displays.insert("a".into(), "v".into());
        s.displays.insert("b".into(), "v".into());
        let p = plan(&s, &lib(), &displays);
        assert_eq!(p.videos.len(), 1);
        assert_eq!(p.videos[0].outputs.len(), 2);
        assert!((p.videos[0].config.volume - 0.3).abs() < 1e-6);
        // Still frames for both, at each display's size.
        assert_eq!(p.pictures.len(), 2);
        assert_eq!((p.pictures[1].width, p.pictures[1].height), (1600, 900));
    }

    #[test]
    fn pictures_and_videos_mix() {
        let displays = [display("a", 0, 1920, 1080), display("b", 1920, 1920, 1080)];
        let mut s = Settings {
            still_frame: false,
            ..Default::default()
        };
        s.displays.insert("a".into(), "i".into());
        s.displays.insert("b".into(), "w".into());
        s.displays.insert("gone".into(), "v".into());
        let p = plan(&s, &lib(), &displays);
        assert_eq!(p.videos.len(), 1);
        assert_eq!(p.videos[0].item, "w");
        assert_eq!(p.pictures.len(), 1);
        assert_eq!(
            p.pictures[0].source,
            Source::Image(lib().get("i").unwrap().path())
        );
    }

    #[test]
    fn span_uses_the_bounds() {
        let displays = [display("a", 0, 1920, 1080), display("b", -1280, 1280, 1024)];
        let s = Settings {
            span: Some("v".into()),
            ..Default::default()
        };
        let p = plan(&s, &lib(), &displays);
        assert_eq!(
            p.videos[0].outputs,
            vec![(SPAN.to_string(), Rect::new(-1280, 0, 1920, 1080))]
        );
        assert_eq!(p.pictures[0].display, None);
        assert_eq!(p.pictures[0].fit, Fit::Fill);
    }

    #[test]
    fn pause_rules() {
        let s = Settings::default();
        let snap = Snapshot {
            covered: vec![true, false],
            ..Default::default()
        };
        assert_eq!(
            active(&s, &snap, Conditions::default(), 2),
            vec![false, true]
        );
        let game = Snapshot {
            covered: vec![true, false],
            fullscreen: Some(0),
            ..Default::default()
        };
        assert_eq!(
            active(&s, &game, Conditions::default(), 2),
            vec![false, false]
        );
        let s2 = Settings {
            fullscreen: FullscreenRule::PauseDisplay,
            pause_when_covered: false,
            ..Default::default()
        };
        assert_eq!(
            active(&s2, &game, Conditions::default(), 2),
            vec![false, true]
        );
        let battery = Snapshot {
            on_battery: true,
            covered: vec![false],
            ..Default::default()
        };
        assert_eq!(active(&s, &battery, Conditions::default(), 1), vec![false]);
        let locked = Conditions {
            locked: true,
            ..Default::default()
        };
        assert_eq!(active(&s, &Snapshot::default(), locked, 1), vec![false]);
        assert_eq!(
            active(&s, &Snapshot::default(), Conditions::default(), 1),
            vec![true]
        );
    }
}

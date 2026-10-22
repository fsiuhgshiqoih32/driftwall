//! The library window: import videos and pictures, pick a display, set wallpapers, adjust
//! fit, volume and speed, and the pause rules. Writes library.json and settings.json and
//! tells the background process to reload; it never plays anything itself.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, CornerRadius, FontId, Layout, Rect, RichText, Sense,
    Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, pos2, vec2,
};

use crate::desktop::{self, Display};
use crate::model::{Fit, FullscreenRule, Item, Kind, Library, Settings};
use crate::{autostart, import, ipc, mf, paths};

const BG: Color32 = Color32::from_rgb(20, 21, 24);
const PANEL: Color32 = Color32::from_rgb(28, 29, 33);
const CARD: Color32 = Color32::from_rgb(36, 37, 42);
const TEXT: Color32 = Color32::from_rgb(222, 222, 226);
const DIM: Color32 = Color32::from_rgb(140, 142, 150);
const ACCENT: Color32 = Color32::from_rgb(64, 190, 210);
const WARN: Color32 = Color32::from_rgb(232, 160, 90);

const CARD_W: f32 = 208.0;
const THUMB_H: f32 = 117.0;
const CARD_H: f32 = THUMB_H + 46.0;
const GAP: f32 = 14.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Library,
    Settings,
}

enum ImportMsg {
    Done(Box<Item>),
    Failed(PathBuf, String),
}

/// One worker thread that imports files in the order they were added.
struct Importer {
    jobs: Sender<PathBuf>,
    results: Receiver<ImportMsg>,
    /// Files sent but not reported yet, and done so far in this batch.
    pending: usize,
    done: usize,
}

impl Importer {
    fn start(ctx: egui::Context) -> Self {
        let (jobs, job_rx) = channel::<PathBuf>();
        let (tx, results) = channel();
        std::thread::Builder::new()
            .name("driftwall-import".into())
            .spawn(move || {
                mf::init_com_mta();
                for f in job_rx {
                    let msg = match import::import(&f) {
                        Ok(item) => ImportMsg::Done(Box::new(item)),
                        Err(e) => ImportMsg::Failed(f, format!("{e:#}")),
                    };
                    if tx.send(msg).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn import thread");
        Self {
            jobs,
            results,
            pending: 0,
            done: 0,
        }
    }
}

pub struct App {
    library: Library,
    settings: Settings,
    displays: Vec<Display>,
    /// Index into `displays` that "Set as wallpaper" targets.
    target: usize,
    selected: Option<String>,
    thumbs: HashMap<String, Option<TextureHandle>>,
    importer: Option<Importer>,
    status: Option<(String, bool, Instant)>,
    view: View,
    title_edit: Option<(String, String)>,
    confirm_remove: Option<String>,
    daemon_running: bool,
    daemon_checked: Instant,
}

pub fn apply_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.faint_bg_color = CARD;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = Color32::from_rgb(30, 92, 104);
    v.selection.stroke.color = TEXT;
    v.hyperlink_color = ACCENT;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(46, 48, 54);
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 4.0);
        s.spacing.scroll = egui::style::ScrollStyle::solid();
    });
}

fn fmt_duration(ms: u64) -> String {
    let s = ms.div_ceil(1000);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn fmt_bytes(b: u64) -> String {
    match b {
        0..1_000_000 => format!("{:.0} KB", b as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        _ => format!("{:.2} GB", b as f64 / 1e9),
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::build(cc, true)
    }

    /// For off-screen tests: no background process, no registry changes.
    pub fn for_test(cc: &eframe::CreationContext<'_>) -> Self {
        Self::build(cc, false)
    }

    pub fn select(&mut self, id: &str) {
        self.selected = Some(id.to_string());
    }

    /// Screenshots: show the status line as it normally reads.
    pub fn assume_background_running(&mut self) {
        self.daemon_running = true;
        self.daemon_checked = Instant::now() + Duration::from_secs(3600);
    }

    pub fn show_settings(&mut self) {
        self.view = View::Settings;
    }

    /// Whether every thumbnail has been loaded (tests wait for this).
    pub fn thumbs_loaded(&self) -> bool {
        self.library
            .items
            .iter()
            .all(|i| self.thumbs.contains_key(&i.id))
    }

    fn build(cc: &eframe::CreationContext<'_>, system: bool) -> Self {
        apply_theme(&cc.egui_ctx);
        mf::init_com_sta();
        let mut library = Library::load();
        // Items whose media file vanished (deleted by hand) are dropped.
        library.items.retain(|i| i.path().exists());
        let mut settings = Settings::load();
        settings.prune(&library);
        let first_run = !paths::settings_file().exists();
        let displays = desktop::displays().unwrap_or_default();
        let mut app = Self {
            library,
            settings,
            displays,
            target: 0,
            selected: None,
            thumbs: HashMap::new(),
            importer: None,
            status: None,
            view: View::Library,
            title_edit: None,
            confirm_remove: None,
            daemon_running: system && ipc::daemon_running(),
            daemon_checked: Instant::now(),
        };
        if !system {
            return app;
        }
        app.remove_orphans();
        if first_run {
            let _ = app.settings.save();
            autostart::set(app.settings.start_with_windows);
        }
        if !mf::available() {
            app.say(mf::MISSING, true);
        }
        // The background process keeps wallpapers running after this window closes.
        if !app.daemon_running {
            ipc::spawn_daemon();
        }
        app
    }

    /// Deletes media and thumbnails no library item uses (left behind when a file was still
    /// open as it was removed).
    fn remove_orphans(&self) {
        let used: std::collections::HashSet<String> = self
            .library
            .items
            .iter()
            .flat_map(|i| {
                [
                    i.file.to_lowercase(),
                    format!("{}.jpg", i.id).to_lowercase(),
                ]
            })
            .collect();
        for dir in [paths::media_dir(), paths::thumbs_dir()] {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_lowercase();
                if !used.contains(&name) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    fn say(&mut self, msg: impl Into<String>, error: bool) {
        let msg = msg.into();
        if error {
            log::warn!("{msg}");
        }
        self.status = Some((msg, error, Instant::now()));
    }

    fn save_settings(&mut self) {
        self.settings.prune(&self.library);
        match self.settings.save() {
            Ok(()) => ipc::notify_reload(),
            Err(e) => self.say(format!("Could not save settings: {e}"), true),
        }
    }

    fn save_library(&mut self, reload: bool) {
        match self.library.save() {
            Ok(()) if reload => ipc::notify_reload(),
            Ok(()) => {}
            Err(e) => self.say(format!("Could not save the library: {e}"), true),
        }
    }

    fn display_label(&self, i: usize) -> String {
        self.displays.get(i).map_or_else(
            || "display".into(),
            |d| format!("display {} ({})", i + 1, d.name),
        )
    }

    /// Display indexes showing `id` (or every display when it spans).
    fn shown_on(&self, id: &str) -> Vec<String> {
        if self.settings.span.as_deref() == Some(id) {
            return vec!["All".into()];
        }
        if self.settings.span.is_some() {
            return Vec::new();
        }
        self.displays
            .iter()
            .enumerate()
            .filter(|(_, d)| self.settings.displays.get(&d.id).is_some_and(|i| i == id))
            .map(|(i, _)| (i + 1).to_string())
            .collect()
    }

    fn set_on(&mut self, id: &str, display: usize) {
        let Some(d) = self.displays.get(display) else {
            return;
        };
        self.settings.span = None;
        self.settings.displays.insert(d.id.clone(), id.to_string());
        let title = self
            .library
            .get(id)
            .map(|i| i.title.clone())
            .unwrap_or_default();
        self.say(
            format!("{title} is now the wallpaper on display {}", display + 1),
            false,
        );
        self.save_settings();
    }

    fn set_on_all(&mut self, id: &str) {
        self.settings.span = None;
        for d in &self.displays {
            self.settings.displays.insert(d.id.clone(), id.to_string());
        }
        self.say("Set on all displays", false);
        self.save_settings();
    }

    fn set_span(&mut self, id: &str) {
        self.settings.span = Some(id.to_string());
        self.say("Spanning all displays", false);
        self.save_settings();
    }

    fn clear_display(&mut self, i: usize) {
        if self.settings.span.is_some() {
            self.settings.span = None;
        } else if let Some(d) = self.displays.get(i) {
            self.settings.displays.remove(&d.id);
        }
        self.save_settings();
    }

    fn clear_all(&mut self) {
        self.settings.span = None;
        self.settings.displays.clear();
        self.say("Wallpapers cleared; Windows' own pictures are back", false);
        self.save_settings();
    }

    fn start_import(&mut self, paths: Vec<PathBuf>, ctx: &egui::Context) {
        let mut files = Vec::new();
        for p in paths {
            if p.is_dir() {
                if let Ok(rd) = std::fs::read_dir(&p) {
                    let mut inner: Vec<PathBuf> = rd
                        .flatten()
                        .map(|e| e.path())
                        .filter(|f| import::kind_of(f).is_some())
                        .collect();
                    inner.sort();
                    files.extend(inner);
                }
            } else {
                files.push(p);
            }
        }
        let imp = self
            .importer
            .get_or_insert_with(|| Importer::start(ctx.clone()));
        for f in files {
            if imp.jobs.send(f).is_ok() {
                imp.pending += 1;
            }
        }
    }

    fn importing(&self) -> bool {
        self.importer.as_ref().is_some_and(|i| i.pending > 0)
    }

    fn poll_import(&mut self) {
        let Some(imp) = &mut self.importer else {
            return;
        };
        let mut added: Vec<Item> = Vec::new();
        let mut errors: Vec<(PathBuf, String)> = Vec::new();
        for m in imp.results.try_iter() {
            imp.pending = imp.pending.saturating_sub(1);
            imp.done += 1;
            match m {
                ImportMsg::Done(item) => added.push(*item),
                ImportMsg::Failed(p, e) => errors.push((p, e)),
            }
        }
        if imp.pending == 0 {
            imp.done = 0;
        }
        if !added.is_empty() {
            let n = added.len();
            self.selected = added.last().map(|i| i.id.clone());
            self.library.items.extend(added);
            self.save_library(false);
            self.say(
                if n == 1 {
                    "Added 1 file".to_string()
                } else {
                    format!("Added {n} files")
                },
                false,
            );
        }
        if let Some((p, e)) = errors.last() {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let more = if errors.len() > 1 {
                format!(" (and {} more)", errors.len() - 1)
            } else {
                String::new()
            };
            self.say(format!("Could not add {name}{more}: {e}"), true);
        }
    }

    fn pick_files(&mut self, ctx: &egui::Context) {
        let mut exts: Vec<&str> = import::VIDEO_EXTS.to_vec();
        exts.extend_from_slice(import::IMAGE_EXTS);
        if let Some(files) = rfd::FileDialog::new()
            .set_title("Add videos or pictures")
            .add_filter("Videos and pictures", &exts)
            .add_filter("Videos", import::VIDEO_EXTS)
            .add_filter("Pictures", import::IMAGE_EXTS)
            .pick_files()
        {
            self.start_import(files, ctx);
        }
    }

    fn thumb(&mut self, ctx: &egui::Context, item: &Item) -> Option<TextureHandle> {
        if let Some(t) = self.thumbs.get(&item.id) {
            return t.clone();
        }
        let tex = crate::wic::load_rgba(&item.thumb_path(), 384, 216)
            .ok()
            .map(|(w, h, rgba)| {
                let img = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                ctx.load_texture(format!("thumb-{}", item.id), img, TextureOptions::LINEAR)
            });
        self.thumbs.insert(item.id.clone(), tex.clone());
        tex
    }

    fn remove_item(&mut self, id: &str) {
        if let Some(pos) = self.library.items.iter().position(|i| i.id == id) {
            let item = self.library.items.remove(pos);
            let was_active = !self.shown_on(id).is_empty();
            self.settings.prune(&self.library);
            // Save settings first so the background process lets go of the file.
            if was_active {
                self.save_settings();
            }
            self.save_library(false);
            let it = item.clone();
            std::thread::spawn(move || {
                // Give the player a moment to close the file.
                std::thread::sleep(Duration::from_millis(if was_active { 1500 } else { 0 }));
                import::remove_files(&it);
            });
            self.thumbs.remove(id);
            if self.selected.as_deref() == Some(id) {
                self.selected = None;
            }
            self.say(format!("Removed {}", item.title), false);
        }
    }

    // ---- drawing ----

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Driftwall").strong().size(16.0));
            ui.add_space(12.0);
            if ui
                .selectable_label(self.view == View::Library, "Library")
                .clicked()
            {
                self.view = View::Library;
            }
            if ui
                .selectable_label(self.view == View::Settings, "Settings")
                .clicked()
            {
                self.view = View::Settings;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button(RichText::new("Add files").strong()).clicked() {
                    let ctx = ui.ctx().clone();
                    self.pick_files(&ctx);
                }
            });
        });
        ui.add_space(2.0);
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if let Some(imp) = self.importer.as_ref().filter(|i| i.pending > 0) {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(format!(
                    "Adding {} of {}",
                    imp.done + 1,
                    imp.done + imp.pending
                ));
            } else if let Some((msg, err, at)) = &self.status
                && (at.elapsed() < Duration::from_secs(12) || *err)
            {
                ui.label(RichText::new(msg).color(if *err { WARN } else { DIM }));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (text, color) = if self.daemon_running {
                    ("Running in the background", DIM)
                } else {
                    ("Background process stopped", WARN)
                };
                ui.label(RichText::new(text).color(color).size(12.0));
            });
        });
    }

    fn displays_strip(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Displays").color(DIM));
            if self.displays.len() > 1 {
                ui.label(
                    RichText::new("click one to choose where wallpapers go")
                        .color(DIM)
                        .size(12.0),
                );
            }
        });
        let mut clear: Option<usize> = None;
        ui.horizontal_wrapped(|ui| {
            for i in 0..self.displays.len() {
                let d = self.displays[i].clone();
                let current = if let Some(span) = &self.settings.span {
                    Some(span.clone())
                } else {
                    self.settings.displays.get(&d.id).cloned()
                };
                let item = current
                    .as_deref()
                    .and_then(|id| self.library.get(id))
                    .cloned();
                let size = vec2(230.0, 64.0);
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                let selected = self.target == i;
                let p = ui.painter();
                p.rect_filled(
                    rect,
                    8.0,
                    if resp.hovered() {
                        Color32::from_rgb(42, 44, 50)
                    } else {
                        CARD
                    },
                );
                if selected {
                    p.rect_stroke(rect, 8.0, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
                }
                let thumb = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(85.0, 48.0));
                match item.as_ref().and_then(|it| self.thumb(&ctx, it)) {
                    Some(tex) => {
                        egui::Image::new((tex.id(), thumb.size()))
                            .corner_radius(4.0)
                            .paint_at(ui, thumb);
                    }
                    None => {
                        ui.painter().rect_filled(thumb, 4.0, BG);
                    }
                }
                let p = ui.painter();
                let tx = thumb.right() + 10.0;
                p.text(
                    pos2(tx, rect.top() + 10.0),
                    Align2::LEFT_TOP,
                    format!("{}  {}", i + 1, d.name),
                    FontId::proportional(13.0),
                    TEXT,
                );
                p.text(
                    pos2(tx, rect.top() + 27.0),
                    Align2::LEFT_TOP,
                    d.size_label(),
                    FontId::proportional(11.5),
                    DIM,
                );
                let what = item
                    .as_ref()
                    .map_or("Windows picture".to_string(), |it| it.title.clone());
                let what: String = if what.chars().count() > 18 {
                    what.chars().take(17).chain(['…']).collect()
                } else {
                    what
                };
                p.text(
                    pos2(tx, rect.top() + 42.0),
                    Align2::LEFT_TOP,
                    what,
                    FontId::proportional(11.5),
                    if item.is_some() { ACCENT } else { DIM },
                );
                if resp.clicked() {
                    self.target = i;
                }
                if item.is_some() {
                    resp.context_menu(|ui| {
                        if ui.button("Clear this display").clicked() {
                            clear = Some(i);
                            ui.close();
                        }
                    });
                }
            }
        });
        if let Some(i) = clear {
            self.clear_display(i);
        }
    }

    fn library_view(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.displays_strip(ui);
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(4.0);
        if self.library.items.is_empty() {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Your library is empty").size(18.0));
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Drop videos or pictures on this window, or use Add files.")
                        .color(DIM),
                );
                ui.label(
                    RichText::new(
                        "MP4, MKV, WebM, MOV, WMV, AVI · JPG, PNG, WebP, HEIC, GIF, BMP, TIFF",
                    )
                    .color(DIM)
                    .size(12.0),
                );
                ui.add_space(14.0);
                if ui
                    .add_enabled(!self.importing(), egui::Button::new("Add files"))
                    .clicked()
                {
                    self.pick_files(&ctx);
                }
            });
            return;
        }
        let ids: Vec<String> = self
            .library
            .items
            .iter()
            .rev()
            .map(|i| i.id.clone())
            .collect();
        let mut action: Option<(String, &'static str, usize)> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = ui.available_width();
                let cols = (((width + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
                for row in ids.chunks(cols) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = GAP;
                        for id in row {
                            let Some(item) = self.library.get(id).cloned() else {
                                continue;
                            };
                            let (rect, resp) =
                                ui.allocate_exact_size(vec2(CARD_W, CARD_H), Sense::click());
                            let selected = self.selected.as_deref() == Some(id.as_str());
                            if ui.is_rect_visible(rect) {
                                self.draw_card(ui, rect, &item, selected, resp.hovered());
                            }
                            if resp.clicked() {
                                self.selected = Some(id.clone());
                            }
                            if resp.double_clicked() {
                                action = Some((id.clone(), "set", self.target));
                            }
                            resp.context_menu(|ui| {
                                if ui
                                    .button(format!("Set on {}", self.display_label(self.target)))
                                    .clicked()
                                {
                                    action = Some((id.clone(), "set", self.target));
                                    ui.close();
                                }
                                for (i, _) in self.displays.iter().enumerate() {
                                    if i != self.target
                                        && ui
                                            .button(format!("Set on {}", self.display_label(i)))
                                            .clicked()
                                    {
                                        action = Some((id.clone(), "set", i));
                                        ui.close();
                                    }
                                }
                                if self.displays.len() > 1 {
                                    if ui.button("Set on all displays").clicked() {
                                        action = Some((id.clone(), "all", 0));
                                        ui.close();
                                    }
                                    if ui.button("Span across displays").clicked() {
                                        action = Some((id.clone(), "span", 0));
                                        ui.close();
                                    }
                                }
                                ui.separator();
                                if ui.button("Remove from library").clicked() {
                                    action = Some((id.clone(), "remove", 0));
                                    ui.close();
                                }
                            });
                        }
                    });
                    ui.add_space(GAP);
                }
            });
        if let Some((id, what, display)) = action {
            match what {
                "set" => self.set_on(&id, display),
                "all" => self.set_on_all(&id),
                "span" => self.set_span(&id),
                "remove" => self.confirm_remove = Some(id),
                _ => {}
            }
        }
    }

    fn draw_card(&mut self, ui: &mut Ui, rect: Rect, item: &Item, selected: bool, hovered: bool) {
        let ctx = ui.ctx().clone();
        let thumb = Rect::from_min_size(rect.min, vec2(CARD_W, THUMB_H));
        match self.thumb(&ctx, item) {
            Some(tex) => {
                egui::Image::new((tex.id(), thumb.size()))
                    .corner_radius(6.0)
                    .paint_at(ui, thumb);
            }
            None => {
                ui.painter().rect_filled(thumb, 6.0, CARD);
            }
        }
        let p = ui.painter();
        if selected {
            p.rect_stroke(thumb, 6.0, Stroke::new(2.0, ACCENT), StrokeKind::Outside);
        } else if hovered {
            p.rect_stroke(
                thumb,
                6.0,
                Stroke::new(1.0, Color32::from_gray(90)),
                StrokeKind::Outside,
            );
        }
        // Where it's showing.
        let mut x = thumb.left() + 6.0;
        for label in self.shown_on(&item.id) {
            let w = 10.0 + 7.0 * label.len() as f32;
            let r = Rect::from_min_size(pos2(x, thumb.top() + 6.0), vec2(w, 18.0));
            p.rect_filled(r, CornerRadius::same(9), ACCENT);
            p.text(
                r.center(),
                Align2::CENTER_CENTER,
                label,
                FontId::proportional(11.0),
                Color32::from_rgb(10, 30, 34),
            );
            x += w + 4.0;
        }
        if item.kind == Kind::Video {
            let text = fmt_duration(item.duration_ms);
            let r = Rect::from_min_size(
                pos2(thumb.right() - 46.0, thumb.bottom() - 22.0),
                vec2(40.0, 16.0),
            );
            p.rect_filled(r, 4.0, Color32::from_black_alpha(170));
            p.text(
                r.center(),
                Align2::CENTER_CENTER,
                text,
                FontId::proportional(11.0),
                TEXT,
            );
        }
        let title: String = if item.title.chars().count() > 28 {
            item.title.chars().take(27).chain(['…']).collect()
        } else {
            item.title.clone()
        };
        p.text(
            pos2(rect.left() + 2.0, thumb.bottom() + 8.0),
            Align2::LEFT_TOP,
            title,
            FontId::proportional(13.5),
            TEXT,
        );
        let sub = match item.kind {
            Kind::Video => format!("Video · {}x{}", item.width, item.height),
            Kind::Image => format!("Picture · {}x{}", item.width, item.height),
        };
        p.text(
            pos2(rect.left() + 2.0, thumb.bottom() + 27.0),
            Align2::LEFT_TOP,
            sub,
            FontId::proportional(11.5),
            DIM,
        );
    }

    fn side_panel(&mut self, ui: &mut Ui, id: &str) {
        let ctx = ui.ctx().clone();
        let Some(item) = self.library.get(id).cloned() else {
            return;
        };
        ui.add_space(8.0);
        if let Some(tex) = self.thumb(&ctx, &item) {
            let w = ui.available_width();
            ui.add(egui::Image::new((tex.id(), vec2(w, w * 9.0 / 16.0))).corner_radius(6.0));
        }
        ui.add_space(6.0);
        // Title, editable.
        let editing = self.title_edit.as_ref().is_some_and(|(i, _)| i == id);
        if !editing {
            self.title_edit = Some((id.to_string(), item.title.clone()));
        }
        let mut commit = None;
        if let Some((_, text)) = &mut self.title_edit {
            let r = ui.add(
                egui::TextEdit::singleline(text)
                    .font(FontId::proportional(15.0))
                    .desired_width(f32::INFINITY),
            );
            if r.lost_focus() && text.trim() != item.title && !text.trim().is_empty() {
                commit = Some(text.trim().to_string());
            }
        }
        if let Some(t) = commit {
            if let Some(it) = self.library.get_mut(id) {
                it.title = t;
            }
            self.save_library(false);
        }

        ui.add_space(4.0);
        egui::Grid::new("info")
            .num_columns(2)
            .spacing(vec2(12.0, 4.0))
            .show(ui, |ui| {
                let row = |ui: &mut Ui, k: &str, v: String| {
                    ui.label(RichText::new(k).color(DIM));
                    ui.label(v);
                    ui.end_row();
                };
                row(
                    ui,
                    "Type",
                    if item.kind == Kind::Video {
                        "Video".into()
                    } else {
                        "Picture".into()
                    },
                );
                row(ui, "Size", format!("{} x {}", item.width, item.height));
                if item.kind == Kind::Video {
                    row(ui, "Length", fmt_duration(item.duration_ms));
                    if item.fps > 0.0 {
                        row(ui, "Frame rate", format!("{:.0} fps", item.fps));
                    }
                    row(ui, "Codec", item.codec.clone());
                    row(
                        ui,
                        "Audio",
                        if item.has_audio {
                            "Yes".into()
                        } else {
                            "None".into()
                        },
                    );
                }
                row(ui, "File", fmt_bytes(item.bytes));
            });
        let shown = self.shown_on(id);
        if !shown.is_empty() {
            ui.label(RichText::new(format!("On display {}", shown.join(", "))).color(ACCENT));
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);
        let mut changed = false;
        let mut fit = item.fit;
        ui.horizontal(|ui| {
            ui.label("Fit");
            for f in Fit::ALL {
                if ui.selectable_value(&mut fit, f, f.label()).changed() {
                    changed = true;
                }
            }
        });
        let mut volume = item.volume;
        let mut speed = item.speed();
        let mut release = false;
        if item.kind == Kind::Video {
            ui.add_enabled_ui(item.has_audio, |ui| {
                let r = ui.add(
                    egui::Slider::new(&mut volume, 0..=100)
                        .text("Volume")
                        .suffix("%"),
                );
                release |= r.drag_stopped() || (r.changed() && !r.dragged());
            });
            if !item.has_audio {
                ui.label(
                    RichText::new("This video has no sound")
                        .color(DIM)
                        .size(12.0),
                );
            } else if volume == 0 {
                ui.label(
                    RichText::new("Muted: the sound isn't even decoded")
                        .color(DIM)
                        .size(12.0),
                );
            }
            let r = ui.add(
                egui::Slider::new(&mut speed, 0.25..=2.0)
                    .text("Speed")
                    .step_by(0.05)
                    .suffix("x"),
            );
            release |= r.drag_stopped() || (r.changed() && !r.dragged());
        }
        if let Some(it) = self.library.get_mut(id) {
            it.fit = fit;
            it.volume = volume;
            it.speed = speed;
        }
        if changed || release {
            let active = !self.shown_on(id).is_empty();
            self.save_library(active);
        }

        ui.add_space(10.0);
        let target = self.display_label(self.target);
        if ui
            .add_sized(
                vec2(ui.available_width(), 30.0),
                egui::Button::new(
                    RichText::new(format!("Set on display {}", self.target + 1)).strong(),
                ),
            )
            .on_hover_text(format!("Set on {target}"))
            .clicked()
        {
            self.set_on(id, self.target);
        }
        if self.displays.len() > 1 {
            ui.horizontal(|ui| {
                if ui.button("Set on all displays").clicked() {
                    self.set_on_all(id);
                }
                if ui
                    .button("Span")
                    .on_hover_text("One wallpaper stretched across every display")
                    .clicked()
                {
                    self.set_span(id);
                }
            });
        }
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if ui.button("Show file").clicked() {
                let _ = std::process::Command::new("explorer")
                    .arg("/select,")
                    .arg(item.path())
                    .spawn();
            }
            if ui.button(RichText::new("Remove").color(WARN)).clicked() {
                self.confirm_remove = Some(id.to_string());
            }
        });
        if !item.original.is_empty() {
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    RichText::new(format!("From {}", item.original))
                        .color(DIM)
                        .size(11.5),
                )
                .wrap(),
            );
        }
    }

    fn settings_view(&mut self, ui: &mut Ui) {
        let mut s = self.settings.clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(640.0);
                ui.add_space(6.0);
                ui.label(RichText::new("Pausing").strong());
                ui.add_space(2.0);
                ui.checkbox(
                    &mut s.pause_when_covered,
                    "Pause a display's video while windows cover it completely",
                );
                ui.add_space(4.0);
                ui.label("When an app or game is full screen");
                ui.indent("fs", |ui| {
                    ui.radio_value(
                        &mut s.fullscreen,
                        FullscreenRule::PauseAll,
                        "Pause every display (frees the GPU for games)",
                    );
                    ui.radio_value(
                        &mut s.fullscreen,
                        FullscreenRule::PauseDisplay,
                        "Pause only the display it's on",
                    );
                    ui.radio_value(
                        &mut s.fullscreen,
                        FullscreenRule::KeepPlaying,
                        "Keep playing",
                    );
                });
                ui.add_space(4.0);
                ui.checkbox(&mut s.pause_on_battery, "Pause on battery power");
                ui.checkbox(
                    &mut s.pause_on_battery_saver,
                    "Pause while battery saver is on",
                );
                ui.label(
                    RichText::new("Videos also pause while the PC is locked or the screen is off.")
                        .color(DIM)
                        .size(12.0),
                );

                ui.add_space(14.0);
                ui.label(RichText::new("System").strong());
                ui.add_space(2.0);
                ui.checkbox(&mut s.start_with_windows, "Start with Windows");
                ui.checkbox(
                    &mut s.still_frame,
                    "Use each video's first frame as the Windows desktop picture",
                )
                .on_hover_text(
                    "The desktop then looks right while Windows starts and after Driftwall quits",
                );
                ui.checkbox(&mut s.low_priority, "Run at low CPU priority");

                ui.add_space(14.0);
                ui.label(RichText::new("Background process").strong());
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    if self.daemon_running {
                        if ui.button("Pause or resume videos").clicked() {
                            ipc::toggle_pause();
                        }
                        if ui
                            .button("Quit background process")
                            .on_hover_text("Videos stop until Driftwall is opened again")
                            .clicked()
                        {
                            ipc::quit_daemon();
                        }
                    } else if ui.button("Start").clicked() {
                        ipc::spawn_daemon();
                    }
                });
                ui.add_space(6.0);
                if ui
                    .button("Clear all wallpapers")
                    .on_hover_text("Puts Windows' own desktop pictures back")
                    .clicked()
                {
                    self.clear_all();
                    s = self.settings.clone();
                }

                ui.add_space(14.0);
                ui.label(RichText::new("Files").strong());
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    if ui.button("Open data folder").clicked() {
                        let _ = std::process::Command::new("explorer")
                            .arg(paths::home())
                            .spawn();
                    }
                    if ui.button("Open log").clicked() {
                        let _ = std::process::Command::new("notepad")
                            .arg(paths::log_file())
                            .spawn();
                    }
                });
                let total: u64 = self.library.items.iter().map(|i| i.bytes).sum();
                ui.label(
                RichText::new(format!(
                    "{} items, {}. Files on the same drive are hard links and take no extra space.",
                    self.library.items.len(),
                    fmt_bytes(total)
                ))
                .color(DIM)
                .size(12.0),
            );

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Driftwall {}", env!("CARGO_PKG_VERSION")))
                            .color(DIM),
                    );
                    ui.hyperlink_to(
                        "github.com/fsiuhgshiqoih32/driftwall",
                        "https://github.com/fsiuhgshiqoih32/driftwall",
                    );
                });
            });
        if s != self.settings {
            if s.start_with_windows != self.settings.start_with_windows {
                autostart::set(s.start_with_windows);
            }
            self.settings = s;
            self.save_settings();
        }
    }

    fn remove_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_remove.clone() else {
            return;
        };
        let Some(item) = self.library.get(&id).cloned() else {
            self.confirm_remove = None;
            return;
        };
        let mut go = false;
        let mut close = false;
        egui::Modal::new(egui::Id::unique("confirm-remove")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.label(
                RichText::new(format!("Remove {}?", item.title))
                    .strong()
                    .size(15.0),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("It leaves the library. The original file is not touched.")
                    .color(DIM),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Remove").color(WARN)).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        });
        if go {
            self.confirm_remove = None;
            self.remove_item(&id);
        } else if close {
            self.confirm_remove = None;
        }
    }

    fn drop_zone(&mut self, ctx: &egui::Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering {
            let rect = ctx.content_rect();
            let p = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("drop"),
            ));
            p.rect_filled(rect, 0.0, Color32::from_black_alpha(170));
            p.rect_stroke(
                rect.shrink(12.0),
                10.0,
                Stroke::new(2.0, ACCENT),
                StrokeKind::Inside,
            );
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Drop to add to the library",
                FontId::proportional(20.0),
                TEXT,
            );
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        if !dropped.is_empty() {
            self.start_import(dropped, ctx);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_import();
        if Instant::now() > self.daemon_checked + Duration::from_secs(2) {
            self.daemon_running = ipc::daemon_running();
            self.daemon_checked = Instant::now();
        }
        if self.target >= self.displays.len() {
            self.target = 0;
        }
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.view == View::Library
            && let Some(id) = self.selected.clone()
        {
            egui::Panel::right("side")
                .resizable(false)
                .default_size(300.0)
                .min_size(280.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.side_panel(ui, &id));
                });
        }
        egui::CentralPanel::default().show(ui, |ui| match self.view {
            View::Library => self.library_view(ui),
            View::Settings => self.settings_view(ui),
        });
        self.remove_dialog(&ctx);
        self.drop_zone(&ctx);
        // Keep the "running" indicator and import progress fresh without spinning.
        ctx.request_repaint_after(Duration::from_secs(2));
    }
}

fn options(renderer: eframe::Renderer) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Driftwall")
            .with_app_id("driftwall")
            .with_inner_size([1120.0, 740.0])
            .with_min_inner_size([760.0, 500.0])
            .with_drag_and_drop(true)
            .with_icon(egui::IconData {
                rgba: crate::art::icon_rgba(64),
                width: 64,
                height: 64,
            }),
        renderer,
        ..Default::default()
    }
}

pub fn run() -> anyhow::Result<()> {
    let Some(_instance) = ipc::SingleInstance::acquire(ipc::WINDOW_MUTEX) else {
        ipc::raise_window();
        return Ok(());
    };
    let created = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let creator =
        |created: std::sync::Arc<std::sync::atomic::AtomicBool>| -> eframe::AppCreator<'static> {
            Box::new(move |cc| {
                created.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(Box::new(App::new(cc)) as Box<dyn eframe::App>)
            })
        };
    // OpenGL starts fastest; machines without OpenGL 2 (VMs, remote desktops) get wgpu.
    match eframe::run_native(
        "Driftwall",
        options(eframe::Renderer::Glow),
        creator(created.clone()),
    ) {
        Err(e) if !created.load(std::sync::atomic::Ordering::SeqCst) => {
            log::warn!("OpenGL unavailable ({e}); trying wgpu");
            eframe::run_native(
                "Driftwall",
                options(eframe::Renderer::Wgpu),
                creator(created),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))
        }
        other => other.map_err(|e| anyhow::anyhow!("{e}")),
    }
}

//! Renders the library window off screen to PNG files (no window opens). Run with
//!
//!     DRIFTWALL_SHOTS=<folder> cargo test --test ui_shots -- --ignored

use std::path::PathBuf;
use std::time::Duration;

use driftwall::model::{Library, Settings};
use driftwall::{desktop, import, mf, paths, ui, wic};
use eframe::egui::vec2;
use egui_kittest::Harness;

#[test]
#[ignore]
fn library_window_shots() {
    let out = PathBuf::from(std::env::var("DRIFTWALL_SHOTS").expect("set DRIFTWALL_SHOTS"));
    std::fs::create_dir_all(&out).unwrap();
    let home = std::env::temp_dir().join(format!("driftwall-shots-{}", std::process::id()));
    unsafe { std::env::set_var("DRIFTWALL_HOME", &home) };
    assert_eq!(paths::home(), home.as_path());
    mf::init_com_mta();

    // A small library: files from DRIFTWALL_SHOT_MEDIA ("1 Title.png" sorts first and is
    // titled "Title"), or the fixture videos and two generated pictures.
    let mut lib = Library::default();
    if let Some(dir) = std::env::var_os("DRIFTWALL_SHOT_MEDIA") {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        files.sort();
        for f in files {
            let mut item = import::import(&f).unwrap();
            if let Some((_, rest)) = item.title.split_once(' ') {
                item.title = rest.to_string();
            }
            lib.items.push(item);
        }
    } else {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for (name, rgb) in [
            ("Evening haze", [214u8, 120, 92]),
            ("Deep water", [40, 90, 170]),
        ] {
            let mut b = wic::Bitmap::black(1920, 1080);
            for (i, px) in b.pixels.chunks_exact_mut(4).enumerate() {
                let y = (i / 1920) as f32 / 1080.0;
                px[0] = (rgb[2] as f32 * (1.0 - y * 0.6)) as u8;
                px[1] = (rgb[1] as f32 * (1.0 - y * 0.5)) as u8;
                px[2] = (rgb[0] as f32 * (1.0 - y * 0.4)) as u8;
            }
            let p = home.join("in").join(format!("{name}.png"));
            wic::save_png(&b, &p).unwrap();
            lib.items.push(import::import(&p).unwrap());
        }
        for f in ["colors.mp4", "tone.mp4"] {
            let mut item = import::import(&fixtures.join(f)).unwrap();
            item.title = if f == "tone.mp4" {
                "Green field with sound".into()
            } else {
                "Red to blue".into()
            };
            lib.items.push(item);
        }
    }
    lib.save().unwrap();
    let displays = desktop::displays().unwrap_or_default();
    let mut s = Settings::default();
    if let Some(d) = displays.first() {
        s.displays.insert(d.id.clone(), lib.items[3].id.clone());
    }
    if let Some(d) = displays.get(1) {
        s.displays.insert(d.id.clone(), lib.items[0].id.clone());
    }
    s.save().unwrap();
    let selected = lib.items[3].id.clone();

    let mut h = Harness::builder()
        .with_size(vec2(1120.0, 740.0))
        .wgpu()
        .build_eframe(|cc| ui::App::for_test(cc));
    for _ in 0..40 {
        h.step();
        if h.state().thumbs_loaded() {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    h.state_mut().assume_background_running();
    h.step();
    h.render().unwrap().save(out.join("library.png")).unwrap();
    h.state_mut().select(&selected);
    for _ in 0..5 {
        h.step();
    }
    h.render().unwrap().save(out.join("selected.png")).unwrap();
    h.state_mut().show_settings();
    for _ in 0..5 {
        h.step();
    }
    h.render().unwrap().save(out.join("settings.png")).unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

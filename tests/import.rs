//! Importing into a throwaway data folder (DRIFTWALL_HOME), headless.

use std::path::PathBuf;

use driftwall::model::{Fit, Kind};
use driftwall::{import, mf, paths, wic};

fn home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("driftwall-import-test-{}", std::process::id()));
    // SAFETY: set before anything reads it, in this single-test binary.
    unsafe { std::env::set_var("DRIFTWALL_HOME", &dir) };
    assert_eq!(paths::home(), dir.as_path());
    dir
}

#[test]
fn imports_videos_and_pictures() {
    let dir = home();
    mf::init_com_mta();
    let png = dir.join("in").join("sunset photo.png");
    let mut bmp = wic::Bitmap::black(800, 600);
    for px in bmp.pixels.as_chunks_mut::<4>().0.iter_mut() {
        px[2] = 220;
    }
    wic::save_png(&bmp, &png).unwrap();

    let pic = import::import(&png).unwrap();
    assert_eq!(
        (pic.kind, pic.width, pic.height, pic.fit),
        (Kind::Image, 800, 600, Fit::Fill)
    );
    assert_eq!(pic.title, "sunset photo");
    assert!(pic.path().exists() && pic.thumb_path().exists());
    let (w, h, _) = wic::load_rgba(&pic.thumb_path(), 1000, 1000).unwrap();
    assert_eq!((w, h), (import::THUMB_W, import::THUMB_H));

    if mf::available() {
        let video = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.mp4");
        let v = import::import(&video).unwrap();
        assert_eq!((v.kind, v.width, v.height), (Kind::Video, 320, 180));
        assert!(v.has_audio && v.volume == 0, "imported videos start muted");
        assert!(v.path().exists() && v.thumb_path().exists());
        import::remove_files(&v);
        assert!(!v.path().exists());
    }

    assert!(import::import(&dir.join("missing.mp4")).is_err());
    std::fs::write(dir.join("notes.txt"), "x").unwrap();
    assert!(import::import(&dir.join("notes.txt")).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

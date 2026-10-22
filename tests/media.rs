//! Decoding the fixture videos through Media Foundation. Skipped where it isn't installed.

use std::path::PathBuf;

use driftwall::{mf, probe};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn mf_ready() -> bool {
    if !mf::available() {
        eprintln!("Media Foundation missing; skipping");
        return false;
    }
    true
}

#[test]
fn probes_video_properties() {
    if !mf_ready() {
        return;
    }
    let info = probe::probe(&fixture("colors.mp4")).unwrap();
    assert_eq!((info.width, info.height), (320, 180));
    assert!((info.fps - 30.0).abs() < 0.1, "{}", info.fps);
    assert!(
        (1900..=2100).contains(&info.duration_ms),
        "{}",
        info.duration_ms
    );
    assert_eq!(info.codec, "H.264");
    assert!(!info.has_audio);
    assert!(probe::probe(&fixture("tone.mp4")).unwrap().has_audio);
}

#[test]
fn grabs_frames() {
    if !mf_ready() {
        return;
    }
    // Red for the first second, blue for the second.
    let first = probe::frame_at(&fixture("colors.mp4"), 0).unwrap();
    assert_eq!((first.width, first.height), (320, 180));
    let [b, g, r, _] = first.pixel(160, 90);
    assert!(r > 200 && g < 40 && b < 40, "first frame {r} {g} {b}");
    let later = probe::frame_at(&fixture("colors.mp4"), 1500).unwrap();
    let [b, g, r, _] = later.pixel(160, 90);
    assert!(b > 200 && g < 40 && r < 40, "frame at 1.5 s {r} {g} {b}");
}

#[test]
fn rejects_non_video() {
    if !mf_ready() {
        return;
    }
    let path = std::env::temp_dir().join(format!("driftwall-not-video-{}.mp4", std::process::id()));
    std::fs::write(&path, b"this is not a video").unwrap();
    assert!(probe::probe(&path).is_err());
    let _ = std::fs::remove_file(path);
}

#[test]
fn media_engine_decodes_on_the_gpu() {
    if !mf_ready() {
        return;
    }
    let gpu = driftwall::gpu::default_device().unwrap();
    eprintln!("adapter: {}", gpu.adapter_name);
    if gpu.adapter_name.contains("Basic Render") {
        eprintln!("no GPU (software adapter); skipping");
        return;
    }
    let px = driftwall::player::decode_offscreen(
        &gpu,
        &fixture("colors.mp4"),
        640,
        360,
        std::time::Duration::from_millis(1900),
    )
    .unwrap();
    eprintln!("{px:?}");
    assert!(px.len() >= 4, "too few frames: {px:?}");
    // BGRA: red first, blue later.
    let red = px
        .iter()
        .position(|p| p[2] > 200 && p[0] < 50)
        .expect("no red frame");
    let blue = px
        .iter()
        .rposition(|p| p[0] > 200 && p[2] < 50)
        .expect("no blue frame");
    assert!(red < blue);
}

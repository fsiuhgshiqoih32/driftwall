//! Headless CPU cost of playback stages (no windows): decode only, and decode plus GPU
//! scaling into an off-screen texture at a given size.
//!
//!     cargo run --release --example bench_decode -- <video> [seconds]

use std::time::{Duration, Instant};

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

fn cpu_seconds() -> f64 {
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).unwrap() };
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
    t(k) + t(u)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = std::path::PathBuf::from(&args[0]);
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(8);
    driftwall::mf::init_thread().unwrap();
    let gpu = driftwall::gpu::default_device().unwrap();
    for (label, w, h) in [
        ("decode + scale to 2560x1440", 2560u32, 1440u32),
        ("decode + copy at 1920x1080", 1920, 1080),
    ] {
        let c0 = cpu_seconds();
        let t0 = Instant::now();
        let px = driftwall::player::decode_offscreen(&gpu, &file, w, h, Duration::from_secs(secs))
            .unwrap();
        let cpu = cpu_seconds() - c0;
        let wall = t0.elapsed().as_secs_f64();
        println!(
            "{label}: {:.1}% of one core over {wall:.1} s ({} samples)",
            cpu / wall * 100.0,
            px.len()
        );
    }
}

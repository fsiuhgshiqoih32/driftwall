//! Runs the real player loop into a hidden window (never shown) and reports CPU use; with
//! DRIFTWALL_STATS=1 the player logs frames presented and wakes per second.
//!
//!     cargo run --release --example bench_player -- <video> [seconds]

use std::time::{Duration, Instant};

use driftwall::model::Fit;
use driftwall::player::{Player, PlayerConfig, Target};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WINDOW_EX_STYLE, WS_POPUP};
use windows::core::w;

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
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    driftwall::logging::init("bench", true);
    driftwall::mf::init_thread().unwrap();
    let displays = driftwall::desktop::displays().unwrap();
    let d = displays.iter().find(|d| d.primary).unwrap();
    let (w, h) = (d.rect.width(), d.rect.height());
    // Hidden: no WS_VISIBLE, never shown.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!(""),
            WS_POPUP,
            d.rect.left,
            d.rect.top,
            w,
            h,
            None,
            None,
            None,
            None,
        )
        .unwrap()
    };
    let gpu = driftwall::gpu::for_monitor(d.hmonitor).unwrap();
    let fps = driftwall::probe::probe(&file).map(|i| i.fps).unwrap_or(0.0);
    println!(
        "{fps} fps video on a {} x {} display",
        d.rect.width(),
        d.rect.height()
    );
    let config = PlayerConfig {
        file,
        fit: Fit::Fill,
        volume: 0.0,
        speed: 1.0,
        fps,
    };
    let target = Target {
        hwnd: hwnd.0 as isize,
        width: w as u32,
        height: h as u32,
        display: "d".into(),
    };
    let player = Player::start(gpu, config, vec![target]);
    player.set_active(vec!["d".into()]);
    std::thread::sleep(Duration::from_secs(2));
    let (c0, t0) = (cpu_seconds(), Instant::now());
    std::thread::sleep(Duration::from_secs(secs));
    let cpu = cpu_seconds() - c0;
    println!(
        "CPU: {:.1}% of one core over {:.0} s",
        cpu / t0.elapsed().as_secs_f64() * 100.0,
        t0.elapsed().as_secs_f64()
    );
    if let Some(e) = player.failure() {
        println!("player failed: {e}");
    }
    player.stop();
}

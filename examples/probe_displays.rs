//! Prints what Windows reports about displays, their desktop pictures and the desktop window.
//! Read-only: opens no windows and changes nothing.
//!
//!     cargo run --example probe_displays

use windows::core::PCWSTR;

fn main() {
    driftwall::mf::init_com_mta();
    let api = driftwall::desktop::wallpaper_api().unwrap();
    for d in driftwall::desktop::displays().unwrap() {
        let w: Vec<u16> = d.id.encode_utf16().chain([0]).collect();
        let r = unsafe { api.GetMonitorRECT(PCWSTR(w.as_ptr())) };
        println!(
            "{} {} primary={} rect={:?} work={:?}",
            d.name,
            d.size_label(),
            d.primary,
            d.rect,
            d.work
        );
        println!("  id {}", d.id);
        println!("  IDesktopWallpaper rect {r:?}");
        println!(
            "  picture {:?}",
            driftwall::desktop::get_wallpaper(&api, Some(&d.id))
        );
    }
    println!("position {:?}", driftwall::desktop::get_position(&api));
    let host = driftwall::desktop::find_host();
    println!("host {host:?}");
    if let Ok(h) = host {
        // Driftwall's wallpaper windows inside the desktop.
        use windows::Win32::Foundation::{HWND, LPARAM, RECT};
        use windows::Win32::UI::WindowsAndMessaging::{
            EnumChildWindows, GetClassNameW, GetWindowRect, IsWindowVisible,
        };
        unsafe extern "system" fn each(w: HWND, _: LPARAM) -> windows::core::BOOL {
            let mut buf = [0u16; 64];
            let n = unsafe { GetClassNameW(w, &mut buf) } as usize;
            if String::from_utf16_lossy(&buf[..n]) == "DriftwallSurface" {
                let mut r = RECT::default();
                unsafe {
                    let _ = GetWindowRect(w, &mut r);
                }
                let vis = unsafe { IsWindowVisible(w) }.as_bool();
                println!(
                    "  surface {:?} visible={vis} screen rect ({}, {}) - ({}, {})",
                    w.0, r.left, r.top, r.right, r.bottom
                );
            }
            true.into()
        }
        unsafe {
            let _ = EnumChildWindows(Some(h.hwnd()), Some(each), LPARAM(0));
        }
    }
}

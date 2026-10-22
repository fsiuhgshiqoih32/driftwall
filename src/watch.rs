//! Deciding when a video can't be seen or shouldn't play: displays fully covered by windows,
//! full-screen apps and games, battery and battery saver.
//!
//! The scan avoids calls that send messages to other programs' windows (no GetWindowText),
//! so a hung app can't stall it. A scan costs well under a millisecond.

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{
    QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TRANSPARENT,
};
use windows::core::BOOL;

use crate::desktop::Display;
use crate::layout::Rect;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Per display (same order as the display list): every part of it is under a window.
    pub covered: Vec<bool>,
    /// The display a full-screen app or game is on.
    pub fullscreen: Option<usize>,
    pub on_battery: bool,
    pub battery_saver: bool,
}

/// Shell windows that sit above the desktop but don't hide it.
const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "DriftwallSurface",
    "DriftwallDaemon",
    "Windows.UI.Core.CoreWindow",
    "XamlExplorerHostIslandWindow",
    "TopLevelWindowForOverflowXamlIsland",
    "NotifyIconOverflowWindow",
];

struct Win {
    rect: Rect,
}

fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

fn cloaked(h: HWND) -> bool {
    let mut v = 0u32;
    unsafe {
        DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut v as *mut u32 as *mut _, 4).is_ok() && v != 0
    }
}

fn frame_rect(h: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    unsafe {
        if DwmGetWindowAttribute(
            h,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
        .is_err()
            && GetWindowRect(h, &mut r).is_err()
        {
            return None;
        }
    }
    Some(Rect::new(r.left, r.top, r.right, r.bottom))
}

/// Whether a top-level window can hide the desktop: visible, not minimised, not cloaked (on
/// another virtual desktop, or a suspended app), not a click-through overlay or tool window,
/// not the shell and not ours.
fn counts(h: HWND, own_pid: u32) -> bool {
    unsafe {
        if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return false;
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & (WS_EX_TRANSPARENT.0 | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0 {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == own_pid {
            return false;
        }
    }
    if cloaked(h) {
        return false;
    }
    let class = class_name(h);
    !SHELL_CLASSES.contains(&class.as_str())
}

fn windows_above_desktop() -> Vec<Win> {
    struct Ctx {
        own: u32,
        out: Vec<Win>,
    }
    unsafe extern "system" fn each(h: HWND, lp: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lp.0 as *mut Ctx) };
        if counts(h, ctx.own)
            && let Some(rect) = frame_rect(h)
            && !rect.is_empty()
        {
            ctx.out.push(Win { rect });
        }
        true.into()
    }
    let mut ctx = Ctx {
        own: unsafe { GetCurrentProcessId() },
        out: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.out
}

/// Whether `rects` together hide all of `area`, checked on a 32 x 18 grid of points (gaps
/// of a few pixels between snapped windows don't count as visible desktop).
pub fn fully_covered(area: &Rect, rects: &[Rect]) -> bool {
    if area.is_empty() {
        return false;
    }
    const NX: i32 = 32;
    const NY: i32 = 18;
    let candidates: Vec<&Rect> = rects
        .iter()
        .filter(|r| !r.intersect(area).is_empty())
        .collect();
    if candidates.is_empty() {
        return false;
    }
    for iy in 0..NY {
        let y = area.top + (area.height() * (2 * iy + 1)) / (2 * NY);
        for ix in 0..NX {
            let x = area.left + (area.width() * (2 * ix + 1)) / (2 * NX);
            if !candidates.iter().any(|r| r.contains(x, y)) {
                return false;
            }
        }
    }
    true
}

/// The display `rect` (a window) covers completely, if any.
pub fn fullscreen_on(rect: &Rect, displays: &[Display]) -> Option<usize> {
    displays.iter().position(|d| {
        rect.left <= d.rect.left
            && rect.top <= d.rect.top
            && rect.right >= d.rect.right
            && rect.bottom >= d.rect.bottom
    })
}

pub fn scan(displays: &[Display]) -> Snapshot {
    let wins = windows_above_desktop();
    let rects: Vec<Rect> = wins.iter().map(|w| w.rect).collect();
    let covered = displays
        .iter()
        .map(|d| fully_covered(&d.work, &rects))
        .collect();

    let mut fullscreen = None;
    unsafe {
        let fg = GetForegroundWindow();
        if !fg.is_invalid() && counts(fg, GetCurrentProcessId()) {
            let mut r = RECT::default();
            if GetWindowRect(fg, &mut r).is_ok() {
                fullscreen = fullscreen_on(&Rect::new(r.left, r.top, r.right, r.bottom), displays);
            }
        }
        // Exclusive full-screen games and presentation mode, which a window rectangle can miss.
        if fullscreen.is_none()
            && let Ok(state) = SHQueryUserNotificationState()
            && (state == QUNS_RUNNING_D3D_FULL_SCREEN
                || state == QUNS_BUSY
                || state == QUNS_PRESENTATION_MODE)
        {
            fullscreen = Some(0);
        }
    }

    let mut power = SYSTEM_POWER_STATUS::default();
    let (on_battery, battery_saver) = if unsafe { GetSystemPowerStatus(&mut power) }.is_ok() {
        (power.ACLineStatus == 0, power.SystemStatusFlag == 1)
    } else {
        (false, false)
    };
    Snapshot {
        covered,
        fullscreen,
        on_battery,
        battery_saver,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage() {
        let area = Rect::new(0, 0, 1920, 1040);
        // One maximised window.
        assert!(fully_covered(&area, &[Rect::new(0, 0, 1920, 1040)]));
        // Two snapped halves with a small gap still hide the desktop.
        assert!(fully_covered(
            &area,
            &[Rect::new(0, 0, 958, 1040), Rect::new(962, 0, 1920, 1040)]
        ));
        // A quarter of the screen showing.
        assert!(!fully_covered(
            &area,
            &[Rect::new(0, 0, 960, 1040), Rect::new(960, 0, 1920, 520)]
        ));
        // A window on another display.
        assert!(!fully_covered(&area, &[Rect::new(1920, 0, 3840, 1080)]));
        assert!(!fully_covered(&area, &[]));
    }

    #[test]
    fn fullscreen_detection() {
        let d = |l, t, r, b| Display {
            id: String::new(),
            wallpaper_id: None,
            name: String::new(),
            rect: Rect::new(l, t, r, b),
            work: Rect::new(l, t, r, b - 40),
            primary: l == 0,
            hmonitor: 0,
        };
        let displays = [d(0, 0, 2560, 1440), d(-1600, 0, 0, 900)];
        assert_eq!(
            fullscreen_on(&Rect::new(0, 0, 2560, 1440), &displays),
            Some(0)
        );
        assert_eq!(
            fullscreen_on(&Rect::new(-1600, 0, 0, 900), &displays),
            Some(1)
        );
        // Maximised (work area only) is not full screen.
        assert_eq!(fullscreen_on(&Rect::new(0, 0, 2560, 1400), &displays), None);
    }
}

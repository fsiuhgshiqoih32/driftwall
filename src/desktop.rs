//! The Windows desktop: finding the window that sits behind the icons, listing displays, and
//! Windows' own desktop picture (through IDesktopWallpaper).
//!
//! Window layout after Progman is sent message 0x052C:
//! - Windows 11 24H2 and later: Progman holds SHELLDLL_DefView (the icons, a layered window)
//!   above a WorkerW child. Wallpaper windows go inside that WorkerW.
//! - Earlier Windows 10/11: the icons move into a top-level WorkerW, and a second top-level
//!   WorkerW right behind it paints the picture. Wallpaper windows go inside that one.

use anyhow::{Context, Result, bail};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    MapWindowPoints,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::{DESKTOP_WALLPAPER_POSITION, DesktopWallpaper, IDesktopWallpaper};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, FindWindowW, IsWindow, SMTO_NORMAL, SendMessageTimeoutW,
};
use windows::core::{BOOL, PCWSTR, PWSTR, w};

use crate::layout::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKind {
    /// 24H2+: a WorkerW inside Progman.
    Progman,
    /// Older: the top-level WorkerW behind the icons.
    TopLevel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Host {
    pub hwnd: isize,
    pub kind: HostKind,
}

impl Host {
    pub fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut _)
    }

    pub fn alive(&self) -> bool {
        unsafe { IsWindow(Some(self.hwnd())).as_bool() }
    }

    /// Converts a rectangle in screen pixels to the host's client coordinates.
    pub fn to_client(&self, r: &Rect) -> Rect {
        let mut pts = [
            POINT {
                x: r.left,
                y: r.top,
            },
            POINT {
                x: r.right,
                y: r.bottom,
            },
        ];
        unsafe { MapWindowPoints(None, Some(self.hwnd()), &mut pts) };
        Rect::new(pts[0].x, pts[0].y, pts[1].x, pts[1].y)
    }
}

fn hwnd_of(h: HWND) -> isize {
    h.0 as isize
}

/// Finds (creating it if needed) the window wallpapers are drawn into.
pub fn find_host() -> Result<Host> {
    unsafe {
        let progman = FindWindowW(w!("Progman"), PCWSTR::null())
            .context("Progman not found (is Explorer running?)")?;
        // Ask Explorer to create the WorkerW behind the icons. Both parameter forms are in use
        // across Windows versions; neither has a visible effect.
        let mut res = 0usize;
        SendMessageTimeoutW(
            progman,
            0x052C,
            WPARAM(0xD),
            LPARAM(1),
            SMTO_NORMAL,
            1000,
            Some(&mut res),
        );
        SendMessageTimeoutW(
            progman,
            0x052C,
            WPARAM(0),
            LPARAM(0),
            SMTO_NORMAL,
            1000,
            Some(&mut res),
        );

        let defview_in_progman =
            FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), PCWSTR::null()).is_ok();
        if defview_in_progman
            && let Ok(worker) = FindWindowExW(Some(progman), None, w!("WorkerW"), PCWSTR::null())
        {
            return Ok(Host {
                hwnd: hwnd_of(worker),
                kind: HostKind::Progman,
            });
        }

        // Older layout: find the top-level window holding the icons, then the WorkerW after it.
        let mut icons: isize = 0;
        unsafe extern "system" fn find_icons(hwnd: HWND, lparam: LPARAM) -> BOOL {
            unsafe {
                if FindWindowExW(Some(hwnd), None, w!("SHELLDLL_DefView"), PCWSTR::null()).is_ok() {
                    *(lparam.0 as *mut isize) = hwnd.0 as isize;
                    return false.into();
                }
            }
            true.into()
        }
        let _ = EnumWindows(Some(find_icons), LPARAM(&mut icons as *mut isize as isize));
        if icons != 0 {
            let after = HWND(icons as *mut _);
            if let Ok(worker) = FindWindowExW(None, Some(after), w!("WorkerW"), PCWSTR::null()) {
                return Ok(Host {
                    hwnd: hwnd_of(worker),
                    kind: HostKind::TopLevel,
                });
            }
        }
        bail!("could not find the desktop window behind the icons")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Display {
    /// Stable across restarts: the monitor device path, or (when Windows has none, as with a
    /// monitor whose driver failed) the monitor's EDID ids and connector.
    pub id: String,
    /// The id IDesktopWallpaper takes for this display, when Windows has one.
    pub wallpaper_id: Option<String>,
    /// The monitor's own name ("DELL U2719D"), or "Display" when Windows doesn't know it.
    pub name: String,
    /// In screen pixels.
    pub rect: Rect,
    pub work: Rect,
    pub primary: bool,
    pub hmonitor: isize,
}

impl Display {
    pub fn size_label(&self) -> String {
        format!("{} x {}", self.rect.width(), self.rect.height())
    }
}

pub fn wallpaper_api() -> Result<IDesktopWallpaper> {
    unsafe { CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL).context("IDesktopWallpaper") }
}

fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = unsafe { p.to_string().unwrap_or_default() };
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// The attached displays: primary first, then left to right.
///
/// Monitors come from EnumDisplayMonitors; their ids (the monitor device paths that
/// IDesktopWallpaper takes) and names from the display config API, matched through the GDI
/// device name. IDesktopWallpaper's own GetMonitorDevicePathAt returns empty strings on some
/// Windows 11 builds, so it isn't used.
pub fn displays() -> Result<Vec<Display>> {
    let names = config_names();
    let mut monitors: Vec<(isize, MONITORINFOEXW)> = Vec::new();
    unsafe extern "system" fn each(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let list = unsafe { &mut *(lp.0 as *mut Vec<(isize, MONITORINFOEXW)>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(h, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) }
            .as_bool()
        {
            list.push((h.0 as isize, info));
        }
        true.into()
    }
    unsafe {
        if !EnumDisplayMonitors(
            None,
            None,
            Some(each),
            LPARAM(&mut monitors as *mut _ as isize),
        )
        .as_bool()
        {
            bail!("EnumDisplayMonitors failed");
        }
    }
    let mut out = Vec::new();
    for (hmonitor, info) in monitors {
        let gdi = wide_to_string(&info.szDevice);
        let m = info.monitorInfo;
        let found = names.iter().find(|n| n.gdi.eq_ignore_ascii_case(&gdi));
        let wallpaper_id = found.map(|n| n.path.clone()).filter(|p| !p.is_empty());
        let id = match (found, &wallpaper_id) {
            (_, Some(path)) => path.clone(),
            (Some(n), None) if n.edid.is_some() => n.edid.clone().unwrap_or_default(),
            _ => gdi.clone(),
        };
        let name = found.map(|n| n.friendly.clone()).unwrap_or_default();
        out.push(Display {
            id,
            wallpaper_id,
            name: if name.is_empty() {
                "Display".into()
            } else {
                name
            },
            rect: rect_from(&m.rcMonitor),
            work: rect_from(&m.rcWork),
            primary: m.dwFlags & MONITORINFOF_PRIMARY != 0,
            hmonitor,
        });
    }
    out.sort_by_key(|d| (!d.primary, d.rect.left, d.rect.top));
    Ok(out)
}

struct ConfigName {
    /// "\.\DISPLAY1"
    gdi: String,
    /// "\?\DISPLAY#GSM5B7F#..."; the id IDesktopWallpaper uses.
    path: String,
    friendly: String,
    /// "monitor:MMMMPPPP:connector" from the EDID, when valid.
    edid: Option<String>,
}

fn config_names() -> Vec<ConfigName> {
    let mut out = Vec::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != ERROR_SUCCESS {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return out;
        }
        for p in &paths[..np as usize] {
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                    size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                    adapterId: p.targetInfo.adapterId,
                    id: p.targetInfo.id,
                },
                ..Default::default()
            };
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: p.sourceInfo.adapterId,
                    id: p.sourceInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut target.header) != 0
                || DisplayConfigGetDeviceInfo(&mut source.header) != 0
            {
                continue;
            }
            let gdi = wide_to_string(&source.viewGdiDeviceName);
            // Duplicated displays share a source; the first target names it.
            if out.iter().any(|n: &ConfigName| n.gdi == gdi) {
                continue;
            }
            // Bit 0 of the flags: the EDID ids are valid.
            let edid = (target.flags.Anonymous.value & 1 != 0).then(|| {
                format!(
                    "monitor:{:04x}{:04x}:{}",
                    target.edidManufactureId, target.edidProductCodeId, target.connectorInstance
                )
            });
            out.push(ConfigName {
                gdi,
                path: wide_to_string(&target.monitorDevicePath),
                friendly: wide_to_string(&target.monitorFriendlyDeviceName),
                edid,
            });
        }
    }
    out
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// The picture Windows shows on a display (`None` = the single picture for all displays).
pub fn get_wallpaper(api: &IDesktopWallpaper, display: Option<&str>) -> Option<String> {
    let id = display.map(wide);
    let p = unsafe {
        api.GetWallpaper(id.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())))
            .ok()?
    };
    Some(take_pwstr(p))
}

pub fn set_wallpaper(api: &IDesktopWallpaper, display: Option<&str>, path: &str) -> Result<()> {
    let id = display.map(wide);
    let p = wide(path);
    unsafe {
        api.SetWallpaper(
            id.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
            PCWSTR(p.as_ptr()),
        )
        .with_context(|| format!("set desktop picture {path}"))
    }
}

pub fn get_position(api: &IDesktopWallpaper) -> Option<i32> {
    unsafe { api.GetPosition().ok().map(|p| p.0) }
}

pub fn set_position(api: &IDesktopWallpaper, pos: i32) -> Result<()> {
    unsafe {
        api.SetPosition(DESKTOP_WALLPAPER_POSITION(pos))
            .context("set picture position")
    }
}

/// Screen rectangle covering every display.
pub fn bounds(displays: &[Display]) -> Rect {
    displays
        .iter()
        .map(|d| d.rect)
        .reduce(|a, b| a.union(&b))
        .unwrap_or_default()
}

pub fn rect_from(r: &RECT) -> Rect {
    Rect::new(r.left, r.top, r.right, r.bottom)
}

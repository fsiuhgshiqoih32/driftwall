//! The background process (`driftwall --background`): tray icon, the engine, and the
//! Windows events that matter to it (lock, display off, power, Explorer restarts, display
//! changes, foreground changes). Everything runs on one thread's message loop; the window
//! thread and the players do the rest.

use std::cell::RefCell;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{POWERBROADCAST_SETTING, RegisterPowerSettingNotification};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification,
};
use windows::Win32::System::SystemServices::GUID_SESSION_DISPLAY_STATUS;
use windows::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, NORMAL_PRIORITY_CLASS, SetPriorityClass,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    EVENT_SYSTEM_MOVESIZEEND, GetCursorPos, GetMessageW, HICON, IDI_APPLICATION, IMAGE_ICON,
    KillTimer, LR_DEFAULTSIZE, LoadIconW, LoadImageW, MF_CHECKED, MF_SEPARATOR, MF_STRING, MSG,
    PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMEAUTOMATIC, PBT_POWERSETTINGCHANGE, PostMessageW,
    PostQuitMessage, REGISTER_NOTIFICATION_FLAGS, RegisterClassExW, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, TranslateMessage, WINEVENT_OUTOFCONTEXT, WM_APP, WM_DISPLAYCHANGE,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_POWERBROADCAST, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER,
    WM_WTSSESSION_CHANGE, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP, WTS_SESSION_LOCK,
    WTS_SESSION_UNLOCK,
};
use windows::core::{PCWSTR, w};

use crate::engine::Engine;
use crate::ipc::{self, DAEMON_CLASS, WM_QUIT_DAEMON, WM_RELOAD, WM_TOGGLE_PAUSE, WM_TRAY};
use crate::{autostart, mf};

const TIMER_TICK: usize = 1;
const TIMER_DISPLAYS: usize = 2;
const TIMER_SOON: usize = 3;
const TICK_MS: u32 = 1000;

const CMD_OPEN: usize = 1;
const CMD_PAUSE: usize = 2;
const CMD_QUIT: usize = 3;

struct Daemon {
    hwnd: HWND,
    engine: Engine,
    icon: HICON,
    taskbar_created: u32,
    ticking: bool,
}

thread_local! {
    static DAEMON: RefCell<Option<Daemon>> = const { RefCell::new(None) };
}

pub fn set_priority(low: bool) {
    unsafe {
        let class = if low {
            BELOW_NORMAL_PRIORITY_CLASS
        } else {
            NORMAL_PRIORITY_CLASS
        };
        let _ = SetPriorityClass(GetCurrentProcess(), class);
    }
}

fn tray_data(hwnd: HWND, icon: HICON, tip: &str) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    for (d, s) in nid.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *d = s;
    }
    nid
}

impl Daemon {
    fn tooltip(&self) -> String {
        if self.engine.cond.user_paused {
            "Driftwall (paused)".into()
        } else {
            "Driftwall".into()
        }
    }

    fn add_tray(&self) {
        let nid = tray_data(self.hwnd, self.icon, &self.tooltip());
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        }
    }

    fn update_tray(&self) {
        let nid = tray_data(self.hwnd, self.icon, &self.tooltip());
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    /// Runs the 1 s tick only while there are videos to watch over.
    fn schedule(&mut self) {
        let want = self.engine.has_videos() || self.engine.has_pending_retries();
        unsafe {
            if want && !self.ticking {
                SetTimer(Some(self.hwnd), TIMER_TICK, TICK_MS, None);
            } else if !want && self.ticking {
                let _ = KillTimer(Some(self.hwnd), TIMER_TICK);
            }
        }
        self.ticking = want;
    }

    fn soon(&self, ms: u32) {
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_SOON, ms, None);
        }
    }

    fn toggle_pause(&mut self) {
        self.engine.cond.user_paused = !self.engine.cond.user_paused;
        log::info!("user pause: {}", self.engine.cond.user_paused);
        self.engine.tick();
        self.update_tray();
    }

    fn quit(&mut self) {
        log::info!("quitting");
        self.engine.shutdown();
        let nid = tray_data(self.hwnd, self.icon, "");
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
        }
    }

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_TRAY => match lp.0 as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => ipc::open_window(),
                _ => {}
            },
            WM_RELOAD => {
                log::info!("reloading settings");
                self.engine.reload();
                autostart::set(self.engine.settings.start_with_windows);
                self.schedule();
            }
            WM_TOGGLE_PAUSE => self.toggle_pause(),
            WM_QUIT_DAEMON => self.quit(),
            WM_TIMER => match wp.0 {
                TIMER_TICK => {
                    self.engine.tick();
                    self.schedule();
                }
                TIMER_DISPLAYS => {
                    unsafe {
                        let _ = KillTimer(Some(self.hwnd), TIMER_DISPLAYS);
                    }
                    log::info!("displays changed");
                    self.engine.refresh_displays();
                    self.engine.apply();
                    self.schedule();
                }
                TIMER_SOON => {
                    unsafe {
                        let _ = KillTimer(Some(self.hwnd), TIMER_SOON);
                    }
                    self.engine.tick();
                }
                _ => {}
            },
            WM_DISPLAYCHANGE => unsafe {
                SetTimer(Some(self.hwnd), TIMER_DISPLAYS, 700, None);
            },
            // The work area changed (taskbar moved or resized).
            WM_SETTINGCHANGE if wp.0 == 0x002F => unsafe {
                SetTimer(Some(self.hwnd), TIMER_DISPLAYS, 700, None);
            },
            WM_WTSSESSION_CHANGE => {
                match wp.0 as u32 {
                    WTS_SESSION_LOCK => self.engine.cond.locked = true,
                    WTS_SESSION_UNLOCK => self.engine.cond.locked = false,
                    _ => return None,
                }
                self.engine.tick();
            }
            WM_POWERBROADCAST => {
                match wp.0 as u32 {
                    PBT_POWERSETTINGCHANGE => {
                        let s = unsafe { &*(lp.0 as *const POWERBROADCAST_SETTING) };
                        if s.PowerSetting == GUID_SESSION_DISPLAY_STATUS && s.DataLength >= 4 {
                            // 0 = off, 1 = on, 2 = dimmed.
                            let state = unsafe { *(s.Data.as_ptr() as *const u32) };
                            self.engine.cond.display_off = state == 0;
                            self.engine.tick();
                        }
                    }
                    PBT_APMPOWERSTATUSCHANGE => self.engine.tick(),
                    // After sleep the GPU may have been reset.
                    PBT_APMRESUMEAUTOMATIC => {
                        self.engine.rebuild();
                        self.schedule();
                    }
                    _ => {}
                }
                return Some(LRESULT(1));
            }
            m if m == self.taskbar_created && m != 0 => {
                log::info!("Explorer restarted");
                self.add_tray();
                self.engine.rebuild();
                self.schedule();
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

/// Runs `f` on the daemon unless it's already busy (a re-entrant message while a call is
/// pumping); then the message is posted again for later.
fn with_daemon(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    DAEMON.with(|cell| match cell.try_borrow_mut() {
        Ok(mut d) => d.as_mut().and_then(|d| d.handle(msg, wp, lp)),
        Err(_) => unsafe {
            if msg >= WM_APP {
                // Nothing pumps posted messages while the daemon is busy, so this runs right
                // after the current handler returns.
                let _ = PostMessageW(Some(hwnd), msg, wp, lp);
                Some(LRESULT(0))
            } else {
                // A sent message (display or power change) arrived mid-call: catch up with
                // a full refresh shortly.
                SetTimer(Some(hwnd), TIMER_DISPLAYS, 700, None);
                None
            }
        },
    })
}

/// The tray menu. Shown outside any borrow of the daemon: the menu runs a modal message loop.
fn show_menu(hwnd: HWND, paused: bool) -> usize {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return 0;
        };
        let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN, w!("Open Driftwall"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let pause_flags = if paused {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let _ = AppendMenuW(menu, pause_flags, CMD_PAUSE, w!("Pause videos"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Quit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Needed so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_TRAY && lp.0 as u32 == WM_RBUTTONUP {
        let paused = DAEMON.with(|c| {
            c.try_borrow()
                .ok()
                .and_then(|d| d.as_ref().map(|d| d.engine.cond.user_paused))
        });
        let post = |m| unsafe {
            let _ = PostMessageW(Some(hwnd), m, WPARAM(0), LPARAM(0));
        };
        match show_menu(hwnd, paused.unwrap_or(false)) {
            CMD_OPEN => ipc::open_window(),
            CMD_PAUSE => post(WM_TOGGLE_PAUSE),
            CMD_QUIT => post(WM_QUIT_DAEMON),
            _ => {}
        }
        return LRESULT(0);
    }
    match with_daemon(hwnd, msg, wp, lp) {
        Some(r) => r,
        None => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

unsafe extern "system" fn win_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _obj: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    DAEMON.with(|cell| {
        if let Ok(d) = cell.try_borrow()
            && let Some(d) = d.as_ref()
            && d.engine.has_videos()
        {
            // Let the window settle (restore animations) before scanning.
            d.soon(150);
        }
    });
}

fn load_icon() -> HICON {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        // winresource embeds the icon as resource 1.
        LoadImageW(
            Some(instance.into()),
            PCWSTR(std::ptr::without_provenance(1)),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE,
        )
        .map(|h| HICON(h.0))
        .or_else(|_| LoadIconW(None, IDI_APPLICATION))
        .unwrap_or_default()
    }
}

pub fn run() -> Result<()> {
    let Some(_instance) = ipc::SingleInstance::acquire(ipc::DAEMON_MUTEX) else {
        log::info!("already running");
        return Ok(());
    };
    // MTA: shell COM calls on this thread then never pump messages (no re-entrancy).
    mf::init_com_mta();
    log::info!("Driftwall {} starting", env!("CARGO_PKG_VERSION"));
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: DAEMON_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            DAEMON_CLASS,
            w!("Driftwall"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .context("create background window")?;
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        let _ = RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_SESSION_DISPLAY_STATUS,
            REGISTER_NOTIFICATION_FLAGS(0),
        );
        let hook = |a, b| SetWinEventHook(a, b, None, Some(win_event), 0, 0, WINEVENT_OUTOFCONTEXT);
        let _h1 = hook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND);
        let _h2 = hook(EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND);
        let _h3 = hook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND);

        let engine = Engine::new()?;
        let d = Daemon {
            hwnd,
            engine,
            icon: load_icon(),
            taskbar_created,
            ticking: false,
        };
        d.add_tray();
        DAEMON.with(|cell| *cell.borrow_mut() = Some(d));
        // First apply goes through the message loop like any reload.
        let _ = PostMessageW(Some(hwnd), WM_RELOAD, WPARAM(0), LPARAM(0));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        DAEMON.with(|cell| {
            if let Some(mut d) = cell.borrow_mut().take() {
                d.engine.shutdown();
            }
        });
    }
    log::info!("stopped");
    Ok(())
}

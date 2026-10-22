//! How the window and the background process find and signal each other: the background
//! process owns a hidden window of class `DriftwallDaemon`, and the window posts it messages.

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WPARAM,
};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, EnumWindows, FindWindowW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, PostMessageW, SW_RESTORE, SetForegroundWindow, ShowWindow,
    WM_APP,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR, w};

pub const DAEMON_CLASS: PCWSTR = w!("DriftwallDaemon");
pub const WM_TRAY: u32 = WM_APP + 1;
pub const WM_RELOAD: u32 = WM_APP + 2;
pub const WM_QUIT_DAEMON: u32 = WM_APP + 3;
pub const WM_TOGGLE_PAUSE: u32 = WM_APP + 4;

/// Holds a named mutex for the life of the process. `None` if another process has it.
pub struct SingleInstance(HANDLE);

impl SingleInstance {
    pub fn acquire(name: &str) -> Option<Self> {
        unsafe {
            let h = CreateMutexW(None, true, &HSTRING::from(name)).ok()?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(h);
                return None;
            }
            Some(Self(h))
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub const DAEMON_MUTEX: &str = "Local\\Driftwall.Background";
pub const WINDOW_MUTEX: &str = "Local\\Driftwall.Window";

pub fn daemon_window() -> Option<HWND> {
    unsafe { FindWindowW(DAEMON_CLASS, PCWSTR::null()).ok() }
}

pub fn daemon_running() -> bool {
    daemon_window().is_some()
}

/// Starts `driftwall.exe --background` detached.
pub fn spawn_daemon() {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    match Command::new(exe)
        .arg("--background")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
    {
        Ok(mut child) => {
            // Reap it so no handle lingers.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => log::warn!("could not start the background process: {e}"),
    }
}

fn post(msg: u32) -> bool {
    match daemon_window() {
        Some(h) => unsafe { PostMessageW(Some(h), msg, WPARAM(0), LPARAM(0)).is_ok() },
        None => false,
    }
}

/// Tells the background process to re-read settings; starts it if it isn't running.
pub fn notify_reload() {
    if !post(WM_RELOAD) {
        spawn_daemon();
    }
}

pub fn toggle_pause() -> bool {
    post(WM_TOGGLE_PAUSE)
}

pub fn quit_daemon() -> bool {
    post(WM_QUIT_DAEMON)
}

/// The full path of a process's program.
fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
                .is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Brings an already open Driftwall window to the front. Matches on title and on the owning
/// program, so an Explorer folder called "Driftwall" isn't mistaken for it.
pub fn raise_window() -> bool {
    struct Ctx {
        exe: String,
        found: Option<HWND>,
    }
    unsafe extern "system" fn each(h: HWND, lp: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lp.0 as *mut Ctx) };
        let mut buf = [0u16; 64];
        let n = unsafe { GetWindowTextW(h, &mut buf) } as usize;
        if String::from_utf16_lossy(&buf[..n]) != "Driftwall" {
            return true.into();
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        if process_path(pid).is_some_and(|p| p.eq_ignore_ascii_case(&ctx.exe)) {
            ctx.found = Some(h);
            return false.into();
        }
        true.into()
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut ctx = Ctx { exe, found: None };
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut ctx as *mut Ctx as isize));
        if let Some(h) = ctx.found {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            let _ = SetForegroundWindow(h);
            return true;
        }
    }
    false
}

/// Opens the Driftwall window (from the tray), letting it take the foreground.
pub fn open_window() {
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    if raise_window() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if let Ok(mut child) = Command::new(exe).spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

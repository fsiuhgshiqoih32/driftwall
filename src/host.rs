//! The thread that owns the wallpaper windows.
//!
//! A child window of Explorer's desktop shares an input queue with Explorer's thread, so if
//! the thread owning it stalls, the desktop can stall too. These windows therefore live on a
//! thread that does nothing but pump messages; rendering happens on the player threads
//! through swap chains, and all other work on the main thread.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, HTTRANSPARENT, IsWindow, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
    RegisterClassExW, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos, TranslateMessage,
    WINDOW_EX_STYLE, WM_APP, WM_ERASEBKGND, WM_NCHITTEST, WM_PAINT, WNDCLASSEXW, WS_CHILD,
    WS_CLIPSIBLINGS, WS_DISABLED, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use crate::layout::Rect;

pub const SURFACE_CLASS: PCWSTR = w!("DriftwallSurface");
const WM_HOST_WAKE: u32 = WM_APP + 0x40;

enum Cmd {
    Create {
        parent: isize,
        rect: Rect,
        reply: Sender<Result<isize, String>>,
    },
    Move {
        hwnd: isize,
        rect: Rect,
    },
    Destroy(isize),
    Quit,
}

pub struct SurfaceHost {
    tx: Sender<Cmd>,
    thread_id: u32,
}

impl SurfaceHost {
    pub fn start() -> Result<Self> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        std::thread::Builder::new()
            .name("driftwall-windows".into())
            .spawn(move || run(rx, ready_tx))?;
        let thread_id = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .context("window thread did not start")?;
        Ok(Self { tx, thread_id })
    }

    fn send(&self, c: Cmd) {
        if self.tx.send(c).is_ok() {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_HOST_WAKE, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// Creates a wallpaper window inside `parent`, at `rect` in the parent's client pixels.
    pub fn create(&self, parent: isize, rect: Rect) -> Result<isize> {
        let (reply, rx) = channel();
        self.send(Cmd::Create {
            parent,
            rect,
            reply,
        });
        rx.recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("window thread is not responding"))?
            .map_err(|e| anyhow!(e))
    }

    pub fn set_rect(&self, hwnd: isize, rect: Rect) {
        self.send(Cmd::Move { hwnd, rect });
    }

    pub fn destroy(&self, hwnd: isize) {
        self.send(Cmd::Destroy(hwnd));
    }
}

impl Drop for SurfaceHost {
    fn drop(&mut self) {
        self.send(Cmd::Quit);
    }
}

pub fn alive(hwnd: isize) -> bool {
    unsafe { IsWindow(Some(HWND(hwnd as *mut _))).as_bool() }
}

unsafe extern "system" fn surface_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            // The swap chain draws everything; never let GDI paint over it.
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn run(rx: Receiver<Cmd>, ready: Sender<u32>) {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(surface_proc),
            hInstance: instance.into(),
            lpszClassName: SURFACE_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&class);
        // Make sure the thread has a message queue before anyone posts to it.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        let _ = ready.send(GetCurrentThreadId());

        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                return;
            }
            if msg.hwnd.is_invalid() && msg.message == WM_HOST_WAKE {
                for c in rx.try_iter() {
                    match c {
                        Cmd::Create {
                            parent,
                            rect,
                            reply,
                        } => {
                            let res = CreateWindowExW(
                                WINDOW_EX_STYLE(0),
                                SURFACE_CLASS,
                                w!(""),
                                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_DISABLED,
                                rect.left,
                                rect.top,
                                rect.width(),
                                rect.height(),
                                Some(HWND(parent as *mut _)),
                                None,
                                Some(instance.into()),
                                None,
                            )
                            .map(|h| h.0 as isize)
                            .map_err(|e| format!("create wallpaper window: {e}"));
                            let _ = reply.send(res);
                        }
                        Cmd::Move { hwnd, rect } => {
                            let _ = SetWindowPos(
                                HWND(hwnd as *mut _),
                                None,
                                rect.left,
                                rect.top,
                                rect.width(),
                                rect.height(),
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                        }
                        Cmd::Destroy(hwnd) => {
                            let _ = DestroyWindow(HWND(hwnd as *mut _));
                        }
                        Cmd::Quit => return,
                    }
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

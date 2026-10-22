//! One video, decoded once on the GPU by the Media Foundation Media Engine and copied into a
//! swap chain on each display it's shown on ("frame-server" mode: decode -> GPU video
//! processor scale -> present). The render thread only wakes on display refresh, presents
//! only when the video has a new frame, and sleeps outright while paused.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{ID3D11RenderTargetView, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_MWA_NO_ALT_ENTER,
    DXGI_MWA_NO_WINDOW_CHANGES, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2,
    IDXGIOutput, IDXGISwapChain1,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFDXGIDeviceManager, IMFMediaEngine,
    IMFMediaEngineClassFactory, IMFMediaEngineEx, IMFMediaEngineNotify, IMFMediaEngineNotify_Impl,
    MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_DXGI_MANAGER, MF_MEDIA_ENGINE_EVENT,
    MF_MEDIA_ENGINE_EVENT_ERROR, MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA,
    MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, MF_MT_MAJOR_TYPE, MFARGB, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFMediaType_Audio, MFVideoNormalizedRect,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToGUID};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::{BSTR, Interface, PCWSTR, implement};

use crate::gpu::Gpu;
use crate::layout;
use crate::mf;
use crate::model::Fit;

/// A window to draw into, and its size in pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub hwnd: isize,
    pub width: u32,
    pub height: u32,
    /// Which display it belongs to (pausing is per display).
    pub display: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlayerConfig {
    pub file: PathBuf,
    pub fit: Fit,
    /// 0..=1. Zero turns the audio track off entirely.
    pub volume: f32,
    pub speed: f32,
    /// Frames per second from the file (0 when unknown); lets the render thread sleep between
    /// frames on displays much faster than the video.
    pub fps: f32,
}

enum Cmd {
    Targets(Vec<Target>),
    /// Display ids whose wallpaper should be drawing now; empty pauses the video.
    Active(Vec<String>),
    Fit(Fit),
    Volume(f32),
    Speed(f32),
    Stop(Sender<()>),
}

#[derive(Default)]
struct Shared {
    metadata: AtomicBool,
    failed: AtomicBool,
    error: Mutex<String>,
}

impl Shared {
    fn fail(&self, msg: String) {
        log::warn!("player: {msg}");
        *self.error.lock().unwrap() = msg;
        self.failed.store(true, Ordering::SeqCst);
    }
}

pub struct Player {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    pub fn start(gpu: Arc<Gpu>, config: PlayerConfig, targets: Vec<Target>) -> Player {
        let (tx, rx) = channel();
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("driftwall-player".into())
            .spawn(move || {
                if let Err(e) = run(gpu, config, targets, rx, s.clone()) {
                    s.fail(format!("{e:#}"));
                }
            })
            .expect("spawn player thread");
        Player {
            tx,
            shared,
            thread: Some(thread),
        }
    }

    pub fn set_targets(&self, t: Vec<Target>) {
        let _ = self.tx.send(Cmd::Targets(t));
    }

    pub fn set_active(&self, displays: Vec<String>) {
        let _ = self.tx.send(Cmd::Active(displays));
    }

    pub fn set_fit(&self, f: Fit) {
        let _ = self.tx.send(Cmd::Fit(f));
    }

    pub fn set_volume(&self, v: f32) {
        let _ = self.tx.send(Cmd::Volume(v));
    }

    pub fn set_speed(&self, s: f32) {
        let _ = self.tx.send(Cmd::Speed(s));
    }

    /// The reason playback stopped, if it did.
    pub fn failure(&self) -> Option<String> {
        self.shared
            .failed
            .load(Ordering::SeqCst)
            .then(|| self.shared.error.lock().unwrap().clone())
    }

    /// Stops playback and releases the swap chains (waits up to two seconds).
    pub fn stop(mut self) {
        let (tx, rx) = channel();
        if self.tx.send(Cmd::Stop(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
        if let Some(t) = self.thread.take() {
            // Join off-thread so a stuck Media Foundation shutdown can't hang the caller.
            std::thread::spawn(move || {
                let _ = t.join();
            });
        }
    }
}

#[implement(IMFMediaEngineNotify)]
struct Notify {
    shared: Arc<Shared>,
}

impl IMFMediaEngineNotify_Impl for Notify_Impl {
    fn EventNotify(&self, event: u32, param1: usize, param2: u32) -> windows::core::Result<()> {
        match MF_MEDIA_ENGINE_EVENT(event as i32) {
            MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA => {
                self.shared.metadata.store(true, Ordering::SeqCst)
            }
            MF_MEDIA_ENGINE_EVENT_ERROR => {
                let what = match param1 {
                    1 => "playback was aborted",
                    2 => "a network error",
                    3 => "the video could not be decoded",
                    4 => "the file or its format is not supported",
                    5 => "the file is encrypted",
                    _ => "an unknown error",
                };
                self.shared.fail(format!("{what} (0x{param2:08X})"));
            }
            _ => {}
        }
        Ok(())
    }
}

struct Surface {
    target: Target,
    swap: IDXGISwapChain1,
    back: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    output: Option<IDXGIOutput>,
    /// The display's refresh rate.
    refresh_hz: u32,
    active: bool,
}

/// Refresh rate of a DXGI output's display, from its current mode (60 when unknown).
fn refresh_rate(output: Option<&IDXGIOutput>) -> u32 {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    let Some(desc) = output.and_then(|o| unsafe { o.GetDesc() }.ok()) else {
        return 60;
    };
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let ok = unsafe {
        EnumDisplaySettingsW(
            PCWSTR(desc.DeviceName.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    };
    if ok.as_bool() && mode.dmDisplayFrequency > 1 {
        mode.dmDisplayFrequency
    } else {
        60
    }
}

/// How long to sleep before waiting for the next refresh, so a video with fewer frames than
/// the display has refreshes doesn't wake the thread on every refresh: until 1.5 refreshes
/// before the next frame is due. `until_due` is the time left to the next frame by the video's
/// own clock. Zero when the video is about as fast as the display.
fn idle_before_vblank(until_due: Duration, frame: Duration, refresh: Duration) -> Duration {
    if frame.is_zero() || refresh.is_zero() || frame < refresh * 3 {
        return Duration::ZERO;
    }
    until_due.min(frame).saturating_sub(refresh * 3 / 2)
}

fn make_surface(gpu: &Gpu, factory: &IDXGIFactory2, t: &Target) -> Result<Surface> {
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: t.width.max(1),
        Height: t.height.max(1),
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    let hwnd = HWND(t.hwnd as *mut _);
    unsafe {
        let swap = factory
            .CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None)
            .context("swap chain")?;
        let _ =
            factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER | DXGI_MWA_NO_WINDOW_CHANGES);
        // With flip-discard, buffer 0 is always the current back buffer.
        let back: ID3D11Texture2D = swap.GetBuffer(0)?;
        let mut rtv = None;
        gpu.device
            .CreateRenderTargetView(&back, None, Some(&mut rtv))?;
        let output = swap.GetContainingOutput().ok();
        let refresh_hz = refresh_rate(output.as_ref());
        log::debug!(
            "surface {}x{}: output {}, {refresh_hz} Hz",
            t.width,
            t.height,
            output
                .as_ref()
                .and_then(|o| o.GetDesc().ok())
                .map(|d| String::from_utf16_lossy(&d.DeviceName)
                    .trim_end_matches('\0')
                    .to_string())
                .unwrap_or_else(|| "none".into())
        );
        Ok(Surface {
            target: t.clone(),
            swap,
            back,
            rtv: rtv.context("render target")?,
            output,
            refresh_hz,
            active: false,
        })
    }
}

/// Waits for the next display refresh, or ~8 ms where that isn't available (hybrid-GPU
/// laptops whose render GPU has no outputs, displays that are off).
struct Pacer {
    timer: HANDLE,
    /// WaitForVBlank calls in a row that returned at once.
    instant_returns: std::cell::Cell<u32>,
}

impl Pacer {
    fn new() -> Self {
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
            .unwrap_or_default()
        };
        Pacer {
            timer,
            instant_returns: std::cell::Cell::new(0),
        }
    }

    /// Sleeps `d` on the high-resolution timer.
    fn sleep(&self, d: Duration) {
        let due = -((d.as_nanos() / 100) as i64).max(1);
        unsafe {
            if self.timer.is_invalid()
                || SetWaitableTimer(self.timer, &due, 0, None, None, false).is_err()
                || WaitForSingleObject(self.timer, INFINITE) != WAIT_OBJECT_0
            {
                std::thread::sleep(d);
            }
        }
    }

    fn wait(&self, output: Option<&IDXGIOutput>) {
        let start = Instant::now();
        if let Some(o) = output
            && unsafe { o.WaitForVBlank() }.is_ok()
        {
            // One quick return just means the refresh was about to happen; several in a row
            // mean there's no refresh to wait for (display asleep).
            if start.elapsed() >= Duration::from_micros(200) {
                self.instant_returns.set(0);
                return;
            }
            let n = self.instant_returns.get() + 1;
            self.instant_returns.set(n);
            if n < 3 {
                return;
            }
        }
        // No output, or no refreshes: fall back to a timer.
        if self.timer.is_invalid() {
            std::thread::sleep(Duration::from_millis(8));
            return;
        }
        let due: i64 = -80_000; // 8 ms, relative, in 100 ns units
        unsafe {
            if SetWaitableTimer(self.timer, &due, 0, None, None, false).is_ok() {
                if WaitForSingleObject(self.timer, INFINITE) != WAIT_OBJECT_0 {
                    std::thread::sleep(Duration::from_millis(8));
                }
            } else {
                std::thread::sleep(Duration::from_millis(8));
            }
        }
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        if !self.timer.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.timer);
            }
        }
    }
}

fn create_engine(
    gpu: &Gpu,
    shared: &Arc<Shared>,
    file: &Path,
) -> Result<(IMFMediaEngine, IMFDXGIDeviceManager)> {
    unsafe {
        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.context("DXGI device manager")?;
        manager.ResetDevice(&gpu.device, token)?;

        let notify: IMFMediaEngineNotify = Notify {
            shared: shared.clone(),
        }
        .into();
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 3)?;
        let attrs = attrs.context("attributes")?;
        attrs.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager)?;
        attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
        attrs.SetUINT32(
            &MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT,
            DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32,
        )?;

        let factory: IMFMediaEngineClassFactory =
            CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
        let engine = factory.CreateInstance(0, &attrs).context("Media Engine")?;
        engine.SetLoop(true)?;
        engine.SetAutoPlay(false)?;
        engine.SetMuted(true)?;
        let url = BSTR::from(file.to_string_lossy().as_ref());
        engine
            .SetSource(&url)
            .with_context(|| format!("open {}", file.display()))?;
        Ok((engine, manager))
    }
}

/// Turns audio tracks on or off. Off means no decoding and no audio device use at all.
fn select_audio(engine: &IMFMediaEngine, on: bool) {
    let Ok(ex) = engine.cast::<IMFMediaEngineEx>() else {
        return;
    };
    unsafe {
        let n = ex.GetNumberOfStreams().unwrap_or(0);
        let mut changed = false;
        for i in 0..n {
            let Ok(mut pv) = ex.GetStreamAttribute(i, &MF_MT_MAJOR_TYPE) else {
                continue;
            };
            let major = PropVariantToGUID(&pv).unwrap_or_default();
            let _ = PropVariantClear(&mut pv);
            if major == MFMediaType_Audio && ex.SetStreamSelection(i, on).is_ok() {
                changed = true;
            }
        }
        if changed {
            let _ = ex.ApplyStreamSelections();
        }
    }
}

fn apply_volume(engine: &IMFMediaEngine, v: f32) {
    unsafe {
        let _ = engine.SetVolume(v.clamp(0.0, 1.0) as f64);
        let _ = engine.SetMuted(v <= 0.0);
    }
}

fn is_device_lost(e: &windows::core::Error) -> bool {
    e.code() == DXGI_ERROR_DEVICE_REMOVED || e.code() == DXGI_ERROR_DEVICE_RESET
}

/// The presentation time (100 ns units) of a new frame, if OnVideoStreamTick reports one. The
/// wrapper maps S_FALSE ("no new frame") to Ok, so call through the vtable to see the result.
fn new_frame(engine: &IMFMediaEngine) -> Option<i64> {
    let mut pts = 0i64;
    let hr = unsafe { (Interface::vtable(engine).OnVideoStreamTick)(engine.as_raw(), &mut pts) };
    (hr.0 == 0).then_some(pts)
}

fn run(
    gpu: Arc<Gpu>,
    config: PlayerConfig,
    targets: Vec<Target>,
    rx: Receiver<Cmd>,
    shared: Arc<Shared>,
) -> Result<()> {
    mf::init_thread()?;
    let factory: IDXGIFactory2 = unsafe {
        gpu.device
            .cast::<IDXGIDevice>()?
            .GetAdapter()?
            .GetParent()?
    };
    let mut surfaces: Vec<Surface> = Vec::new();
    for t in &targets {
        surfaces.push(make_surface(&gpu, &factory, t)?);
    }
    let (engine, _manager) = create_engine(&gpu, &shared, &config.file)?;
    let mut fit = config.fit;
    let mut volume = config.volume;
    let mut speed = config.speed;
    let mut active: Vec<String> = Vec::new();
    let mut playing = false;
    let mut ready = false;
    let mut size = (0u32, 0u32);
    let pacer = Pacer::new();
    let black = MFARGB {
        rgbBlue: 0,
        rgbGreen: 0,
        rgbRed: 0,
        rgbAlpha: 255,
    };
    // DRIFTWALL_STATS=1 logs frames presented every 5 s (for measuring, off by default).
    let stats = std::env::var_os("DRIFTWALL_STATS").is_some();
    let (mut presented, mut wakes, mut since) = (0u32, 0u32, Instant::now());
    // Presentation time (seconds) of the last frame shown, and the time between frames.
    let mut last_pts: Option<f64> = None;
    let frame_secs = if config.fps > 0.0 {
        1.0 / config.fps as f64
    } else {
        0.0
    };

    let result = (|| -> Result<()> {
        loop {
            // Commands. While paused (or before the video has loaded), block here instead of
            // spinning.
            let wait = if ready && playing {
                Duration::ZERO
            } else {
                Duration::from_millis(if ready { 1000 } else { 10 })
            };
            let first = if wait.is_zero() {
                rx.try_recv().ok()
            } else {
                match rx.recv_timeout(wait) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
            };
            let mut cmds: Vec<Cmd> = first.into_iter().collect();
            cmds.extend(rx.try_iter());
            for c in cmds {
                match c {
                    Cmd::Stop(done) => {
                        surfaces.clear();
                        unsafe {
                            let _ = engine.Shutdown();
                        }
                        let _ = done.send(());
                        return Ok(());
                    }
                    Cmd::Targets(t) => {
                        surfaces.retain(|s| t.contains(&s.target));
                        for nt in t {
                            if !surfaces.iter().any(|s| s.target == nt) {
                                surfaces.push(make_surface(&gpu, &factory, &nt)?);
                            }
                        }
                        for s in &mut surfaces {
                            s.active = active.contains(&s.target.display);
                        }
                    }
                    Cmd::Active(a) => {
                        active = a;
                        for s in &mut surfaces {
                            s.active = active.contains(&s.target.display);
                        }
                    }
                    Cmd::Fit(f) => fit = f,
                    Cmd::Volume(v) => {
                        if ready && (volume <= 0.0) != (v <= 0.0) {
                            select_audio(&engine, v > 0.0);
                        }
                        volume = v;
                        apply_volume(&engine, v);
                    }
                    Cmd::Speed(s) => {
                        speed = s;
                        unsafe {
                            let _ = engine.SetDefaultPlaybackRate(s as f64);
                            let _ = engine.SetPlaybackRate(s as f64);
                        }
                    }
                }
            }
            if shared.failed.load(Ordering::SeqCst) {
                bail!("{}", shared.error.lock().unwrap());
            }

            if !ready {
                if !shared.metadata.load(Ordering::SeqCst) {
                    continue;
                }
                let (mut w, mut h) = (0u32, 0u32);
                unsafe { engine.GetNativeVideoSize(Some(&mut w), Some(&mut h))? };
                size = (w, h);
                select_audio(&engine, volume > 0.0);
                apply_volume(&engine, volume);
                unsafe {
                    let _ = engine.SetDefaultPlaybackRate(speed as f64);
                    let _ = engine.SetPlaybackRate(speed as f64);
                }
                ready = true;
                log::info!(
                    "playing {} ({w}x{h}) on {} display(s)",
                    config.file.display(),
                    surfaces.len()
                );
            }

            let want = surfaces.iter().any(|s| s.active);
            if want != playing {
                unsafe {
                    if want {
                        engine.Play()?
                    } else {
                        engine.Pause()?
                    }
                }
                playing = want;
                last_pts = None;
            }
            if !playing {
                continue;
            }

            let lead = surfaces.iter().find(|s| s.active);
            if let (Some(pts), Some(s)) = (last_pts, lead)
                && frame_secs > 0.0
            {
                let rate = (speed as f64).max(0.1);
                let now = unsafe { engine.GetCurrentTime() };
                // Wall-clock time until the next frame's presentation time.
                let until_due = ((pts + frame_secs - now) / rate).clamp(0.0, 1.0);
                let idle = idle_before_vblank(
                    Duration::from_secs_f64(until_due),
                    Duration::from_secs_f64(frame_secs / rate),
                    Duration::from_secs_f64(1.0 / s.refresh_hz as f64),
                );
                if !idle.is_zero() {
                    pacer.sleep(idle);
                }
            }
            pacer.wait(lead.and_then(|s| s.output.as_ref()));
            if stats {
                wakes += 1;
                if since.elapsed() >= Duration::from_secs(5) {
                    let secs = since.elapsed().as_secs_f32();
                    log::info!(
                        "stats: {:.1} frames/s presented, {:.0} wakes/s, {} of {} surfaces active",
                        presented as f32 / secs,
                        wakes as f32 / secs,
                        surfaces.iter().filter(|s| s.active).count(),
                        surfaces.len()
                    );
                    (presented, wakes, since) = (0, 0, Instant::now());
                }
            }
            let Some(pts) = new_frame(&engine) else {
                continue;
            };
            presented += 1;
            last_pts = Some(pts as f64 / 1e7);
            if size.0 == 0 {
                let (mut w, mut h) = (0u32, 0u32);
                unsafe { engine.GetNativeVideoSize(Some(&mut w), Some(&mut h))? };
                size = (w, h);
            }
            for s in surfaces.iter().filter(|s| s.active) {
                let (src, dst) =
                    layout::video_rects(fit, size.0, size.1, s.target.width, s.target.height);
                let src = MFVideoNormalizedRect {
                    left: src[0],
                    top: src[1],
                    right: src[2],
                    bottom: src[3],
                };
                let dst = RECT {
                    left: dst.left,
                    top: dst.top,
                    right: dst.right,
                    bottom: dst.bottom,
                };
                unsafe {
                    if matches!(fit, Fit::Fit | Fit::Center) {
                        gpu.context
                            .ClearRenderTargetView(&s.rtv, &[0.0, 0.0, 0.0, 1.0]);
                    }
                    if let Err(e) =
                        engine.TransferVideoFrame(&s.back, Some(&src), &dst, Some(&black))
                    {
                        if is_device_lost(&e) {
                            bail!("graphics device lost");
                        }
                        continue;
                    }
                    let hr = s.swap.Present(0, DXGI_PRESENT(0));
                    if hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET {
                        bail!("graphics device lost");
                    }
                }
            }
        }
    })();
    unsafe {
        let _ = engine.Shutdown();
    }
    result
}

/// Plays `file` without any window, copying frames into an off-screen texture: used by the
/// self-test and tests to check the decode path. Returns the BGRA centre pixel about every
/// 250 ms of playback.
pub fn decode_offscreen(
    gpu: &Gpu,
    file: &Path,
    width: u32,
    height: u32,
    run_for: Duration,
) -> Result<Vec<[u8; 4]>> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    };
    mf::init_thread()?;
    let shared = Arc::new(Shared::default());
    let (engine, _manager) = create_engine(gpu, &shared, file)?;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..desc
    };
    let mut samples = Vec::new();
    unsafe {
        let mut tex = None;
        gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        let tex = tex.context("texture")?;
        let mut staging = None;
        gpu.device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.context("staging")?;
        let start = Instant::now();
        while !shared.metadata.load(Ordering::SeqCst) {
            if shared.failed.load(Ordering::SeqCst) {
                bail!("{}", shared.error.lock().unwrap());
            }
            if start.elapsed() > Duration::from_secs(10) {
                bail!("video never loaded");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        engine.Play()?;
        let dst = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let black = MFARGB {
            rgbBlue: 0,
            rgbGreen: 0,
            rgbRed: 0,
            rgbAlpha: 255,
        };
        let mut last_sample = Instant::now() - Duration::from_secs(1);
        while start.elapsed() < run_for {
            if shared.failed.load(Ordering::SeqCst) {
                bail!("{}", shared.error.lock().unwrap());
            }
            std::thread::sleep(Duration::from_millis(4));
            if new_frame(&engine).is_none() {
                continue;
            }
            engine.TransferVideoFrame(&tex, None, &dst, Some(&black))?;
            if last_sample.elapsed() >= Duration::from_millis(250) {
                last_sample = Instant::now();
                gpu.context.CopyResource(&staging, &tex);
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                gpu.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
                let row = (height / 2) as usize * m.RowPitch as usize;
                let p = (m.pData as *const u8).add(row + (width / 2) as usize * 4);
                samples.push([*p, *p.add(1), *p.add(2), *p.add(3)]);
                gpu.context.Unmap(&staging, 0);
            }
        }
        let _ = engine.Shutdown();
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleeps_only_when_the_display_is_much_faster() {
        let ms = Duration::from_millis;
        let us = Duration::from_micros;
        // 60 fps video on a 60 Hz display: wait for every refresh.
        assert_eq!(
            idle_before_vblank(ms(15), us(16_667), us(16_667)),
            Duration::ZERO
        );
        // 30 fps on 60 Hz: still every refresh (less than 3 refreshes per frame).
        assert_eq!(
            idle_before_vblank(ms(30), us(33_333), us(16_667)),
            Duration::ZERO
        );
        // 30 fps on 240 Hz, next frame due in 31 ms: sleep until 1.5 refreshes before it.
        let idle = idle_before_vblank(ms(31), us(33_333), us(4_167));
        assert!((idle.as_secs_f64() - 0.024_750).abs() < 1e-4, "{idle:?}");
        // Next frame due soon: no sleep. A bogus far-off due time is capped at one frame.
        assert_eq!(
            idle_before_vblank(ms(3), us(33_333), us(4_167)),
            Duration::ZERO
        );
        assert!(idle_before_vblank(ms(900), us(33_333), us(4_167)) < us(33_333));
    }
}

//! D3D11 devices for video playback, one per graphics adapter, created on the adapter that
//! drives the display so frames never cross between GPUs.

use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP,
    D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice1, IDXGIFactory1,
};
use windows::core::Interface;

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    /// Adapter LUID, or 0 for the default/WARP device.
    pub luid: i64,
    pub adapter_name: String,
}

// Created with multithread protection: Media Foundation's threads and the render thread share
// the immediate context.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

static POOL: Mutex<Vec<Weak<Gpu>>> = Mutex::new(Vec::new());

fn create(
    adapter: Option<&IDXGIAdapter>,
    driver: D3D_DRIVER_TYPE,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
            ]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
    }
    Ok((device.context("no device")?, context.context("no context")?))
}

fn wrap(device: ID3D11Device, context: ID3D11DeviceContext, luid: i64) -> Result<Gpu> {
    unsafe {
        let mt: ID3D11Multithread = context.cast()?;
        let _ = mt.SetMultithreadProtected(true);
        let dxgi: IDXGIDevice1 = device.cast()?;
        // One queued frame is plenty for a wallpaper and keeps memory down.
        let _ = dxgi.SetMaximumFrameLatency(1);
        let adapter_name = dxgi
            .GetAdapter()
            .and_then(|a| a.GetDesc())
            .map(|d| {
                let end = d
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(d.Description.len());
                String::from_utf16_lossy(&d.Description[..end])
            })
            .unwrap_or_default();
        Ok(Gpu {
            device,
            context,
            luid,
            adapter_name,
        })
    }
}

/// The adapter whose outputs include `hmonitor`.
fn adapter_for(hmonitor: isize) -> Option<(IDXGIAdapter1, i64)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let mut j = 0;
            while let Ok(output) = adapter.EnumOutputs(j) {
                j += 1;
                if let Ok(desc) = output.GetDesc()
                    && desc.Monitor.0 as isize == hmonitor
                {
                    let d = adapter.GetDesc1().ok()?;
                    let luid =
                        ((d.AdapterLuid.HighPart as i64) << 32) | d.AdapterLuid.LowPart as i64;
                    return Some((adapter, luid));
                }
            }
        }
        None
    }
}

/// A shared device on the GPU driving `hmonitor` (falls back to the default adapter).
pub fn for_monitor(hmonitor: isize) -> Result<Arc<Gpu>> {
    let found = adapter_for(hmonitor);
    let luid = found.as_ref().map_or(0, |(_, l)| *l);
    let mut pool = POOL.lock().unwrap();
    pool.retain(|w| w.strong_count() > 0);
    if let Some(g) = pool
        .iter()
        .filter_map(Weak::upgrade)
        .find(|g| g.luid == luid)
    {
        return Ok(g);
    }
    let gpu = match found {
        Some((adapter, luid)) => {
            let a: IDXGIAdapter = adapter.cast()?;
            let (d, c) = create(Some(&a), D3D_DRIVER_TYPE_UNKNOWN)?;
            wrap(d, c, luid)?
        }
        None => default_device()?,
    };
    log::info!("GPU: {}", gpu.adapter_name);
    let gpu = Arc::new(gpu);
    pool.push(Arc::downgrade(&gpu));
    Ok(gpu)
}

/// The default hardware device, or WARP where there is no GPU (CI, some VMs).
pub fn default_device() -> Result<Gpu> {
    let (d, c) =
        create(None, D3D_DRIVER_TYPE_HARDWARE).or_else(|_| create(None, D3D_DRIVER_TYPE_WARP))?;
    wrap(d, c, 0)
}

/// Drops cached devices so the next request creates fresh ones (after a device loss).
pub fn reset_pool() {
    POOL.lock().unwrap().clear();
}

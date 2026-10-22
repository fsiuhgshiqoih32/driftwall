//! COM and Media Foundation start-up, and small helpers.

use std::sync::{Once, OnceLock};

use anyhow::{Result, bail};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoInitializeEx};

static MF_INIT: Once = Once::new();

/// Whether Media Foundation is installed. Windows N editions and some servers lack it; its
/// DLLs are delay-loaded (see build.rs), so this must be checked before any call into them.
pub fn available() -> bool {
    use windows::Win32::Foundation::FreeLibrary;
    use windows::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
    use windows::core::w;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        [
            w!("mfplat.dll"),
            w!("mfreadwrite.dll"),
            w!("mfmediaengine.dll"),
        ]
        .iter()
        .all(|dll| unsafe {
            match LoadLibraryExW(*dll, None, LOAD_LIBRARY_SEARCH_SYSTEM32) {
                Ok(h) => {
                    let _ = FreeLibrary(h);
                    true
                }
                Err(_) => false,
            }
        })
    })
}

pub const MISSING: &str = "Media Foundation is not installed, so videos can't play. On Windows N, add the Media Feature Pack in Settings > Apps > Optional features.";

/// Joins the multithreaded apartment and starts Media Foundation once per process.
pub fn init_thread() -> Result<()> {
    init_com_mta();
    if !available() {
        bail!(MISSING);
    }
    MF_INIT.call_once(|| unsafe {
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            log::error!("MFStartup failed: {e}");
        }
    });
    Ok(())
}

pub fn init_com_mta() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// For threads that own windows and talk to the shell.
pub fn init_com_sta() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// Packs two u32 values the way `MFSetAttributeSize`/`MFSetAttributeRatio` do.
pub fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

pub fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

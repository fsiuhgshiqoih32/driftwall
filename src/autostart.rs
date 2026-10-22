//! "Start with Windows": a value under HKCU\...\Run that starts the background process.

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::w;

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: windows::core::PCWSTR = w!("Driftwall");

fn command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" --background", exe.display()))
}

/// The command line currently registered, if any.
pub fn registered() -> Option<String> {
    unsafe {
        let mut len = 0u32;
        if RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut len),
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let mut buf = vec![0u16; len as usize / 2 + 1];
        if RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut len),
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

/// Registers or removes the entry. Re-registers when it points at another copy of the exe.
pub fn set(enabled: bool) {
    unsafe {
        if enabled {
            let Some(cmd) = command() else { return };
            if registered().as_deref() == Some(cmd.as_str()) {
                return;
            }
            let wide: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
            let r = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE,
                REG_SZ.0,
                Some(wide.as_ptr() as *const _),
                (wide.len() * 2) as u32,
            );
            if r != ERROR_SUCCESS {
                log::warn!("could not register start with Windows: {r:?}");
            }
        } else if registered().is_some() {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE);
        }
    }
}

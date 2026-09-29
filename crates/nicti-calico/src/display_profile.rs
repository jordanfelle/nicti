//! The active monitor's ICC profile (ADR-0042). Any failure — non-Windows, no profile assigned,
//! unreadable or unparseable file — degrades to sRGB with a logged reason (ADR-0101).

use crate::space::OutputSpace;
use crate::transform::DisplayProfile;
use moxcms::ColorProfile;
use std::sync::Arc;

/// Parses ICC bytes into a [`DisplayProfile`]; an unparseable profile is an `Err` with the
/// reason (the caller falls back to sRGB). A parseable profile that turns out unusable for RGB
/// display (gray, CMYK, no B2A table) fails later, in `DisplayTransform::build`.
pub fn from_icc_bytes(bytes: &[u8]) -> Result<DisplayProfile, String> {
    let profile =
        ColorProfile::new_from_slice(bytes).map_err(|e| format!("unparseable ICC: {e}"))?;
    Ok(DisplayProfile::Icc(Arc::new(profile)))
}

/// Identifies the monitor a window (native handle `hwnd`, `None` = no window yet) is on, so a
/// caller can detect a move to another monitor by comparing values frame to frame. Always `None`
/// off Windows.
pub fn current_monitor(hwnd: Option<isize>) -> Option<isize> {
    monitor_of(hwnd)
}

/// Resolves the display profile for the monitor the window `hwnd` is on (`None` = the primary
/// monitor), or sRGB with the reason it fell back returned alongside.
pub fn resolve(hwnd: Option<isize>) -> (DisplayProfile, Option<String>) {
    match read_profile_bytes(hwnd).and_then(|b| from_icc_bytes(&b)) {
        Ok(p) => (p, None),
        Err(reason) => (DisplayProfile::Space(OutputSpace::Srgb), Some(reason)),
    }
}

#[cfg(windows)]
fn monitor_of(hwnd: Option<isize>) -> Option<isize> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTOPRIMARY};
    // SAFETY: `MonitorFromWindow` only reads the handle value; an invalid/stale HWND makes it
    // fall back to the primary monitor (`MONITOR_DEFAULTTOPRIMARY`), never UB.
    let h = unsafe { MonitorFromWindow(HWND(hwnd.unwrap_or(0) as _), MONITOR_DEFAULTTOPRIMARY) };
    Some(h.0 as isize)
}

#[cfg(not(windows))]
fn monitor_of(_hwnd: Option<isize>) -> Option<isize> {
    None
}

#[cfg(windows)]
fn read_profile_bytes(hwnd: Option<isize>) -> Result<Vec<u8>, String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        CreateDCW, DeleteDC, GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFOEXW,
        MONITOR_DEFAULTTOPRIMARY,
    };
    use windows::Win32::UI::ColorSystem::GetICMProfileW;

    // SAFETY: plain Win32 calls on handles created and released within this function; the
    // buffers passed in outlive each call and their lengths are passed alongside.
    unsafe {
        let hmon: HMONITOR =
            MonitorFromWindow(HWND(hwnd.unwrap_or(0) as _), MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(hmon, &mut info as *mut _ as *mut _).as_bool() {
            return Err("GetMonitorInfoW failed".into());
        }
        let hdc = CreateDCW(
            windows::core::PCWSTR(info.szDevice.as_ptr()),
            windows::core::PCWSTR(info.szDevice.as_ptr()),
            windows::core::PCWSTR::null(),
            None,
        );
        if hdc.is_invalid() {
            return Err("CreateDCW failed".into());
        }
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = GetICMProfileW(hdc, &mut len, Some(PWSTR(buf.as_mut_ptr()))).as_bool();
        let _ = DeleteDC(hdc);
        if !ok {
            return Err("no ICC profile assigned to this monitor".into());
        }
        let path = String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]);
        std::fs::read(&path).map_err(|e| format!("reading {path}: {e}"))
    }
}

#[cfg(not(windows))]
fn read_profile_bytes(_hwnd: Option<isize>) -> Result<Vec<u8>, String> {
    Err("display profile lookup is Windows-only in v1".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_bytes_fall_back_to_srgb_with_a_reason() {
        assert!(from_icc_bytes(b"not an icc profile").is_err());
    }

    #[test]
    fn valid_profile_bytes_parse() {
        let bytes = crate::icc::profile_bytes(OutputSpace::DisplayP3).unwrap();
        assert!(matches!(from_icc_bytes(&bytes), Ok(DisplayProfile::Icc(_))));
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_resolves_to_srgb_with_reason() {
        let (p, reason) = resolve(None);
        assert!(matches!(p, DisplayProfile::Space(OutputSpace::Srgb)));
        assert!(reason.is_some());
    }
}

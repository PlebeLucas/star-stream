//! Lista o que dá pra transmitir: monitores e janelas abertas.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    /// "monitor" ou "window"
    pub kind: String,
    /// HMONITOR ou HWND
    pub handle: u64,
    pub name: String,
    pub detail: String,
    /// PID do programa dono da janela (pra sugerir "Só esse app" no áudio)
    pub pid: u32,
}

pub fn list() -> Vec<SourceInfo> {
    platform::list()
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub fn list() -> Vec<SourceInfo> {
        vec![]
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW, MONITORINFOF_PRIMARY,
    };

    unsafe extern "system" fn on_monitor(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<SourceInfo>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(h, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let primary = info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
            let n = out.len() + 1;
            out.push(SourceInfo {
                kind: "monitor".into(),
                handle: h.0 as u64,
                name: if primary { "Monitor principal".into() } else { format!("Monitor {n}") },
                detail: format!("{}×{}", r.right - r.left, r.bottom - r.top),
                pid: 0,
            });
        }
        true.into()
    }

    unsafe extern "system" fn on_window(h: HWND, data: LPARAM) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<SourceInfo>);
        if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return true.into();
        }
        if (GetWindowLongW(h, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW.0 != 0 {
            return true.into();
        }
        let len = GetWindowTextLengthW(h);
        if len <= 0 {
            return true.into();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(h, &mut buf);
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        if title == "Program Manager" || title.contains("STAR // STREAM") {
            return true.into();
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        out.push(SourceInfo { kind: "window".into(), handle: h.0 as u64, name: title, detail: "Janela".into(), pid });
        true.into()
    }

    pub fn list() -> Vec<SourceInfo> {
        let mut monitors: Vec<SourceInfo> = Vec::new();
        let mut windows: Vec<SourceInfo> = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(None, None, Some(on_monitor), LPARAM(&mut monitors as *mut _ as isize));
            let _ = EnumWindows(Some(on_window), LPARAM(&mut windows as *mut _ as isize));
        }
        // Principal primeiro
        monitors.sort_by_key(|m| m.name != "Monitor principal");
        monitors.extend(windows);
        monitors
    }
}

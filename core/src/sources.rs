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

/// Miniatura JPEG (como data URL) do que aparece na tela ou janela, cabendo
/// em max_w x max_h. None se não der pra capturar (ex.: janela minimizada).
pub fn thumbnail(kind: &str, handle: u64, max_w: u32, max_h: u32) -> Option<String> {
    let (w, h, bgra) = platform::grab(kind, handle, max_w.clamp(16, 1280), max_h.clamp(16, 720))?;
    let mut jpg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpg, 72)
        .encode(&bgra, w as u16, h as u16, jpeg_encoder::ColorType::Bgra)
        .ok()?;
    use base64::Engine;
    Some(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(jpg)))
}

/// Retângulo na tela (esquerda, topo, largura, altura) do que está sendo
/// capturado: o monitor inteiro ou a área interna da janela.
pub fn capture_rect(src: &crate::video::Source) -> Option<(i32, i32, i32, i32)> {
    platform::rect(src)
}

/// Tamanho atual (largura, altura) do monitor ou da janela, em pixels.
pub fn source_size(src: &crate::video::Source) -> Option<(u32, u32)> {
    platform::size(src)
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub fn list() -> Vec<SourceInfo> {
        vec![]
    }
    pub fn size(_s: &crate::video::Source) -> Option<(u32, u32)> {
        None
    }
    pub fn grab(_k: &str, _h: u64, _w: u32, _hh: u32) -> Option<(u32, u32, Vec<u8>)> {
        None
    }
    pub fn rect(_s: &crate::video::Source) -> Option<(i32, i32, i32, i32)> {
        None
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
        IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW, MONITORINFOF_PRIMARY, GetClientRect,
    };

    /// Copia a imagem atual (GDI), reduz e devolve (largura, altura, BGRA).
    pub fn grab(kind: &str, handle: u64, max_w: u32, max_h: u32) -> Option<(u32, u32, Vec<u8>)> {
        use windows::Win32::Graphics::Gdi::*;
        use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
        let src = match kind {
            "monitor" => crate::video::Source::Monitor { handle },
            "window" => crate::video::Source::Window { hwnd: handle },
            _ => return None,
        };
        let (sw, sh) = size(&src)?;
        // Tamanho da miniatura mantendo a proporção
        let scale = (max_w as f64 / sw as f64).min(max_h as f64 / sh as f64).min(1.0);
        let (tw, th) = (((sw as f64 * scale) as i32).max(1), ((sh as f64 * scale) as i32).max(1));
        unsafe {
            let screen = GetDC(None);
            let full_dc = CreateCompatibleDC(Some(screen));
            let full_bmp = CreateCompatibleBitmap(screen, sw as i32, sh as i32);
            let old_full = SelectObject(full_dc, full_bmp.into());
            let ok = if kind == "monitor" {
                let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                let _ = GetMonitorInfoW(HMONITOR(handle as _), &mut info);
                let r = info.rcMonitor;
                BitBlt(full_dc, 0, 0, sw as i32, sh as i32, Some(screen), r.left, r.top, SRCCOPY | CAPTUREBLT).is_ok()
            } else {
                // 1 = só a área interna, 2 = funciona com janelas aceleradas por GPU
                PrintWindow(HWND(handle as _), full_dc, PRINT_WINDOW_FLAGS(3)).as_bool()
            };
            let thumb_dc = CreateCompatibleDC(Some(screen));
            let thumb_bmp = CreateCompatibleBitmap(screen, tw, th);
            let old_thumb = SelectObject(thumb_dc, thumb_bmp.into());
            SetStretchBltMode(thumb_dc, HALFTONE);
            let _ = SetBrushOrgEx(thumb_dc, 0, 0, None);
            let _ = StretchBlt(thumb_dc, 0, 0, tw, th, Some(full_dc), 0, 0, sw as i32, sh as i32, SRCCOPY);
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: tw,
                    biHeight: -th, // de cima pra baixo
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut buf = vec![0u8; (tw * th * 4) as usize];
            let lines = GetDIBits(thumb_dc, thumb_bmp, 0, th as u32, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
            SelectObject(thumb_dc, old_thumb);
            SelectObject(full_dc, old_full);
            let _ = DeleteObject(thumb_bmp.into());
            let _ = DeleteObject(full_bmp.into());
            let _ = DeleteDC(thumb_dc);
            let _ = DeleteDC(full_dc);
            ReleaseDC(None, screen);
            if !ok || lines == 0 {
                return None;
            }
            for px in buf.chunks_exact_mut(4) {
                px[3] = 255; // GDI deixa o alfa zerado
            }
            Some((tw as u32, th as u32, buf))
        }
    }

    pub fn rect(src: &crate::video::Source) -> Option<(i32, i32, i32, i32)> {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::ClientToScreen;
        unsafe {
            match src {
                crate::video::Source::Monitor { handle } => {
                    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                    if !GetMonitorInfoW(HMONITOR(*handle as _), &mut info).as_bool() {
                        return None;
                    }
                    let r = info.rcMonitor;
                    Some((r.left, r.top, r.right - r.left, r.bottom - r.top))
                }
                crate::video::Source::Window { hwnd } => {
                    let h = HWND(*hwnd as _);
                    let mut r = RECT::default();
                    GetClientRect(h, &mut r).ok()?;
                    let mut p = POINT { x: 0, y: 0 };
                    if !ClientToScreen(h, &mut p).as_bool() {
                        return None;
                    }
                    Some((p.x, p.y, r.right - r.left, r.bottom - r.top))
                }
                crate::video::Source::Webcam { .. } => None,
            }
        }
    }

    pub fn size(src: &crate::video::Source) -> Option<(u32, u32)> {
        unsafe {
            let r = match src {
                crate::video::Source::Monitor { handle } => {
                    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                    if !GetMonitorInfoW(HMONITOR(*handle as _), &mut info).as_bool() {
                        return None;
                    }
                    info.rcMonitor
                }
                crate::video::Source::Window { hwnd } => {
                    let mut r = RECT::default();
                    GetClientRect(HWND(*hwnd as _), &mut r).ok()?;
                    r
                }
                crate::video::Source::Webcam { .. } => return None,
            };
            let (w, h) = ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32);
            (w > 0 && h > 0).then_some((w, h))
        }
    }

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

    /// Mesma regra do Alt+Tab: só janelas de verdade, visíveis pro usuário.
    unsafe fn is_alt_tab_window(h: HWND) -> bool {
        use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
        use windows::Win32::UI::WindowsAndMessaging::{
            GetAncestor, GetClassNameW, GetLastActivePopup, GetWindow, GA_ROOTOWNER, GW_OWNER, WS_EX_APPWINDOW,
            WS_EX_NOACTIVATE,
        };
        if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return false;
        }
        // "Escondida" pelo Windows: apps como Configurações ficam abertos e invisíveis
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4).is_ok() && cloaked != 0 {
            return false;
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_APPWINDOW.0 == 0 {
            if ex & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0 {
                return false;
            }
            // Janelas "filhas" de outra (diálogos, popups) ficam de fora
            if GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
                return false;
            }
            // Regra do Alt+Tab: a janela tem que ser a ponta da sua cadeia de popups
            let root = GetAncestor(h, GA_ROOTOWNER);
            if !root.is_invalid() && GetLastActivePopup(root) != h && root != h {
                return false;
            }
        }
        // Janelas internas de apps da loja (o quadro de fora é que aparece)
        let mut cls = [0u16; 128];
        let n = GetClassNameW(h, &mut cls);
        let cls = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
        if matches!(cls.as_str(), "Windows.UI.Core.CoreWindow" | "Progman" | "WorkerW" | "Shell_TrayWnd") {
            return false;
        }
        // Tamanho mínimo: ignora janelas fantasmas de 0 a poucos pixels
        let mut r = RECT::default();
        if GetClientRect(h, &mut r).is_err() || r.right - r.left < 120 || r.bottom - r.top < 80 {
            return false;
        }
        true
    }

    unsafe extern "system" fn on_window(h: HWND, data: LPARAM) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<SourceInfo>);
        if !is_alt_tab_window(h) {
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
        if pid == std::process::id() {
            return true.into();
        }
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

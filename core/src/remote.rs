//! Controle remoto: um amigo que assiste move o mouse e usa o teclado do PC de
//! quem transmite, DENTRO da área transmitida.
//!
//! Regras de segurança:
//! - só funciona depois que quem transmite aceita (a cada pedido);
//! - um amigo por vez, com tempo limite;
//! - Ctrl+Alt+X (no app) corta na hora;
//! - tecla Windows e atalhos do sistema são bloqueados;
//! - ao cortar, solta qualquer tecla/botão que estava apertado.

use crate::video::Source;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

struct Grant {
    peer: String,
    until: Instant,
    source: Source,
}

pub struct Remote {
    grant: Mutex<Option<Grant>>,
    held_keys: Mutex<HashSet<u16>>,
    held_buttons: Mutex<HashSet<u8>>,
    /// Avisa quando o controle termina: (quem, motivo)
    ended: mpsc::UnboundedSender<(String, String)>,
}

/// Teclas que nunca passam: Windows (esq./dir.), menu de apps
const BLOCKED_VK: [u16; 3] = [0x5B, 0x5C, 0x5D];

impl Remote {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<(String, String)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                grant: Mutex::new(None),
                held_keys: Mutex::new(HashSet::new()),
                held_buttons: Mutex::new(HashSet::new()),
                ended: tx,
            },
            rx,
        )
    }

    pub fn grant(&self, peer: &str, minutes: u64, source: Source) {
        self.release_all();
        log::info!(target: "star::remote", "controle liberado para {peer} por {minutes} min");
        *self.grant.lock().unwrap() =
            Some(Grant { peer: peer.into(), until: Instant::now() + Duration::from_secs(minutes.clamp(1, 60) * 60), source });
    }

    pub fn controller(&self) -> Option<String> {
        self.grant.lock().unwrap().as_ref().map(|g| g.peer.clone())
    }

    /// Corta o controle (se houver). Devolve quem estava controlando.
    pub fn revoke(&self, reason: &str) -> Option<String> {
        let g = self.grant.lock().unwrap().take();
        self.release_all();
        if let Some(g) = &g {
            log::info!(target: "star::remote", "controle de {} encerrado: {reason}", g.peer);
            let _ = self.ended.send((g.peer.clone(), reason.into()));
        }
        g.map(|g| g.peer)
    }

    /// Chamado a cada segundo: encerra se o tempo acabou.
    pub fn check_expiry(&self) {
        let expired = self.grant.lock().unwrap().as_ref().is_some_and(|g| Instant::now() >= g.until);
        if expired {
            self.revoke("o tempo acabou");
        }
    }

    /// Um evento de mouse/teclado vindo de quem assiste.
    pub fn handle(&self, peer: &str, v: &Value) {
        let (rect, ok) = {
            let g = self.grant.lock().unwrap();
            match g.as_ref() {
                Some(g) if g.peer == peer && Instant::now() < g.until => (crate::sources::capture_rect(&g.source), true),
                _ => (None, false),
            }
        };
        if !ok {
            return;
        }
        let num = |k: &str| v.get(k).and_then(Value::as_f64);
        let pos = || -> Option<(i32, i32)> {
            let (l, t, w, h) = rect?;
            let x = num("x")?.clamp(0.0, 1.0);
            let y = num("y")?.clamp(0.0, 1.0);
            Some((l + (x * (w - 1).max(0) as f64).round() as i32, t + (y * (h - 1).max(0) as f64).round() as i32))
        };
        match v.get("k").and_then(Value::as_str).unwrap_or("") {
            "m" => {
                if let Some((x, y)) = pos() {
                    platform::mouse_move(x, y);
                }
            }
            "d" | "u" => {
                let b = num("b").unwrap_or(0.0) as u8;
                if b > 2 {
                    return;
                }
                if let Some((x, y)) = pos() {
                    platform::mouse_move(x, y);
                }
                let down = v["k"] == "d";
                platform::mouse_button(b, down);
                let mut held = self.held_buttons.lock().unwrap();
                if down {
                    held.insert(b);
                } else {
                    held.remove(&b);
                }
            }
            "w" => {
                let dy = num("dy").unwrap_or(0.0).clamp(-2000.0, 2000.0);
                platform::wheel((-dy / 100.0 * 120.0) as i32);
            }
            "kd" | "ku" => {
                let vk = num("vk").unwrap_or(0.0) as u16;
                if vk == 0 || vk > 0xFE || BLOCKED_VK.contains(&vk) {
                    return;
                }
                let down = v["k"] == "kd";
                platform::key(vk, down);
                let mut held = self.held_keys.lock().unwrap();
                if down {
                    held.insert(vk);
                } else {
                    held.remove(&vk);
                }
            }
            _ => {}
        }
    }

    /// Solta tudo que o amigo deixou apertado.
    fn release_all(&self) {
        for vk in self.held_keys.lock().unwrap().drain() {
            platform::key(vk, false);
        }
        for b in self.held_buttons.lock().unwrap().drain() {
            platform::mouse_button(b, false);
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn mouse_move(_x: i32, _y: i32) {}
    pub fn mouse_button(_b: u8, _down: bool) {}
    pub fn wheel(_d: i32) {}
    pub fn key(_vk: u16, _down: bool) {}
}

#[cfg(windows)]
mod platform {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    fn send(inputs: &[INPUT]) {
        unsafe {
            SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        }
    }

    pub fn mouse_move(x: i32, y: i32) {
        unsafe {
            let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
            let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2), GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2));
            let nx = ((x - vx) as i64 * 65535 / (vw - 1) as i64) as i32;
            let ny = ((y - vy) as i64 * 65535 / (vh - 1) as i64) as i32;
            send(&[mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
        }
    }

    pub fn mouse_button(b: u8, down: bool) {
        let f = match (b, down) {
            (0, true) => MOUSEEVENTF_LEFTDOWN,
            (0, false) => MOUSEEVENTF_LEFTUP,
            (1, true) => MOUSEEVENTF_MIDDLEDOWN,
            (1, false) => MOUSEEVENTF_MIDDLEUP,
            (2, true) => MOUSEEVENTF_RIGHTDOWN,
            _ => MOUSEEVENTF_RIGHTUP,
        };
        send(&[mouse(0, 0, 0, f)]);
    }

    pub fn wheel(d: i32) {
        if d != 0 {
            send(&[mouse(0, 0, d, MOUSEEVENTF_WHEEL)]);
        }
    }

    /// Setas, Insert/Delete/Home/End/PgUp/PgDn, Ctrl/Alt da direita etc.
    fn extended(vk: u16) -> bool {
        matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x6F | 0x90 | 0xA3 | 0xA5)
    }

    pub fn key(vk: u16, down: bool) {
        unsafe {
            let scan = MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16;
            let mut flags = KEYBD_EVENT_FLAGS(0);
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            if extended(vk) {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            send(&[INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
                },
            }]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn so_quem_foi_liberado_controla() {
        let (r, mut rx) = Remote::new();
        assert_eq!(r.controller(), None);
        r.grant("amigo", 5, Source::Monitor { handle: 1 });
        assert_eq!(r.controller().as_deref(), Some("amigo"));
        // outro peer é ignorado (não deve travar nem registrar tecla)
        r.handle("intruso", &json!({"k": "kd", "vk": 65}));
        assert!(r.held_keys.lock().unwrap().is_empty());
        // o liberado registra tecla apertada; tecla Windows é bloqueada
        r.handle("amigo", &json!({"k": "kd", "vk": 65}));
        r.handle("amigo", &json!({"k": "kd", "vk": 0x5B}));
        assert_eq!(r.held_keys.lock().unwrap().len(), 1);
        // cortar solta tudo e avisa
        assert_eq!(r.revoke("teste").as_deref(), Some("amigo"));
        assert!(r.held_keys.lock().unwrap().is_empty());
        assert_eq!(rx.try_recv().unwrap(), ("amigo".to_string(), "teste".to_string()));
        assert_eq!(r.controller(), None);
    }

    #[test]
    fn expira() {
        let (r, mut rx) = Remote::new();
        r.grant("amigo", 1, Source::Monitor { handle: 1 });
        r.grant.lock().unwrap().as_mut().unwrap().until = Instant::now() - Duration::from_secs(1);
        r.check_expiry();
        assert_eq!(r.controller(), None);
        assert_eq!(rx.try_recv().unwrap().1, "o tempo acabou");
    }
}

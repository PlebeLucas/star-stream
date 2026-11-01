//! Interação ao vivo do lado de quem transmite:
//! - camada transparente por cima da tela (pings, reações, aviso de controle),
//!   que NÃO entra na captura;
//! - clipes;
//! - controle remoto (liberar, cortar, atalho Ctrl+Alt+X).

use crate::{err, AppState};
use star_core::remote::Remote;
use star_core::sources;
use star_core::video::Source;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};

pub const OVERLAY: &str = "overlay";

/// Cria (uma vez) a janela transparente por cima de tudo.
fn ensure_overlay(app: &tauri::AppHandle) -> Result<tauri::WebviewWindow, String> {
    if let Some(w) = app.get_webview_window(OVERLAY) {
        return Ok(w);
    }
    // Precisa usar os mesmos argumentos do navegador da janela principal
    let args = app.config().app.windows.first().and_then(|w| w.additional_browser_args.clone());
    let mut b = tauri::WebviewWindowBuilder::new(app, OVERLAY, tauri::WebviewUrl::App("overlay.html".into()))
        .title("STAR // STREAM overlay")
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .visible(false);
    if let Some(a) = args {
        b = b.additional_browser_args(&a);
    }
    let w = b.build().map_err(err)?;
    let _ = w.set_ignore_cursor_events(true);
    exclude_from_capture(&w);
    Ok(w)
}

/// A camada não aparece na transmissão (senão quem assiste veria o ping duplicado).
fn exclude_from_capture(w: &tauri::WebviewWindow) {
    #[cfg(windows)]
    if let Ok(h) = w.hwnd() {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};
        unsafe {
            let _ = SetWindowDisplayAffinity(HWND(h.0 as _), WDA_EXCLUDEFROMCAPTURE);
        }
    }
    #[cfg(not(windows))]
    let _ = w;
}

fn place_overlay(w: &tauri::WebviewWindow, src: &Source) -> bool {
    match sources::capture_rect(src) {
        Some((x, y, cw, ch)) if cw > 0 && ch > 0 => {
            let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
            let _ = w.set_size(tauri::PhysicalSize::new(cw as u32, ch as u32));
            true
        }
        _ => false,
    }
}

/// Mostra a camada em cima do que está sendo transmitido e acompanha a janela se ela mexer.
#[tauri::command]
pub async fn overlay_show(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<bool, String> {
    let Some(live) = state.live.lock().await.clone() else { return Ok(false) };
    let src = live.source();
    if matches!(src, Source::Webcam { .. }) {
        return Ok(false);
    }
    let w = ensure_overlay(&app)?;
    if !place_overlay(&w, &src) {
        return Ok(false);
    }
    let _ = w.show();
    let _ = w.set_ignore_cursor_events(true);
    // Segue a janela/monitor enquanto a live durar
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let st = app2.state::<AppState>();
            let alive = st.live.lock().await.as_ref().is_some_and(|l| Arc::ptr_eq(l, &live));
            let Some(w) = app2.get_webview_window(OVERLAY) else { break };
            if !alive {
                let _ = w.hide();
                break;
            }
            if !place_overlay(&w, &src) {
                let _ = w.hide();
            } else if !w.is_visible().unwrap_or(true) {
                let _ = w.show();
            }
        }
    });
    Ok(true)
}

#[tauri::command]
pub fn overlay_hide(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window(OVERLAY) {
        let _ = w.hide();
    }
}

/// Mensagem pra todo mundo que assiste.
#[tauri::command]
pub async fn hub_broadcast(state: State<'_, AppState>, msg: serde_json::Value) -> Result<(), String> {
    if let Some(l) = state.live.lock().await.clone() {
        l.broadcast(&msg).await;
    }
    Ok(())
}

/// Mensagem pra um espectador.
#[tauri::command]
pub async fn hub_send(state: State<'_, AppState>, peer: String, msg: serde_json::Value) -> Result<(), String> {
    if let Some(l) = state.live.lock().await.clone() {
        l.send_to(&peer, &msg).await;
    }
    Ok(())
}

fn clips_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path()
        .video_dir()
        .or_else(|_| app.path().home_dir())
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("STAR Stream")
}

/// Salva os últimos 30 s. `name` vem da interface (data/hora local + quem pediu).
#[tauri::command]
pub async fn clip_save(app: tauri::AppHandle, state: State<'_, AppState>, name: String) -> Result<String, String> {
    let Some(l) = state.live.lock().await.clone() else { return Err("não tem live rolando".into()) };
    let safe: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(80)
        .collect();
    let out = clips_dir(&app).join(format!("{safe}.mp4"));
    l.clip(30, &out).await.map_err(|e| {
        log::warn!(target: "star", "clipe falhou: {e:#}");
        err(e)
    })?;
    log::info!(target: "star", "clipe salvo: {}", out.display());
    Ok(out.display().to_string())
}

#[tauri::command]
pub fn open_clips_folder(app: tauri::AppHandle) -> Result<(), String> {
    let d = clips_dir(&app);
    let _ = std::fs::create_dir_all(&d);
    tauri_plugin_opener::open_path(d, None::<&str>).map_err(err)
}

/// Libera o controle pra um espectador (depois que quem transmite aceitou).
#[tauri::command]
pub async fn remote_grant(
    state: State<'_, AppState>,
    remote: State<'_, Arc<Remote>>,
    peer: String,
    minutes: u64,
) -> Result<(), String> {
    let Some(l) = state.live.lock().await.clone() else { return Err("não tem live rolando".into()) };
    let src = l.source();
    if matches!(src, Source::Webcam { .. }) {
        return Err("controle remoto não funciona transmitindo a webcam".into());
    }
    remote.grant(&peer, minutes, src);
    Ok(())
}

#[tauri::command]
pub fn remote_revoke(remote: State<'_, Arc<Remote>>, reason: Option<String>) {
    remote.revoke(reason.as_deref().unwrap_or("quem transmite cortou"));
}

/// Tarefas de fundo: fim do controle vira evento pra interface; tempo limite;
/// atalho global Ctrl+Alt+X pra cortar na hora.
pub fn start_background(app: &tauri::AppHandle, remote: Arc<Remote>, mut ended: tokio::sync::mpsc::UnboundedReceiver<(String, String)>) {
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some((peer, reason)) = ended.recv().await {
            let _ = app2.emit("remote-ended", serde_json::json!({ "peer": peer, "reason": reason }));
        }
    });
    let r = remote.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            r.check_expiry();
        }
    });
    #[cfg(windows)]
    {
        let r = remote;
        std::thread::Builder::new()
            .name("star-hotkey".into())
            .spawn(move || unsafe {
                use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT};
                use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};
                if RegisterHotKey(None, 1, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'X' as u32).is_err() {
                    log::warn!(target: "star", "não consegui registrar Ctrl+Alt+X");
                    return;
                }
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_HOTKEY {
                        r.revoke("cortado com Ctrl+Alt+X");
                    }
                }
            })
            .ok();
    }
}

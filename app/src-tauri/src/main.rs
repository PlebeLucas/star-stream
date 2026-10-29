// STAR // STREAM: app desktop (Tauri). A interface fica em ../src;
// aqui ficam só os comandos que ela chama e a ponte com o núcleo.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
use star_core::audio::{list_audio_apps, AudioApp};
use star_core::hub::{HubEvent, IceServer, Signal};
use star_core::session::{LiveConfig, LiveSession};
use star_core::sources::{self, SourceInfo};
use star_core::video::{detect_encoder, Encoder};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};

#[derive(Default)]
struct AppState {
    live: Mutex<Option<Arc<LiveSession>>>,
    encoder: Mutex<Option<Encoder>>,
}

fn ffmpeg_path(app: &tauri::AppHandle) -> PathBuf {
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Ok(dir) = app.path().resource_dir() {
        let p = dir.join("bin").join(exe);
        if p.exists() {
            return p;
        }
    }
    // Desenvolvimento: ffmpeg no PATH
    PathBuf::from(exe)
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Endereço do servidor de encontro embutido na hora do build (pode ser vazio).
#[tauri::command]
fn default_signal_url() -> String {
    option_env!("STAR_SIGNAL_URL").unwrap_or("").to_string()
}

/// Login com Discord: abre o navegador e espera o servidor devolver o token
/// numa porta local temporária.
#[tauri::command]
async fn login(app: tauri::AppHandle, signal_url: String) -> Result<String, String> {
    use rand::distr::{Alphanumeric, SampleString};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(err)?;
    let port = listener.local_addr().map_err(err)?.port();
    let nonce = Alphanumeric.sample_string(&mut rand::rng(), 32);
    let url = format!("{}/login?port={port}&nonce={nonce}", signal_url.trim_end_matches('/'));
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(err)?;

    let wait = async {
        loop {
            let (mut sock, _) = listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await?;
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
            let q = path.split_once('?').map(|x| x.1).unwrap_or("");
            let param = |k: &str| {
                q.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}=")).map(|v| v.to_string()))
            };
            let ok = path.starts_with("/callback") && param("nonce").as_deref() == Some(nonce.as_str());
            let body = if ok {
                "<meta charset=utf-8><body style='background:#07080A;color:#F2F4F5;font-family:sans-serif;display:grid;place-items:center;height:100vh'><div>Login feito. Pode fechar esta aba e voltar pro STAR // STREAM.</div>"
            } else {
                "<meta charset=utf-8>Pedido inválido."
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            if ok {
                if let Some(t) = param("token") {
                    return Ok::<String, std::io::Error>(t);
                }
            }
        }
    };
    let token = tokio::time::timeout(std::time::Duration::from_secs(300), wait)
        .await
        .map_err(|_| "o login demorou demais, tente de novo".to_string())?
        .map_err(err)?;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_focus();
    }
    Ok(token)
}

#[tauri::command]
fn list_sources() -> Vec<SourceInfo> {
    sources::list()
}

#[tauri::command]
fn audio_apps() -> Vec<AudioApp> {
    list_audio_apps()
}

#[derive(Serialize)]
struct EncoderInfo {
    id: Encoder,
    label: String,
}

#[tauri::command]
async fn encoder_info(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<EncoderInfo, String> {
    let mut cached = state.encoder.lock().await;
    let enc = match *cached {
        Some(e) => e,
        None => {
            let e = detect_encoder(&ffmpeg_path(&app)).await;
            *cached = Some(e);
            e
        }
    };
    Ok(EncoderInfo { id: enc, label: enc.label().into() })
}

#[tauri::command]
async fn go_live(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    config: LiveConfig,
    ice: Vec<IceServer>,
) -> Result<(), String> {
    if let Some(old) = state.live.lock().await.take() {
        old.stop().await;
    }
    let enc = encoder_info(app.clone(), state.clone()).await?.id;
    let (tx, mut rx) = mpsc::unbounded_channel::<HubEvent>();
    let session = LiveSession::start(config, ffmpeg_path(&app), enc, ice, tx).await.map_err(err)?;
    *state.live.lock().await = Some(session);
    // Eventos do núcleo vão pra interface (que encaminha os sinais pelo servidor)
    let app2 = app.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let _ = app2.emit("hub", ev);
        }
    });
    Ok(())
}

#[tauri::command]
async fn stop_live(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(s) = state.live.lock().await.take() {
        s.stop().await;
    }
    Ok(())
}

#[tauri::command]
async fn set_volume(state: State<'_, AppState>, volume: u32) -> Result<(), String> {
    if let Some(s) = state.live.lock().await.as_ref() {
        s.set_volume(volume);
    }
    Ok(())
}

/// Sinal de um amigo que chegou pelo servidor de encontro.
#[tauri::command]
async fn signal_in(state: State<'_, AppState>, from: String, data: Signal) -> Result<(), String> {
    let live = state.live.lock().await.clone();
    match live {
        Some(s) => s.on_signal(&from, data).await.map_err(err),
        None => Ok(()),
    }
}

#[tauri::command]
async fn viewer_gone(state: State<'_, AppState>, peer: String) -> Result<(), String> {
    if let Some(s) = state.live.lock().await.clone() {
        s.viewer_gone(&peer).await;
    }
    Ok(())
}

/// Procura versão nova no GitHub e instala sozinho (os amigos nunca reinstalam).
async fn auto_update(app: tauri::AppHandle) {
    use tauri_plugin_updater::UpdaterExt;
    let Ok(updater) = app.updater() else { return };
    match updater.check().await {
        Ok(Some(update)) => {
            let _ = app.emit("update", &update.version);
            if update.download_and_install(|_, _| {}, || {}).await.is_ok() {
                app.restart();
            }
        }
        Ok(None) => {}
        Err(e) => log::warn!("checagem de atualização falhou: {e}"),
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(auto_update(handle));
            Ok(())
        })
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            default_signal_url,
            login,
            list_sources,
            audio_apps,
            encoder_info,
            go_live,
            stop_live,
            set_volume,
            signal_in,
            viewer_gone
        ])
        .run(tauri::generate_context!())
        .expect("erro ao iniciar o STAR // STREAM");
}

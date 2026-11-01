// STAR // STREAM: app desktop (Tauri). A interface fica em ../src;
// aqui ficam só os comandos que ela chama e a ponte com o núcleo.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod diag;
mod interact;

use serde::Serialize;
use star_core::audio::{list_audio_apps, AudioApp};
use star_core::hub::{HubEvent, IceServer, Signal};
use star_core::session::{LiveConfig, LiveSession};
use star_core::sources::{self, SourceInfo};
use star_core::video::{detect_encoder, list_webcams, webcam_snapshot, Encoder};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};

#[derive(Default)]
pub struct AppState {
    /// Link starstream:// que abriu o app, esperando a interface pegar
    pending_url: std::sync::Mutex<Option<String>>,
    pub live: Mutex<Option<Arc<LiveSession>>>,
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

pub fn err(e: impl std::fmt::Display) -> String {
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

/// Miniatura de uma tela ou janela (data URL JPEG), pra prévia na escolha.
#[tauri::command]
async fn source_thumb(kind: String, handle: u64, w: u32, h: u32) -> Option<String> {
    tokio::task::spawn_blocking(move || sources::thumbnail(&kind, handle, w, h)).await.ok().flatten()
}

/// Webcams conectadas.
#[tauri::command]
async fn webcams(app: tauri::AppHandle) -> Vec<String> {
    list_webcams(&ffmpeg_path(&app)).await
}

/// Uma foto da webcam pra prévia (liga a câmera por ~1 s).
#[tauri::command]
async fn webcam_thumb(app: tauri::AppHandle, name: String) -> Option<String> {
    webcam_snapshot(&ffmpeg_path(&app), &name).await
}

/// A interface pega o link que abriu o app (se houver) quando fica pronta.
#[tauri::command]
fn take_pending_url(state: State<'_, AppState>) -> Option<String> {
    state.pending_url.lock().unwrap().take()
}

fn handle_open_url(app: &tauri::AppHandle, url: String) {
    if !url.starts_with("starstream://") {
        return;
    }
    log::info!(target: "star", "aberto por link: {url}");
    if let Some(st) = app.try_state::<AppState>() {
        *st.pending_url.lock().unwrap() = Some(url.clone());
    }
    let _ = app.emit("open-url", url);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
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
    remote: State<'_, Arc<star_core::remote::Remote>>,
    config: LiveConfig,
    ice: Vec<IceServer>,
) -> Result<(), String> {
    log::info!(target: "star", "go_live: {config:?}");
    if let Some(old) = state.live.lock().await.take() {
        old.stop().await;
    }
    let enc = encoder_info(app.clone(), state.clone()).await?.id;
    log::info!(target: "star", "encoder: {enc:?}, ffmpeg: {}", ffmpeg_path(&app).display());
    let (tx, mut rx) = mpsc::unbounded_channel::<HubEvent>();
    remote.revoke("a live recomeçou");
    let session = LiveSession::start(config, ffmpeg_path(&app), enc, ice, tx, Some(remote.inner().clone())).await.map_err(|e| {
        log::error!(target: "star", "go_live falhou: {e:#}");
        err(e)
    })?;
    log::info!(target: "star", "go_live: transmissão iniciada");
    *state.live.lock().await = Some(session);
    // Eventos do núcleo vão pra interface (que encaminha os sinais pelo servidor)
    let app2 = app.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if !matches!(ev, HubEvent::Stats { .. }) {
                log::info!(target: "star::evento", "{ev:?}");
            }
            let _ = app2.emit("hub", ev);
        }
    });
    Ok(())
}

#[tauri::command]
async fn stop_live(app: tauri::AppHandle, state: State<'_, AppState>, remote: State<'_, Arc<star_core::remote::Remote>>) -> Result<(), String> {
    remote.revoke("a live terminou");
    interact::overlay_hide(app);
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

/// Erros e eventos da interface também vão pro arquivo de diagnóstico.
#[tauri::command]
fn js_log(level: String, message: String) {
    let msg: String = message.chars().take(2000).collect();
    match level.as_str() {
        "error" => log::error!(target: "star::ui", "{msg}"),
        "warn" => log::warn!(target: "star::ui", "{msg}"),
        _ => log::info!(target: "star::ui", "{msg}"),
    }
}

/// Abre a pasta com o arquivo de diagnóstico.
#[tauri::command]
fn open_log_folder() -> Result<(), String> {
    tauri_plugin_opener::open_path(diag::log_dir(), None::<&str>).map_err(err)
}

fn main() {
    diag::init();
    tauri::Builder::default()
        // Um app só: clicar num link com o app aberto manda o link pro que já está aberto
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            use tauri_plugin_deep_link::DeepLinkExt;
            // Garante que links starstream:// abrem este app (registro do usuário, sem admin)
            if let Err(e) = app.deep_link().register_all() {
                log::warn!(target: "star", "registro do link falhou: {e}");
            }
            let h = app.handle().clone();
            app.deep_link().on_open_url(move |ev| {
                for u in ev.urls() {
                    handle_open_url(&h, u.to_string());
                }
            });
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                for u in urls {
                    handle_open_url(app.handle(), u.to_string());
                }
            }
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(auto_update(handle));
            let (remote, ended) = star_core::remote::Remote::new();
            let remote = Arc::new(remote);
            interact::start_background(app.handle(), remote.clone(), ended);
            app.manage(remote);
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
            viewer_gone,
            open_log_folder,
            js_log,
            source_thumb,
            take_pending_url,
            webcams,
            webcam_thumb,
            interact::overlay_show,
            interact::overlay_hide,
            interact::hub_broadcast,
            interact::hub_send,
            interact::clip_save,
            interact::open_clips_folder,
            interact::remote_grant,
            interact::remote_revoke
        ])
        .build(tauri::generate_context!())
        .expect("erro ao iniciar o STAR // STREAM")
        .run(|_app, ev| {
            if let tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit = ev {
                log::info!(target: "star", "app encerrando normalmente ({ev:?})");
            }
        });
}

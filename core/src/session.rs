//! Uma transmissão ao vivo: liga FFmpeg + áudio + mini servidor e desliga tudo junto.

use crate::audio::{AudioCapture, AudioMode, Gain, OpusPacker};
use crate::hub::{Hub, HubEvent, IceServer, Signal};
use crate::video::{ffmpeg_args, Encoder, Preset, Source, VideoProcess};
use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, Clone, Deserialize)]
pub struct LiveConfig {
    pub source: Source,
    pub preset: Preset,
    pub audio: AudioMode,
    /// 0 a 150 (%)
    pub volume: u32,
}

pub struct LiveSession {
    hub: Arc<Hub>,
    video: Mutex<Option<VideoProcess>>,
    audio: std::sync::Mutex<Option<AudioCapture>>,
    gain: Gain,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl LiveSession {
    pub async fn start(
        cfg: LiveConfig,
        ffmpeg: PathBuf,
        encoder: Encoder,
        ice: Vec<IceServer>,
        events: mpsc::UnboundedSender<HubEvent>,
    ) -> Result<Arc<Self>> {
        log::info!(target: "star::session", "1/4 criando mini servidor");
        let hub = Hub::new(ice, events.clone())?;
        let port = hub.start_video_ingest().await?;

        // --- vídeo ---
        log::info!(target: "star::session", "2/4 iniciando captura de vídeo");
        let src_size = crate::sources::source_size(&cfg.source);
        log::info!(target: "star::session", "tamanho da fonte: {src_size:?}");
        let args = ffmpeg_args(&cfg.source, src_size, cfg.preset, encoder, port);
        log::info!(target: "star::session", "ffmpeg {}", args.join(" "));
        let mut video = VideoProcess::spawn(&ffmpeg, &args).await?;
        let fps = Arc::new(AtomicU32::new(0));
        let mut tasks = Vec::new();

        if let Some(out) = video.child.stdout.take() {
            let fps = fps.clone();
            tasks.push(tokio::spawn(async move {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if let Some(v) = l.strip_prefix("fps=") {
                        if let Ok(f) = v.trim().parse::<f32>() {
                            fps.store(f.to_bits(), Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        let last_err = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        if let Some(err) = video.child.stderr.take() {
            let ev = events.clone();
            let last_err = last_err.clone();
            tasks.push(tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    log::warn!(target: "star::ffmpeg", "{l}");
                    let mut last = last_err.lock().unwrap();
                    last.push(l);
                    if last.len() > 6 {
                        last.remove(0);
                    }
                }
                log::warn!(target: "star::session", "FFmpeg encerrou");
                let msg = last_err.lock().unwrap().join(" | ");
                let _ = ev.send(HubEvent::Error { message: format!("A captura de vídeo parou. {msg}").trim().to_string() });
            }));
        }

        // Espera o FFmpeg mostrar que está vivo antes de seguir: se ele cair logo
        // de cara, a interface recebe o erro em vez de "ao vivo" com tela preta.
        for _ in 0..15 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(Some(status)) = video.child.try_wait() {
                tokio::time::sleep(Duration::from_millis(100)).await; // deixa o stderr terminar de chegar
                let msg = last_err.lock().unwrap().join(" | ");
                log::error!(target: "star::session", "FFmpeg saiu na largada ({status}): {msg}");
                for t in &tasks {
                    t.abort();
                }
                hub.close_all().await;
                anyhow::bail!("a captura de vídeo não iniciou: {msg}");
            }
            if fps.load(Ordering::Relaxed) != 0 {
                break;
            }
        }
        log::info!(target: "star::session", "captura de vídeo rodando");

        // --- áudio ---
        log::info!(target: "star::session", "3/4 iniciando captura de som: {:?}", cfg.audio);
        let gain = Gain::new(cfg.volume as f32 / 100.0);
        let (atx, mut arx) = mpsc::unbounded_channel::<bytes::Bytes>();
        let mut packer = OpusPacker::new(160_000)?;
        let g = gain.clone();
        let audio = match AudioCapture::start(
            cfg.audio,
            Box::new(move |samples| {
                for p in packer.push(samples, g.get()) {
                    let _ = atx.send(p);
                }
            }),
        ) {
            Ok(a) => a,
            Err(e) => {
                // Sem áudio a transmissão continua; só avisa.
                let _ = events.send(HubEvent::Error { message: format!("{e}. Transmitindo sem som.") });
                None
            }
        };
        {
            let hub = hub.clone();
            tasks.push(tokio::spawn(async move {
                let mut n = 0u32;
                while let Some(p) = arx.recv().await {
                    if n < 2 {
                        log::debug!(target: "star::session", "áudio: pacote Opus {n} ({} bytes)", p.len());
                    }
                    hub.write_audio(p).await;
                    if n < 2 {
                        log::debug!(target: "star::session", "áudio: pacote {n} enviado");
                    }
                    n = n.saturating_add(1);
                }
            }));
        }

        log::info!(target: "star::session", "4/4 tudo ligado");
        // --- telemetria a cada segundo ---
        {
            let hub = hub.clone();
            let ev = events.clone();
            let fps = fps.clone();
            tasks.push(tokio::spawn(async move {
                let mut last = Instant::now();
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let s = hub.stats(last.elapsed()).await;
                    last = Instant::now();
                    let _ = ev.send(HubEvent::Stats {
                        fps: f32::from_bits(fps.load(Ordering::Relaxed)),
                        kbps: s.video_kbps,
                        viewers: s.viewers,
                        encoder: encoder.label().to_string(),
                    });
                }
            }));
        }

        Ok(Arc::new(Self {
            hub,
            video: Mutex::new(Some(video)),
            audio: std::sync::Mutex::new(audio),
            gain,
            tasks,
        }))
    }

    pub async fn on_signal(&self, from: &str, sig: Signal) -> Result<()> {
        self.hub.on_signal(from, sig).await
    }

    pub async fn viewer_gone(&self, peer: &str) {
        self.hub.remove_viewer(peer).await;
    }

    pub fn set_volume(&self, percent: u32) {
        self.gain.set(percent as f32 / 100.0);
    }

    pub async fn stop(&self) {
        for t in &self.tasks {
            t.abort();
        }
        self.hub.close_all().await;
        if let Some(mut v) = self.video.lock().await.take() {
            v.stop().await;
        }
        // Soltar a captura encerra a thread de áudio
        let a = self.audio.lock().unwrap().take();
        drop(a);
    }
}

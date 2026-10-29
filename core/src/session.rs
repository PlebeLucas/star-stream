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
        let hub = Hub::new(ice, events.clone())?;
        let port = hub.start_video_ingest().await?;

        // --- vídeo ---
        let args = ffmpeg_args(&cfg.source, cfg.preset, encoder, port);
        log::info!("ffmpeg {}", args.join(" "));
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
        if let Some(err) = video.child.stderr.take() {
            let ev = events.clone();
            tasks.push(tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                let mut last = Vec::<String>::new();
                while let Ok(Some(l)) = lines.next_line().await {
                    log::warn!("ffmpeg: {l}");
                    last.push(l);
                    if last.len() > 6 {
                        last.remove(0);
                    }
                }
                // stderr fechou = FFmpeg saiu
                let _ = ev.send(HubEvent::Error {
                    message: format!("a captura de vídeo parou. {}", last.join(" | ")).trim().to_string(),
                });
            }));
        }

        // --- áudio ---
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
                while let Some(p) = arx.recv().await {
                    hub.write_audio(p).await;
                }
            }));
        }

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

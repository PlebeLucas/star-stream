//! Uma transmissão ao vivo: liga FFmpeg + áudio + mini servidor e desliga tudo junto.
//! Se quem assiste começa a perder pacotes, baixa a qualidade sozinho.

use crate::audio::{AudioCapture, AudioMode, Gain, OpusPacker};
use crate::hub::{Hub, HubEvent, IceServer, Signal};
use crate::video::{ffmpeg_args, Encoder, Preset, Source, VideoProcess};
use anyhow::Result;
use serde::Deserialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct LiveConfig {
    pub source: Source,
    pub preset: Preset,
    pub audio: AudioMode,
    /// 0 a 150 (%)
    pub volume: u32,
    /// Baixar a qualidade sozinho se quem assiste estiver perdendo pacotes
    #[serde(default = "yes")]
    pub auto_quality: bool,
}

/// Perda acima disso num relatório conta como "ruim" (13/256 ≈ 5%)
const BAD_LOSS: u8 = 13;
/// Relatórios ruins dentro da janela que disparam a troca
const BAD_COUNT: usize = 3;
const WINDOW: Duration = Duration::from_secs(8);
/// Depois de trocar, espera a nova qualidade assentar
const COOLDOWN: Duration = Duration::from_secs(20);

/// Captura de vídeo em execução + tarefas que leem a saída dela.
struct Video {
    proc: VideoProcess,
    tasks: Vec<JoinHandle<()>>,
}

struct VideoCtx {
    ffmpeg: PathBuf,
    encoder: Encoder,
    source: Source,
    port: u16,
    fps: Arc<AtomicU32>,
    events: mpsc::UnboundedSender<HubEvent>,
    /// true enquanto a gente mesmo está reiniciando o FFmpeg (não é erro)
    restarting: Arc<AtomicBool>,
}

impl VideoCtx {
    /// Liga o FFmpeg na qualidade pedida e espera ele mostrar que está vivo.
    async fn launch(&self, preset: Preset) -> Result<Video> {
        let src_size = crate::sources::source_size(&self.source);
        log::info!(target: "star::session", "vídeo: {preset:?}, fonte {src_size:?}");
        let args = ffmpeg_args(&self.source, src_size, preset, self.encoder, self.port);
        log::info!(target: "star::session", "ffmpeg {}", args.join(" "));
        let mut proc = VideoProcess::spawn(&self.ffmpeg, &args).await?;
        self.fps.store(0, Ordering::Relaxed);
        let mut tasks = Vec::new();

        if let Some(out) = proc.child.stdout.take() {
            let fps = self.fps.clone();
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
        if let Some(err) = proc.child.stderr.take() {
            let ev = self.events.clone();
            let last_err = last_err.clone();
            let restarting = self.restarting.clone();
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
                if !restarting.load(Ordering::Relaxed) {
                    let msg = last_err.lock().unwrap().join(" | ");
                    let _ = ev.send(HubEvent::Error { message: format!("A captura de vídeo parou. {msg}").trim().to_string() });
                }
            }));
        }

        // Se o FFmpeg cair logo de cara, devolve o erro em vez de "ao vivo" com tela preta.
        for _ in 0..15 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(Some(status)) = proc.child.try_wait() {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let msg = last_err.lock().unwrap().join(" | ");
                log::error!(target: "star::session", "FFmpeg saiu na largada ({status}): {msg}");
                for t in &tasks {
                    t.abort();
                }
                anyhow::bail!("a captura de vídeo não iniciou: {msg}");
            }
            if self.fps.load(Ordering::Relaxed) != 0 {
                break;
            }
        }
        log::info!(target: "star::session", "captura de vídeo rodando");
        Ok(Video { proc, tasks })
    }
}

async fn stop_video(v: Video) {
    let Video { mut proc, tasks } = v;
    proc.stop().await;
    for t in tasks {
        t.abort();
    }
}

pub struct LiveSession {
    hub: Arc<Hub>,
    ctx: Arc<VideoCtx>,
    /// Qualidade atual (muda com o ajuste automático)
    preset: Arc<std::sync::Mutex<Preset>>,
    video: Arc<Mutex<Option<Video>>>,
    audio: std::sync::Mutex<Option<AudioCapture>>,
    gain: Gain,
    tasks: Vec<JoinHandle<()>>,
}

impl LiveSession {
    pub async fn start(
        cfg: LiveConfig,
        ffmpeg: PathBuf,
        encoder: Encoder,
        ice: Vec<IceServer>,
        events: mpsc::UnboundedSender<HubEvent>,
        remote: Option<Arc<crate::remote::Remote>>,
    ) -> Result<Arc<Self>> {
        log::info!(target: "star::session", "1/4 criando mini servidor");
        let hub = Hub::new(ice, events.clone())?;
        if let Some(r) = remote {
            hub.set_remote(r);
        }
        let port = hub.start_video_ingest(cfg.preset.spec().kbps).await?;
        let mut loss_rx = hub.subscribe_loss();

        // --- vídeo ---
        log::info!(target: "star::session", "2/4 iniciando captura de vídeo");
        let ctx = Arc::new(VideoCtx {
            ffmpeg,
            encoder,
            source: cfg.source.clone(),
            port,
            fps: Arc::new(AtomicU32::new(0)),
            events: events.clone(),
            restarting: Arc::new(AtomicBool::new(false)),
        });
        let first = match ctx.launch(cfg.preset).await {
            Ok(v) => v,
            Err(e) => {
                hub.close_all().await;
                return Err(e);
            }
        };
        let video = Arc::new(Mutex::new(Some(first)));
        let current = Arc::new(std::sync::Mutex::new(cfg.preset));
        let mut tasks = Vec::new();

        // --- ajuste automático de qualidade ---
        {
            let ctx = ctx.clone();
            let video = video.clone();
            let hub = hub.clone();
            let ev = events.clone();
            let auto = cfg.auto_quality;
            let mut preset = cfg.preset;
            let current = current.clone();
            tasks.push(tokio::spawn(async move {
                let mut bad: VecDeque<Instant> = VecDeque::new();
                let mut last_change = Instant::now();
                while let Some((who, loss)) = loss_rx.recv().await {
                    if loss < BAD_LOSS {
                        continue;
                    }
                    let now = Instant::now();
                    bad.push_back(now);
                    while bad.front().is_some_and(|t| now.duration_since(*t) > WINDOW) {
                        bad.pop_front();
                    }
                    log::info!(target: "star::session", "perda alta em {who}: {:.1}% ({} em {}s)", loss as f64 / 2.56, bad.len(), WINDOW.as_secs());
                    if !auto || bad.len() < BAD_COUNT || last_change.elapsed() < COOLDOWN {
                        continue;
                    }
                    let Some(lower) = preset.lower() else { continue };
                    log::warn!(target: "star::session", "baixando qualidade: {preset:?} -> {lower:?}");
                    ctx.restarting.store(true, Ordering::Relaxed);
                    let mut slot = video.lock().await;
                    if let Some(old) = slot.take() {
                        stop_video(old).await;
                    }
                    hub.set_pace_kbps(lower.spec().kbps);
                    match ctx.launch(lower).await {
                        Ok(v) => {
                            *slot = Some(v);
                            preset = lower;
                            *current.lock().unwrap() = lower;
                            let _ = ev.send(HubEvent::Quality {
                                preset: lower,
                                reason: "a conexão de quem assiste estava perdendo pacotes".into(),
                            });
                        }
                        Err(e) => {
                            // Não deu na menor: tenta voltar pra anterior
                            log::error!(target: "star::session", "troca de qualidade falhou: {e:#}");
                            hub.set_pace_kbps(preset.spec().kbps);
                            match ctx.launch(preset).await {
                                Ok(v) => *slot = Some(v),
                                Err(e) => {
                                    let _ = ev.send(HubEvent::Error { message: format!("A captura de vídeo parou: {e}") });
                                }
                            }
                        }
                    }
                    ctx.restarting.store(false, Ordering::Relaxed);
                    drop(slot);
                    bad.clear();
                    last_change = Instant::now();
                }
            }));
        }

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
                while let Some(p) = arx.recv().await {
                    hub.write_audio(p).await;
                }
            }));
        }

        log::info!(target: "star::session", "4/4 tudo ligado");
        // --- telemetria a cada segundo ---
        {
            let hub = hub.clone();
            let ev = events.clone();
            let fps = ctx.fps.clone();
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

        Ok(Arc::new(Self { hub, ctx, preset: current, video, audio: std::sync::Mutex::new(audio), gain, tasks }))
    }

    pub async fn on_signal(&self, from: &str, sig: Signal) -> Result<()> {
        self.hub.on_signal(from, sig).await
    }

    pub async fn viewer_gone(&self, peer: &str) {
        self.hub.remove_viewer(peer).await;
    }

    /// O que está sendo transmitido (pra ping e controle remoto)
    pub fn source(&self) -> Source {
        self.ctx.source.clone()
    }

    pub async fn broadcast(&self, msg: &serde_json::Value) {
        self.hub.broadcast(msg).await;
    }

    pub async fn send_to(&self, peer: &str, msg: &serde_json::Value) {
        self.hub.send_to(peer, msg).await;
    }

    /// Salva os últimos `secs` segundos da live em `out` (MP4).
    pub async fn clip(&self, secs: u64, out: &std::path::Path) -> Result<()> {
        let preset = *self.preset.lock().unwrap();
        let fps = if matches!(self.ctx.source, Source::Webcam { .. }) { preset.spec().fps.min(30) } else { preset.spec().fps };
        self.hub.save_clip(&self.ctx.ffmpeg, fps, secs, out).await
    }

    pub fn set_volume(&self, percent: u32) {
        self.gain.set(percent as f32 / 100.0);
    }

    pub async fn stop(&self) {
        for t in &self.tasks {
            t.abort();
        }
        self.hub.close_all().await;
        if let Some(v) = self.video.lock().await.take() {
            stop_video(v).await;
        }
        // Soltar a captura encerra a thread de áudio
        let a = self.audio.lock().unwrap().take();
        drop(a);
    }
}

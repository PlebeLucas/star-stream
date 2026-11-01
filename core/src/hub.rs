//! O "mini servidor" embutido: recebe o vídeo do FFmpeg e o áudio do PC
//! e manda uma cópia pra cada amigo conectado, direto PC→PC (WebRTC).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use crate::remote::Remote;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp::packet::Packet;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::util::Unmarshal;

/// Formato que o navegador do amigo vai receber.
const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640034";

/// Servidor ICE como chega do servidor de encontro.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

/// Mensagens trocadas com o amigo (passam pelo servidor de encontro).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Signal {
    Join,
    Leave,
    Offer { sdp: String },
    Answer { sdp: String },
    Ice { candidate: RTCIceCandidateInit },
}

/// Algo que o núcleo precisa mandar pra fora (pra interface encaminhar).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubEvent {
    Signal { to: String, data: Signal },
    ViewerState { peer: String, state: String },
    Stats { fps: f32, kbps: u64, viewers: usize, encoder: String },
    Error { message: String },
    /// Mensagem do canal de interação vinda de um espectador
    Interaction { peer: String, msg: serde_json::Value },
    /// Qualidade baixada automaticamente por causa de perdas
    Quality { preset: crate::video::Preset, reason: String },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HubStats {
    pub video_kbps: u64,
    pub viewers: usize,
}

pub struct Hub {
    api: webrtc::api::API,
    ice: Vec<RTCIceServer>,
    video: Arc<TrackLocalStaticRTP>,
    audio: Arc<TrackLocalStaticSample>,
    peers: Mutex<HashMap<String, Arc<RTCPeerConnection>>>,
    events: mpsc::UnboundedSender<HubEvent>,
    video_bytes: Arc<AtomicU64>,
    /// Ritmo de envio do vídeo (pacer); muda junto com a qualidade
    pace_kbps: Arc<std::sync::atomic::AtomicU32>,
    /// Relatórios de perda dos espectadores (quem: id, perda 0-255)
    loss_tx: std::sync::Mutex<Option<mpsc::UnboundedSender<(String, u8)>>>,
    /// Canal de interação (reações, ping, clipe, controle) com cada espectador
    channels: Mutex<HashMap<String, Arc<RTCDataChannel>>>,
    /// Quem pode receber comandos de mouse/teclado
    remote: std::sync::Mutex<Option<Arc<Remote>>>,
    /// Últimos segundos de vídeo e áudio, pro "clipa isso"
    clip: Arc<std::sync::Mutex<ClipBuf>>,
}

impl Hub {
    pub fn new(ice: Vec<IceServer>, events: mpsc::UnboundedSender<HubEvent>) -> Result<Arc<Self>> {
        let mut m = MediaEngine::default();
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line: H264_FMTP.to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 102,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)?;
        let api = APIBuilder::new().with_media_engine(m).with_interceptor_registry(registry).build();

        let video = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                sdp_fmtp_line: H264_FMTP.to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "star".to_owned(),
        ));
        let audio = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                ..Default::default()
            },
            "audio".to_owned(),
            "star".to_owned(),
        ));

        // O lado Rust só usa STUN e TURN por UDP; TURN por TCP/TLS fica pro navegador do amigo.
        let ice = ice
            .into_iter()
            .filter_map(|s| {
                let urls: Vec<String> = s
                    .urls
                    .into_iter()
                    .filter(|u| u.starts_with("stun:") || (u.starts_with("turn:") && !u.contains("transport=tcp")))
                    .collect();
                (!urls.is_empty()).then(|| RTCIceServer {
                    urls,
                    username: s.username,
                    credential: s.credential,
                    ..Default::default()
                })
            })
            .collect();

        Ok(Arc::new(Self {
            api,
            ice,
            video,
            audio,
            peers: Mutex::new(HashMap::new()),
            events,
            video_bytes: Arc::new(AtomicU64::new(0)),
            pace_kbps: Arc::new(std::sync::atomic::AtomicU32::new(10_000)),
            loss_tx: std::sync::Mutex::new(None),
            channels: Mutex::new(HashMap::new()),
            remote: std::sync::Mutex::new(None),
            clip: Arc::new(std::sync::Mutex::new(ClipBuf::default())),
        }))
    }

    /// Abre a porta local onde o FFmpeg vai jogar o vídeo e começa a repassar.
    /// Devolve a porta pra montar o comando do FFmpeg.
    /// `kbps` é o bitrate do preset: os pacotes saem espalhados no tempo (pacer)
    /// em vez de em rajada, que é o que fazia a internet de quem assiste perder pedaços.
    /// Liga o controle remoto (mouse/teclado vindo de quem assiste).
    pub fn set_remote(&self, r: Arc<Remote>) {
        *self.remote.lock().unwrap() = Some(r);
    }

    /// Manda uma mensagem de interação pra todo mundo que assiste.
    pub async fn broadcast(&self, msg: &serde_json::Value) {
        let text = msg.to_string();
        let chans: Vec<_> = self.channels.lock().await.values().cloned().collect();
        for dc in chans {
            let _ = dc.send_text(text.clone()).await;
        }
    }

    /// Manda uma mensagem de interação pra um espectador.
    pub async fn send_to(&self, peer: &str, msg: &serde_json::Value) {
        let dc = self.channels.lock().await.get(peer).cloned();
        if let Some(dc) = dc {
            let _ = dc.send_text(msg.to_string()).await;
        }
    }

    /// Salva os últimos `secs` segundos em `out` (MP4). Precisa do FFmpeg pra juntar vídeo e áudio.
    pub async fn save_clip(&self, ffmpeg: &std::path::Path, fps: u32, secs: u64, out: &std::path::Path) -> Result<()> {
        let (video, audio) = self.clip.lock().unwrap().snapshot(Duration::from_secs(secs));
        if video.is_empty() {
            return Err(anyhow!("ainda não tem vídeo suficiente pra um clipe"));
        }
        crate::clip::write_mp4(ffmpeg, fps, video, audio, out).await
    }

    pub fn set_pace_kbps(&self, kbps: u32) {
        self.pace_kbps.store(kbps, Ordering::Relaxed);
    }

    /// Recebe os relatórios de perda de cada espectador (pra ajuste automático).
    pub fn subscribe_loss(&self) -> mpsc::UnboundedReceiver<(String, u8)> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.loss_tx.lock().unwrap() = Some(tx);
        rx
    }

    pub async fn start_video_ingest(self: &Arc<Self>, kbps: u32) -> Result<u16> {
        self.set_pace_kbps(kbps);
        let sock = UdpSocket::bind("127.0.0.1:0").await?;
        let port = sock.local_addr()?.port();
        let (tx, mut rx) = mpsc::channel::<Packet>(4096);
        let hub = self.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let mut stitch = Stitch::default();
            loop {
                let n = match sock.recv(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => break,
                };
                hub.video_bytes.fetch_add(n as u64, Ordering::Relaxed);
                let mut raw = &buf[..n];
                if let Ok(mut pkt) = Packet::unmarshal(&mut raw) {
                    if stitch.apply(&mut pkt) {
                        // FFmpeg reiniciou (outra qualidade): o clipe recomeça daqui
                        hub.clip.lock().unwrap().clear();
                    }
                    hub.clip.lock().unwrap().push_video(pkt.clone());
                    if tx.try_send(pkt).is_err() {
                        log::warn!(target: "star::hub", "fila de vídeo cheia, descartando pacote");
                    }
                }
            }
        });
        let hub = self.clone();
        tokio::spawn(async move {
            // Envia no máximo 2x o bitrate; rajada máxima ~48 KB
            let pace = hub.pace_kbps.clone();
            let burst = 48_000.0f64;
            let mut tokens = burst;
            let mut last = Instant::now();
            while let Some(pkt) = rx.recv().await {
                let rate = (pace.load(Ordering::Relaxed).max(500) as f64) * 1000.0 / 8.0 * 2.0; // bytes/s
                let size = (pkt.payload.len() + 12) as f64;
                loop {
                    let now = Instant::now();
                    tokens = (tokens + now.duration_since(last).as_secs_f64() * rate).min(burst);
                    last = now;
                    if tokens >= size {
                        break;
                    }
                    let wait = (size - tokens) / rate;
                    tokio::time::sleep(Duration::from_secs_f64(wait.max(0.001))).await;
                }
                tokens -= size;
                if let Err(e) = hub.video.write_rtp(&pkt).await {
                    log::debug!(target: "star::hub", "vídeo: {e}");
                }
            }
        });
        Ok(port)
    }

    /// Manda 20 ms de áudio Opus já codificado.
    pub async fn write_audio(&self, opus: bytes::Bytes) {
        self.clip.lock().unwrap().push_audio(opus.clone());
        let _ = self
            .audio
            .write_sample(&Sample { data: opus, duration: Duration::from_millis(20), ..Default::default() })
            .await;
    }

    /// Lê e zera o contador de bytes de vídeo (pra mostrar o bitrate real).
    pub async fn stats(&self, elapsed: Duration) -> HubStats {
        let bytes = self.video_bytes.swap(0, Ordering::Relaxed);
        let secs = elapsed.as_secs_f64().max(0.001);
        HubStats { video_kbps: ((bytes as f64 * 8.0) / 1000.0 / secs) as u64, viewers: self.peers.lock().await.len() }
    }

    /// Chegou um sinal de um amigo.
    pub async fn on_signal(self: &Arc<Self>, from: &str, sig: Signal) -> Result<()> {
        match sig {
            Signal::Join => self.add_viewer(from).await,
            Signal::Answer { sdp } => {
                let pc = self.peer(from).await?;
                pc.set_remote_description(RTCSessionDescription::answer(sdp)?).await?;
                Ok(())
            }
            Signal::Ice { candidate } => {
                log::info!(target: "star::hub", "candidato de {from}: {}", candidate.candidate);
                let pc = self.peer(from).await?;
                pc.add_ice_candidate(candidate).await?;
                Ok(())
            }
            Signal::Leave => {
                self.remove_viewer(from).await;
                Ok(())
            }
            Signal::Offer { .. } => Err(anyhow!("o transmissor não aceita offer")),
        }
    }

    async fn peer(&self, id: &str) -> Result<Arc<RTCPeerConnection>> {
        self.peers.lock().await.get(id).cloned().ok_or_else(|| anyhow!("amigo {id} não está conectado"))
    }

    async fn add_viewer(self: &Arc<Self>, id: &str) -> Result<()> {
        // Reconexão: derruba a antiga primeiro
        self.remove_viewer(id).await;

        let config = RTCConfiguration { ice_servers: self.ice.clone(), ..Default::default() };
        let pc = Arc::new(self.api.new_peer_connection(config).await?);

        for track in [self.video.clone() as Arc<dyn TrackLocal + Send + Sync>, self.audio.clone() as _] {
            let sender = pc.add_track(track).await?;
            // Lê o RTCP (relatórios do amigo): mantém a fila andando e registra
            // perdas e pedidos de imagem, pra diagnosticar travadas.
            let who = id.to_owned();
            let loss_tx = self.loss_tx.lock().unwrap().clone();
            tokio::spawn(async move {
                use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
                use webrtc::rtcp::receiver_report::ReceiverReport;
                use webrtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;
                let mut b = vec![0u8; 1500];
                let (mut plis, mut nacks, mut lost, mut worst) = (0u32, 0u32, 0u32, 0u8);
                let mut last = Instant::now();
                while let Ok((pkts, _)) = sender.read(&mut b).await {
                    for p in pkts {
                        let a = p.as_any();
                        if a.downcast_ref::<PictureLossIndication>().is_some() {
                            plis += 1;
                        } else if let Some(n) = a.downcast_ref::<TransportLayerNack>() {
                            nacks += n.nacks.len() as u32;
                        } else if let Some(rr) = a.downcast_ref::<ReceiverReport>() {
                            for r in &rr.reports {
                                worst = worst.max(r.fraction_lost);
                                lost = r.total_lost;
                                if let Some(tx) = &loss_tx {
                                    let _ = tx.send((who.clone(), r.fraction_lost));
                                }
                            }
                        }
                    }
                    if last.elapsed() >= Duration::from_secs(10) {
                        if plis + nacks > 0 || worst > 0 {
                            log::info!(
                                target: "star::hub",
                                "espectador {who}: perda até {:.1}%, total perdido {lost}, NACKs {nacks}, pedidos de imagem {plis}",
                                worst as f64 / 2.56
                            );
                        }
                        (plis, nacks, worst) = (0, 0, 0);
                        last = Instant::now();
                    }
                }
            });
        }

        let ev = self.events.clone();
        let to = id.to_owned();
        pc.on_ice_candidate(Box::new(move |c| {
            let ev = ev.clone();
            let to = to.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    log::info!(target: "star::hub", "meu candidato: {c}");
                    if let Ok(init) = c.to_json() {
                        let _ = ev.send(HubEvent::Signal { to, data: Signal::Ice { candidate: init } });
                    }
                }
            })
        }));

        let ev = self.events.clone();
        let hub = Arc::downgrade(self);
        let who = id.to_owned();
        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            let ev = ev.clone();
            let hub = hub.clone();
            let who = who.clone();
            Box::pin(async move {
                log::info!(target: "star::hub", "espectador {who}: {s}");
                let _ = ev.send(HubEvent::ViewerState { peer: who.clone(), state: s.to_string() });
                if matches!(s, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
                    if let Some(h) = hub.upgrade() {
                        h.peers.lock().await.remove(&who);
                    }
                }
            })
        }));

        // Canal de interação: reações, ping, clipe e controle remoto
        let dc = pc.create_data_channel("star", None).await?;
        {
            let who = id.to_owned();
            let ev = self.events.clone();
            let remote = self.remote.lock().unwrap().clone();
            dc.on_message(Box::new(move |m: DataChannelMessage| {
                let who = who.clone();
                let ev = ev.clone();
                let remote = remote.clone();
                Box::pin(async move {
                    if m.data.len() > 4096 {
                        return;
                    }
                    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&m.data) else { return };
                    if v.get("t").and_then(|t| t.as_str()) == Some("in") {
                        // Mouse/teclado vai direto, sem passar pela interface (menos atraso)
                        if let Some(r) = &remote {
                            r.handle(&who, &v);
                        }
                        return;
                    }
                    let _ = ev.send(HubEvent::Interaction { peer: who, msg: v });
                })
            }));
        }
        self.channels.lock().await.insert(id.to_owned(), dc);

        let offer = pc.create_offer(None).await?;
        self.peers.lock().await.insert(id.to_owned(), pc.clone());
        // O offer sai ANTES de começar a coletar candidatos ICE, senão eles
        // chegam no amigo antes do offer e são descartados.
        let _ = self.events.send(HubEvent::Signal { to: id.to_owned(), data: Signal::Offer { sdp: offer.sdp.clone() } });
        pc.set_local_description(offer).await?;
        Ok(())
    }

    pub async fn remove_viewer(&self, id: &str) {
        self.channels.lock().await.remove(id);
        if let Some(r) = self.remote.lock().unwrap().clone() {
            if r.controller().as_deref() == Some(id) {
                r.revoke("quem controlava saiu da live");
            }
        }
        let pc = self.peers.lock().await.remove(id);
        if let Some(pc) = pc {
            let _ = pc.close().await;
        }
    }

    pub async fn close_all(&self) {
        let all: Vec<_> = self.peers.lock().await.drain().map(|(_, pc)| pc).collect();
        for pc in all {
            let _ = pc.close().await;
        }
    }
}

/// Quando o FFmpeg reinicia (troca de qualidade), ele recomeça a numeração dos
/// pacotes do zero. Isso "costura" a numeração pra quem assiste não perceber.
#[derive(Default)]
struct Stitch {
    ssrc: Option<u32>,
    seq_off: u16,
    ts_off: u32,
    last_seq: u16,
    last_ts: u32,
}

impl Stitch {
    /// Devolve true quando percebe que o FFmpeg reiniciou.
    fn apply(&mut self, p: &mut Packet) -> bool {
        let mut restarted = false;
        if self.ssrc != Some(p.header.ssrc) {
            if self.ssrc.is_some() {
                restarted = true;
                self.seq_off = self.last_seq.wrapping_add(1).wrapping_sub(p.header.sequence_number);
                self.ts_off = self.last_ts.wrapping_add(3000).wrapping_sub(p.header.timestamp);
            }
            self.ssrc = Some(p.header.ssrc);
        }
        p.header.sequence_number = p.header.sequence_number.wrapping_add(self.seq_off);
        p.header.timestamp = p.header.timestamp.wrapping_add(self.ts_off);
        self.last_seq = p.header.sequence_number;
        self.last_ts = p.header.timestamp;
        restarted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pkt(ssrc: u32, seq: u16, ts: u32) -> Packet {
        let mut p = Packet::default();
        p.header.ssrc = ssrc;
        p.header.sequence_number = seq;
        p.header.timestamp = ts;
        p
    }
    #[test]
    fn costura_reinicio_do_ffmpeg() {
        let mut s = Stitch::default();
        let mut a = pkt(1, 100, 9000);
        s.apply(&mut a);
        assert_eq!((a.header.sequence_number, a.header.timestamp), (100, 9000));
        let mut b = pkt(1, 101, 12000);
        s.apply(&mut b);
        // FFmpeg reiniciou: outro ssrc, numeração aleatória
        let mut c = pkt(7, 55000, 123);
        s.apply(&mut c);
        assert_eq!(c.header.sequence_number, 102);
        assert_eq!(c.header.timestamp, 15000);
        let mut d = pkt(7, 55001, 3123);
        s.apply(&mut d);
        assert_eq!((d.header.sequence_number, d.header.timestamp), (103, 18000));
    }
}

/// Guarda os últimos ~32 s de vídeo (pacotes RTP) e áudio (Opus), com a hora de chegada.
#[derive(Default)]
pub struct ClipBuf {
    video: std::collections::VecDeque<(Instant, Packet)>,
    audio: std::collections::VecDeque<(Instant, bytes::Bytes)>,
}

const CLIP_KEEP: Duration = Duration::from_secs(32);

impl ClipBuf {
    fn trim(&mut self, now: Instant) {
        while self.video.front().is_some_and(|(t, _)| now.duration_since(*t) > CLIP_KEEP) {
            self.video.pop_front();
        }
        while self.audio.front().is_some_and(|(t, _)| now.duration_since(*t) > CLIP_KEEP) {
            self.audio.pop_front();
        }
    }
    pub fn push_video(&mut self, p: Packet) {
        let now = Instant::now();
        self.video.push_back((now, p));
        self.trim(now);
    }
    pub fn push_audio(&mut self, b: bytes::Bytes) {
        let now = Instant::now();
        self.audio.push_back((now, b));
        self.trim(now);
    }
    pub fn clear(&mut self) {
        self.video.clear();
        self.audio.clear();
    }
    /// Pega a partir do primeiro quadro-chave dentro dos últimos `secs`
    /// (ou o mais antigo disponível) e o áudio a partir do mesmo instante.
    pub fn snapshot(&self, secs: Duration) -> (Vec<Packet>, Vec<bytes::Bytes>) {
        let now = Instant::now();
        let cut = now.checked_sub(secs).unwrap_or(now);
        let is_key = |p: &Packet| crate::clip::starts_keyframe(&p.payload);
        let start = self
            .video
            .iter()
            .position(|(t, p)| *t >= cut && is_key(p))
            .or_else(|| self.video.iter().position(|(_, p)| is_key(p)));
        let Some(i) = start else { return (vec![], vec![]) };
        let t0 = self.video[i].0;
        let video = self.video.iter().skip(i).map(|(_, p)| p.clone()).collect();
        let audio = self.audio.iter().filter(|(t, _)| *t >= t0).map(|(_, b)| b.clone()).collect();
        (video, audio)
    }
}

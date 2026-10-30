//! O "mini servidor" embutido: recebe o vídeo do FFmpeg e o áudio do PC
//! e manda uma cópia pra cada amigo conectado, direto PC→PC (WebRTC).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
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
        }))
    }

    /// Abre a porta local onde o FFmpeg vai jogar o vídeo e começa a repassar.
    /// Devolve a porta pra montar o comando do FFmpeg.
    pub async fn start_video_ingest(self: &Arc<Self>) -> Result<u16> {
        let sock = UdpSocket::bind("127.0.0.1:0").await?;
        let port = sock.local_addr()?.port();
        let hub = self.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let n = match sock.recv(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => break,
                };
                hub.video_bytes.fetch_add(n as u64, Ordering::Relaxed);
                let mut raw = &buf[..n];
                if let Ok(pkt) = Packet::unmarshal(&mut raw) {
                    // Sem ninguém assistindo isso só descarta; com amigos, vai pra cada um.
                    if let Err(e) = hub.video.write_rtp(&pkt).await {
                        log::debug!("vídeo: {e}");
                    }
                }
            }
        });
        Ok(port)
    }

    /// Manda 20 ms de áudio Opus já codificado.
    pub async fn write_audio(&self, opus: bytes::Bytes) {
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
            // Precisa ler o RTCP (relatórios do amigo) pra fila não travar
            tokio::spawn(async move {
                let mut b = vec![0u8; 1500];
                while sender.read(&mut b).await.is_ok() {}
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

        let offer = pc.create_offer(None).await?;
        self.peers.lock().await.insert(id.to_owned(), pc.clone());
        // O offer sai ANTES de começar a coletar candidatos ICE, senão eles
        // chegam no amigo antes do offer e são descartados.
        let _ = self.events.send(HubEvent::Signal { to: id.to_owned(), data: Signal::Offer { sdp: offer.sdp.clone() } });
        pc.set_local_description(offer).await?;
        Ok(())
    }

    pub async fn remove_viewer(&self, id: &str) {
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

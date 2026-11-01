//! Teste de ponta a ponta sem Windows: FFmpeg (padrão de teste) → mini
//! servidor → "amigo" WebRTC na mesma máquina. Conta quanto vídeo e áudio
//! chegaram do outro lado.
//!
//!   cargo run --example loopback

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use star_core::audio::OpusPacker;
use star_core::hub::{Hub, HubEvent, Signal};
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<HubEvent>();
    let hub = Hub::new(vec![], ev_tx)?;
    let port = hub.start_video_ingest(4000).await?;

    // "FFmpeg" com padrão de teste no lugar da tela
    let spawn_ff = |size: &str| {
        tokio::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={size}:rate=30"))
            .args(["-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency", "-bf", "0", "-profile:v", "high"])
            .args(["-b:v", "4000k", "-g", "60", "-bsf:v", "dump_extra=freq=keyframe", "-f", "rtp", "-payload_type", "96"])
            .arg(format!("rtp://127.0.0.1:{port}?pkt_size=1200"))
            .kill_on_drop(true)
            .spawn()
    };
    let mut ff = spawn_ff("1280x720")?;

    // Áudio: senoide de 440 Hz em Opus
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut p = OpusPacker::new(128_000).unwrap();
            let mut t = 0f32;
            loop {
                let mut s = Vec::with_capacity(1920);
                for _ in 0..960 {
                    let v = (t * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3;
                    s.push(v);
                    s.push(v);
                    t += 1.0;
                }
                for pkt in p.push(&s, 1.0) {
                    hub.write_audio(pkt).await;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    }

    // O "amigo"
    let mut m = MediaEngine::default();
    if std::env::var("DEFAULTS").is_ok() { m.register_default_codecs()?; } else {
    use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters};
    m.register_codec(RTCRtpCodecParameters { capability: RTCRtpCodecCapability { mime_type: "video/H264".into(), clock_rate: 90000, channels: 0, sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640034".into(), rtcp_feedback: vec![] }, payload_type: 102, ..Default::default() }, RTPCodecType::Video)?;
    m.register_codec(RTCRtpCodecParameters { capability: RTCRtpCodecCapability { mime_type: "audio/opus".into(), clock_rate: 48000, channels: 2, sdp_fmtp_line: "minptime=10;useinbandfec=1".into(), rtcp_feedback: vec![] }, payload_type: 111, ..Default::default() }, RTPCodecType::Audio)?;
    }
    let reg = register_default_interceptors(Registry::new(), &mut m)?;
    let api = APIBuilder::new().with_media_engine(m).with_interceptor_registry(reg).build();
    let viewer = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);
    for kind in [RTPCodecType::Video, RTPCodecType::Audio] {
        viewer
            .add_transceiver_from_kind(
                kind,
                Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, send_encodings: vec![] }),
            )
            .await?;
    }
    let vbytes = Arc::new(AtomicU64::new(0));
    let abytes = Arc::new(AtomicU64::new(0));
    {
        let (vb, ab) = (vbytes.clone(), abytes.clone());
        viewer.on_track(Box::new(move |track, _, _| {
            let counter = if track.kind() == RTPCodecType::Video { vb.clone() } else { ab.clone() };
            println!("amigo recebeu trilha: {} ({})", track.kind(), track.codec().capability.mime_type);
            // O callback precisa retornar logo: a leitura roda numa tarefa separada
            tokio::spawn(async move {
                while let Ok((pkt, _)) = track.read_rtp().await {
                    counter.fetch_add(pkt.payload.len() as u64, Ordering::Relaxed);
                }
            });
            Box::pin(async {})
        }));
    }
    // Canal de interação do lado de quem assiste
    let (dc_tx, mut dc_rx) = mpsc::unbounded_channel::<Arc<webrtc::data_channel::RTCDataChannel>>();
    let got_back = Arc::new(AtomicU64::new(0));
    {
        let got_back = got_back.clone();
        viewer.on_data_channel(Box::new(move |dc| {
            let dc_tx = dc_tx.clone();
            let got_back = got_back.clone();
            Box::pin(async move {
                println!("amigo abriu o canal de interação: {}", dc.label());
                dc.on_message(Box::new(move |m| {
                    let txt = String::from_utf8_lossy(&m.data).to_string();
                    if txt.contains("\"react\"") {
                        got_back.fetch_add(1, Ordering::Relaxed);
                    }
                    Box::pin(async {})
                }));
                let d2 = dc.clone();
                dc.on_open(Box::new(move || {
                    let _ = dc_tx.send(d2.clone());
                    Box::pin(async {})
                }));
            })
        }));
    }
    let mut interactions = 0u32;
    let (vice_tx, mut vice_rx) = mpsc::unbounded_channel();
    viewer.on_ice_candidate(Box::new(move |c| {
        let tx = vice_tx.clone();
        Box::pin(async move {
            if let Some(c) = c {
                let _ = tx.send(c.to_json().unwrap());
            }
        })
    }));

    hub.on_signal("amigo", Signal::Join).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let restart_at = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut restarted = false;
    let mut v_at_restart = 0u64;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(restart_at), if !restarted => {
                // Simula a troca de qualidade: FFmpeg novo, outra resolução, numeração nova
                restarted = true;
                let _ = ff.kill().await;
                v_at_restart = vbytes.load(Ordering::Relaxed);
                ff = spawn_ff("640x360")?;
                println!("FFmpeg reiniciado em 640x360 (recebido até aqui: {} KB)", v_at_restart / 1024);
            }
            Some(ev) = ev_rx.recv() => match ev {
                HubEvent::Signal { data: Signal::Offer { sdp }, .. } => {
                    if std::env::var("SDP").is_ok() { println!("OFFER:\n{}", sdp); }
                    viewer.set_remote_description(RTCSessionDescription::offer(sdp)?).await?;
                    let ans = viewer.create_answer(None).await?;
                    viewer.set_local_description(ans.clone()).await?;
                    if std::env::var("SDP").is_ok() { println!("ANSWER:\n{}", ans.sdp); }
                    hub.on_signal("amigo", Signal::Answer { sdp: ans.sdp }).await?;
                }
                HubEvent::Signal { data: Signal::Ice { candidate }, .. } => { viewer.add_ice_candidate(candidate).await?; }
                HubEvent::ViewerState { state, .. } => println!("conexão: {state}"),
                HubEvent::Interaction { peer, msg } => {
                    println!("interação de {peer}: {msg}");
                    interactions += 1;
                    let mut out = msg.clone();
                    out["by"] = serde_json::json!(peer);
                    hub.broadcast(&out).await;
                }
                _ => {}
            },
            Some(c) = vice_rx.recv() => { let _ = hub.on_signal("amigo", Signal::Ice { candidate: c }).await; }
            Some(dc) = dc_rx.recv() => {
                dc.send_text(r#"{"t":"react","e":"fire"}"#).await?;
                dc.send_text(r#"{"t":"ping","x":0.5,"y":0.25}"#).await?;
            }
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }

    println!("hub viu de vídeo: {:?}", hub.stats(Duration::from_secs(8)).await);
    let st = viewer.get_stats().await;
    for (k, v) in st.reports.iter() {
        let d = format!("{:?}", v);
        if d.contains("InboundRTP") || d.contains("Transport(") { println!("{k}: {}", &d[..d.len().min(400)]); }
    }
    let v = vbytes.load(Ordering::Relaxed);
    let a = abytes.load(Ordering::Relaxed);
    println!("vídeo recebido: {} KB, áudio recebido: {} KB", v / 1024, a / 1024);
    let _ = ff.kill().await;
    // Clipe dos últimos 4 s
    let clip = std::env::temp_dir().join("star-clip-teste.mp4");
    let _ = std::fs::remove_file(&clip);
    match hub.save_clip(std::path::Path::new("ffmpeg"), 30, 4, &clip).await {
        Ok(()) => {
            let probe = std::process::Command::new("ffprobe")
                .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name,width,height,duration", "-of", "compact"])
                .arg(&clip)
                .output()?;
            println!("clipe: {}", String::from_utf8_lossy(&probe.stdout).replace('\n', " | "));
        }
        Err(e) => println!("clipe falhou: {e}"),
    }
    println!("interações recebidas: {interactions}, reações devolvidas ao amigo: {}", got_back.load(Ordering::Relaxed));
    let after = v - v_at_restart;
    println!("vídeo depois da troca: {} KB", after / 1024);
    let clip_ok = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
        .arg(&clip)
        .output()
        .map(|o| { let t = String::from_utf8_lossy(&o.stdout).to_string(); t.contains("video") && t.contains("audio") })
        .unwrap_or(false);
    println!("clipe com vídeo e áudio: {clip_ok}");
    if v > 100 * 1024 && a > 10 * 1024 && after > 100 * 1024 && clip_ok && interactions >= 2 && got_back.load(Ordering::Relaxed) >= 1 {
        println!("OK: vídeo e áudio chegaram do outro lado");
        Ok(())
    } else {
        anyhow::bail!("FALHOU: dados insuficientes")
    }
}

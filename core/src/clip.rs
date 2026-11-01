//! "Clipa isso": transforma os últimos segundos guardados (RTP H.264 + Opus)
//! num arquivo MP4, sem recodificar (é instantâneo e não pesa no PC).

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use webrtc::media::io::h264_writer::H264Writer;
use webrtc::media::io::ogg_writer::OggWriter;
use webrtc::media::io::Writer;
use webrtc::rtp::packet::Packet;

use crate::video::NoWindow;

/// O pacote começa um quadro-chave? (SPS sozinho, ou num pacote agregado STAP-A)
pub fn starts_keyframe(payload: &[u8]) -> bool {
    let Some(&b0) = payload.first() else { return false };
    match b0 & 0x1F {
        7 => true,
        24 => payload.get(3).is_some_and(|b| b & 0x1F == 7),
        _ => false,
    }
}

pub async fn write_mp4(ffmpeg: &Path, fps: u32, video: Vec<Packet>, audio: Vec<bytes::Bytes>, out: &Path) -> Result<()> {
    let dir = out.parent().ok_or_else(|| anyhow!("pasta de clipes inválida"))?;
    std::fs::create_dir_all(dir).context("não consegui criar a pasta de clipes")?;
    let stem = out.file_stem().and_then(|s| s.to_str()).unwrap_or("clip").to_string();
    let vpath = dir.join(format!(".{stem}.h264"));
    let apath = dir.join(format!(".{stem}.ogg"));
    let has_audio = !audio.is_empty();

    // Escreve os arquivos brutos (fora da thread assíncrona: é disco)
    {
        let (vpath, apath) = (vpath.clone(), apath.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut w = H264Writer::new(std::io::BufWriter::new(std::fs::File::create(&vpath)?));
            for p in &video {
                w.write_rtp(p)?;
            }
            w.close()?;
            if !audio.is_empty() {
                let mut o = OggWriter::new(std::io::BufWriter::new(std::fs::File::create(&apath)?), 48_000, 2)?;
                for (i, b) in audio.iter().enumerate() {
                    let mut p = Packet::default();
                    p.header.sequence_number = i as u16;
                    p.header.timestamp = (i as u32).wrapping_mul(960); // 20 ms por pacote
                    p.payload = b.clone();
                    o.write_rtp(&p)?;
                }
                o.close()?;
            }
            Ok(())
        })
        .await??;
    }

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-r", &fps.max(1).to_string(), "-f", "h264", "-i"])
        .arg(&vpath);
    if has_audio {
        // (sem -shortest: com vídeo bruto sem horários ele descarta o áudio)
        cmd.arg("-i").arg(&apath).args(["-map", "0:v", "-map", "1:a"]);
    }
    cmd.args(["-c", "copy", "-movflags", "+faststart"])
        .arg(out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .creation_flags_no_window();
    let res = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output()).await;
    if std::env::var("STAR_KEEP_CLIP").is_err() {
        let _ = std::fs::remove_file(&vpath);
        let _ = std::fs::remove_file(&apath);
    }
    let o = res.map_err(|_| anyhow!("o clipe demorou demais"))??;
    if !o.status.success() {
        return Err(anyhow!("FFmpeg não conseguiu montar o clipe: {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detecta_quadro_chave() {
        assert!(starts_keyframe(&[0x67, 0x64, 0x00]));
        assert!(starts_keyframe(&[0x78, 0x00, 0x10, 0x67]));
        assert!(!starts_keyframe(&[0x65, 0x88]));
        assert!(!starts_keyframe(&[0x7C, 0x85]));
        assert!(!starts_keyframe(&[]));
    }
}

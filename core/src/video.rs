//! Vídeo: o FFmpeg captura a tela/janela pela própria GPU (Windows Graphics
//! Capture, sem a borda amarela), codifica no encoder de hardware e manda
//! pacotes RTP pra uma porta local. O núcleo só repassa esses pacotes
//! pros amigos via WebRTC: a imagem nunca passa pela CPU.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Encoder {
    /// NVIDIA
    Nvenc,
    /// AMD
    Amf,
    /// Intel
    Qsv,
    /// Sem GPU compatível: processador (último recurso)
    X264,
}

impl Encoder {
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Encoder::Nvenc => "h264_nvenc",
            Encoder::Amf => "h264_amf",
            Encoder::Qsv => "h264_qsv",
            Encoder::X264 => "libx264",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Encoder::Nvenc => "NVIDIA (NVENC)",
            Encoder::Amf => "AMD (AMF)",
            Encoder::Qsv => "Intel (QuickSync)",
            Encoder::X264 => "Processador (x264)",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    /// Pra conexões lentas (relay do Radmin, internet fraca)
    Economica,
    Leve,
    Equilibrada,
    Maxima,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PresetSpec {
    pub max_height: u32,
    pub fps: u32,
    pub kbps: u32,
}

impl Preset {
    pub fn spec(self) -> PresetSpec {
        match self {
            Preset::Economica => PresetSpec { max_height: 720, fps: 30, kbps: 2_500 },
            Preset::Leve => PresetSpec { max_height: 1080, fps: 30, kbps: 6_000 },
            Preset::Equilibrada => PresetSpec { max_height: 1080, fps: 60, kbps: 10_000 },
            Preset::Maxima => PresetSpec { max_height: 1440, fps: 60, kbps: 15_000 },
        }
    }
}

/// O que transmitir.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    /// Monitor inteiro (HMONITOR, vindo da lista de fontes)
    Monitor { handle: u64 },
    /// Uma janela específica (HWND)
    Window { hwnd: u64 },
}

/// Descobre qual encoder de hardware funciona de verdade nesse PC,
/// testando um encode de meio segundo em cada um, do melhor pro pior.
pub async fn detect_encoder(ffmpeg: &Path) -> Encoder {
    for enc in [Encoder::Nvenc, Encoder::Amf, Encoder::Qsv] {
        let ok = Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=black:s=1280x720:r=30:d=0.5"])
            .args(["-c:v", enc.ffmpeg_name(), "-f", "null", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .creation_flags_no_window()
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return enc;
        }
    }
    Encoder::X264
}

/// Monta a linha de comando do FFmpeg. Separado pra dar pra testar.
/// Tamanho de saída: no máximo a altura do preset, mantendo a proporção,
/// nunca maior que a original, sempre par (exigência do H.264).
pub fn output_size(src: (u32, u32), max_height: u32) -> (u32, u32) {
    let (w, h) = (src.0.max(2), src.1.max(2));
    let even = |v: u32| (v.max(2)) & !1;
    if h <= max_height {
        return (even(w), even(h));
    }
    let nh = max_height;
    let nw = ((w as u64 * nh as u64 + h as u64 / 2) / h as u64) as u32;
    (even(nw), even(nh))
}

/// Monta a linha de comando do FFmpeg. `src_size` é o tamanho real da tela ou
/// janela (quando conhecido). Separado pra dar pra testar.
pub fn ffmpeg_args(source: &Source, src_size: Option<(u32, u32)>, preset: Preset, encoder: Encoder, rtp_port: u16) -> Vec<String> {
    let spec = preset.spec();
    let fps = spec.fps;
    let kbps = spec.kbps;
    let gop = fps; // um quadro-chave por segundo: imagem volta rápido se algo se perder

    // Captura via Windows Graphics Capture, direto na GPU, sem borda amarela.
    let target = match source {
        Source::Monitor { handle } => format!("hmonitor={handle}"),
        Source::Window { hwnd } => format!("hwnd={hwnd}"),
    };
    let mut capture = format!(
        "gfxcapture={target}:max_framerate={fps}:display_border=0:capture_cursor=1:dup_frames=1"
    );
    // Redimensiona na própria GPU só quando precisa diminuir
    if let Some(src) = src_size {
        let (w, h) = output_size(src, spec.max_height);
        if (w, h) != (src.0 & !1, src.1 & !1) || src.0 % 2 == 1 || src.1 % 2 == 1 {
            capture.push_str(&format!(":width={w}:height={h}:resize_mode=scale_aspect:scale_mode=bilinear"));
        }
    }

    let mut filter = capture;
    match encoder {
        // NVENC e AMF aceitam a textura D3D11 direto (zero cópia)
        Encoder::Nvenc | Encoder::Amf => {}
        Encoder::Qsv => filter.push_str(",hwmap=derive_device=qsv,format=qsv"),
        Encoder::X264 => filter.push_str(",hwdownload,format=bgra,format=yuv420p"),
    }

    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-stats_period".into(),
        "1".into(),
        "-progress".into(),
        "pipe:1".into(),
        // Dispositivo da GPU com nome próprio ("star"): sem nome o FFmpeg chama de "d3d11va0"
        "-init_hw_device".into(),
        "d3d11va=star".into(),
        "-filter_hw_device".into(),
        "star".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        filter,
        "-an".into(),
        "-c:v".into(),
        encoder.ffmpeg_name().into(),
    ];

    let bitrate = format!("{kbps}k");
    let bufsize = format!("{}k", kbps / 2); // buffer curto = pouca latência
    match encoder {
        Encoder::Nvenc => a.extend(
            [
                "-preset", "p4", "-tune", "ll", "-rc", "cbr", "-zerolatency", "1", "-bf", "0",
                "-profile:v", "high", "-forced-idr", "1",
            ]
            .map(String::from),
        ),
        Encoder::Amf => a.extend(
            ["-usage", "lowlatency", "-quality", "balanced", "-rc", "cbr", "-bf", "0", "-profile:v", "high"]
                .map(String::from),
        ),
        Encoder::Qsv => a.extend(
            ["-preset", "veryfast", "-look_ahead", "0", "-bf", "0", "-profile:v", "high", "-async_depth", "1"]
                .map(String::from),
        ),
        Encoder::X264 => a.extend(
            ["-preset", "veryfast", "-tune", "zerolatency", "-bf", "0", "-profile:v", "high"].map(String::from),
        ),
    }
    a.extend([
        "-b:v".into(),
        bitrate.clone(),
        "-maxrate".into(),
        bitrate,
        "-bufsize".into(),
        bufsize,
        "-g".into(),
        gop.to_string(),
        // Repete SPS/PPS em todo quadro-chave: amigo que entra no meio consegue decodificar
        "-bsf:v".into(),
        "dump_extra=freq=keyframe".into(),
        "-f".into(),
        "rtp".into(),
        "-payload_type".into(),
        "96".into(),
        format!("rtp://127.0.0.1:{rtp_port}?pkt_size=1200"),
    ]);
    a
}

/// Processo do FFmpeg em execução.
pub struct VideoProcess {
    pub child: Child,
}

impl VideoProcess {
    pub async fn spawn(ffmpeg: &Path, args: &[String]) -> Result<Self> {
        if !ffmpeg.exists() {
            bail!("FFmpeg não encontrado em {}", ffmpeg.display());
        }
        let child = Command::new(ffmpeg)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .creation_flags_no_window()
            .spawn()
            .context("não consegui iniciar o FFmpeg")?;
        #[cfg(windows)]
        if let Some(h) = child.raw_handle() {
            job::bind_to_app(h);
        }
        Ok(Self { child })
    }

    pub async fn stop(&mut self) {
        // "q" encerra o FFmpeg de forma limpa; se não sair, mata.
        if let Some(stdin) = self.child.stdin.as_mut() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(b"q").await;
        }
        if tokio::time::timeout(std::time::Duration::from_secs(2), self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
        }
    }
}

/// "Job" do Windows: tudo que entra nele morre quando o app fecha, mesmo se o
/// app cair. Assim o ffmpeg.exe nunca fica preso rodando escondido.
#[cfg(windows)]
mod job {
    use std::sync::OnceLock;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    struct Job(HANDLE);
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}
    static JOB: OnceLock<Option<Job>> = OnceLock::new();

    pub fn bind_to_app(process: std::os::windows::io::RawHandle) {
        let job = JOB.get_or_init(|| unsafe {
            let h = CreateJobObjectW(None, None).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .ok()?;
            Some(Job(h)) // o handle fica aberto até o app fechar, de propósito
        });
        if let Some(Job(h)) = job {
            unsafe {
                if let Err(e) = AssignProcessToJobObject(*h, HANDLE(process)) {
                    log::warn!(target: "star::video", "não consegui amarrar o FFmpeg ao app: {e}");
                }
            }
        }
    }
}

/// Caminho padrão do ffmpeg.exe ao lado do app.
pub fn default_ffmpeg_path(app_dir: &Path) -> PathBuf {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    app_dir.join("bin").join(name)
}

/// Esconde a janela de console do FFmpeg no Windows.
trait NoWindow {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}
impl NoWindow for Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn args_nvenc_monitor() {
        let a = ffmpeg_args(&Source::Monitor { handle: 65537 }, Some((2560, 1440)), Preset::Maxima, Encoder::Nvenc, 5004);
        let line = a.join(" ");
        assert!(line.contains("gfxcapture=hmonitor=65537:max_framerate=60:display_border=0"));
        assert!(!line.contains("height="), "1440p na Máxima não precisa redimensionar");
        assert!(line.contains("-init_hw_device d3d11va=star -filter_hw_device star"));
        assert!(line.contains("-c:v h264_nvenc"));
        assert!(line.contains("-b:v 15000k"));
        assert!(line.ends_with("rtp://127.0.0.1:5004?pkt_size=1200"));
    }
    #[test]
    fn args_x264_window() {
        let a = ffmpeg_args(&Source::Window { hwnd: 1234 }, None, Preset::Leve, Encoder::X264, 6000);
        let line = a.join(" ");
        assert!(line.contains("hwnd=1234"));
        assert!(line.contains("hwdownload,format=bgra"));
        assert!(line.contains("-g 30"));
    }

    #[test]
    fn tamanhos() {
        // O bug: tela 1080p na Máxima virava 1920x0
        assert_eq!(output_size((1920, 1080), 1440), (1920, 1080));
        assert_eq!(output_size((2560, 1440), 1080), (1920, 1080));
        assert_eq!(output_size((1920, 1080), 720), (1280, 720));
        assert_eq!(output_size((3440, 1440), 1080), (2580, 1080));
        assert_eq!(output_size((1001, 777), 1080), (1000, 776)); // janela ímpar
        let a = ffmpeg_args(&Source::Monitor { handle: 1 }, Some((1920, 1080)), Preset::Maxima, Encoder::Nvenc, 1).join(" ");
        assert!(!a.contains("height="));
        let b = ffmpeg_args(&Source::Monitor { handle: 1 }, Some((1920, 1080)), Preset::Economica, Encoder::Nvenc, 1).join(" ");
        assert!(b.contains(":width=1280:height=720:"));
    }
}

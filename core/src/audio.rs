//! Áudio: captura o som do PC e manda em Opus pros amigos.
//!
//! Modo "PC inteiro": em vez de capturar tudo de uma vez, o app captura
//! CADA programa que tem som (Windows 10 build 20348+ / 11, captura por
//! processo) e mistura. Assim ficam de fora:
//!   - o Discord (ninguém ouve a call de volta na transmissão)
//!   - o próprio STAR Stream (dá pra assistir um amigo enquanto transmite
//!     sem reenviar o som da live dele)
//!
//! O volume da transmissão é aplicado aqui, depois da captura: os amigos
//! ouvem no volume que você escolher, sem mexer no que sai no seu fone.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 20 ms por pacote Opus
pub const FRAME_SAMPLES: usize = 960;

/// O mixer entrega blocos de 10 ms
const MIX_FRAMES: usize = 480;
/// Espera 30 ms de som numa fonte antes de começar a usar (evita estalos)
const PRIME_SAMPLES: usize = 1440 * CHANNELS;
/// Mais de 200 ms acumulado numa fonte = descarta o excesso (mantém o atraso baixo)
const MAX_SAMPLES: usize = 9600 * CHANNELS;
/// De quanto em quanto tempo procura programas novos tocando som
const RESCAN: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum AudioMode {
    /// Todos os programas com som, menos o Discord e o próprio STAR Stream
    Pc,
    /// Só um programa (e os processos filhos dele)
    App { pid: u32 },
    /// Sem som
    Off,
}

/// Volume da transmissão compartilhado com a interface (0.0 a 1.5).
#[derive(Clone)]
pub struct Gain(Arc<AtomicU32>);
impl Gain {
    pub fn new(v: f32) -> Self {
        Self(Arc::new(AtomicU32::new(v.to_bits())))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.clamp(0.0, 1.5).to_bits(), Ordering::Relaxed)
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// Encoder Opus: junta amostras em blocos de 20 ms, aplica o volume e codifica.
pub struct OpusPacker {
    enc: audiopus::coder::Encoder,
    pending: Vec<f32>,
    out: Vec<u8>,
}

impl OpusPacker {
    pub fn new(bitrate: i32) -> Result<Self> {
        let mut enc = audiopus::coder::Encoder::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Stereo,
            audiopus::Application::Audio,
        )?;
        enc.set_bitrate(audiopus::Bitrate::BitsPerSecond(bitrate))?;
        Ok(Self { enc, pending: Vec::with_capacity(FRAME_SAMPLES * CHANNELS * 4), out: vec![0u8; 4000] })
    }

    /// Recebe amostras intercaladas (L,R,L,R...) e devolve os pacotes prontos.
    pub fn push(&mut self, samples: &[f32], gain: f32) -> Vec<bytes::Bytes> {
        self.pending.extend(samples.iter().map(|s| (s * gain).clamp(-1.0, 1.0)));
        let mut packets = Vec::new();
        let frame = FRAME_SAMPLES * CHANNELS;
        while self.pending.len() >= frame {
            let chunk: Vec<f32> = self.pending.drain(..frame).collect();
            if let Ok(n) = self.enc.encode_float(&chunk, &mut self.out[..]) {
                packets.push(bytes::Bytes::copy_from_slice(&self.out[..n]));
            }
        }
        packets
    }
}

// ---------------- mixer ----------------

/// Fila de som de um programa.
#[derive(Default)]
pub struct SourceBuf {
    q: VecDeque<f32>,
    primed: bool,
}

impl SourceBuf {
    pub fn push(&mut self, samples: &[f32]) {
        self.q.extend(samples.iter().copied());
        if self.q.len() > MAX_SAMPLES {
            let extra = self.q.len() - PRIME_SAMPLES;
            self.q.drain(..extra);
        }
    }
}

pub type SharedBuf = Arc<Mutex<SourceBuf>>;

/// Soma 10 ms de todas as fontes. Fonte sem som suficiente contribui com silêncio.
pub fn mix_tick(sources: &[SharedBuf], out: &mut Vec<f32>) {
    let n = MIX_FRAMES * CHANNELS;
    out.clear();
    out.resize(n, 0.0);
    for s in sources {
        let mut b = s.lock().unwrap();
        if !b.primed {
            if b.q.len() < PRIME_SAMPLES {
                continue;
            }
            b.primed = true;
        }
        let take = n.min(b.q.len());
        for (o, v) in out.iter_mut().zip(b.q.drain(..take)) {
            *o += v;
        }
        if take < n {
            // Acabou o som dessa fonte: espera encher de novo antes de voltar
            b.primed = false;
        }
    }
    for o in out.iter_mut() {
        *o = o.clamp(-1.0, 1.0);
    }
}

struct Running {
    stop: Arc<AtomicBool>,
    buf: SharedBuf,
    thread: std::thread::JoinHandle<()>,
}

/// Captura em andamento. Para quando for descartada.
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioCapture {
    /// `on_samples` é chamado a cada 10 ms com amostras f32 estéreo 48 kHz.
    pub fn start(mode: AudioMode, mut on_samples: Box<dyn FnMut(&[f32]) + Send>) -> Result<Option<Self>> {
        if matches!(mode, AudioMode::Off) {
            return Ok(None);
        }
        if !platform::SUPPORTED {
            anyhow::bail!("captura de áudio só existe no Windows");
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new().name("star-audio-mixer".into()).spawn(move || {
            platform::com_init();
            let mut running: HashMap<u32, Running> = HashMap::new();
            let mut last_scan: Option<Instant> = None;
            let mut mixed = Vec::with_capacity(MIX_FRAMES * CHANNELS);
            let mut next = Instant::now();

            while !stop2.load(Ordering::Relaxed) {
                if last_scan.is_none_or(|t| t.elapsed() >= RESCAN) {
                    last_scan = Some(Instant::now());
                    let wanted = platform::wanted_roots(&mode);
                    log::debug!(target: "star::audio", "programas com som: {wanted:?}");
                    // Programas que pararam / fecharam
                    running.retain(|pid, r| {
                        let keep = wanted.contains(pid) && !r.thread.is_finished();
                        if !keep {
                            r.stop.store(true, Ordering::Relaxed);
                        }
                        keep
                    });
                    // Programas novos
                    for pid in wanted {
                        if running.contains_key(&pid) {
                            continue;
                        }
                        let s = Arc::new(AtomicBool::new(false));
                        let buf: SharedBuf = Arc::default();
                        let (s2, b2) = (s.clone(), buf.clone());
                        log::info!(target: "star::audio", "capturando processo {pid}");
                        if let Ok(t) = std::thread::Builder::new()
                            .name(format!("star-audio-{pid}"))
                            .spawn(move || platform::capture_source(pid, s2, b2))
                        {
                            running.insert(pid, Running { stop: s, buf, thread: t });
                        }
                    }
                }

                let bufs: Vec<SharedBuf> = running.values().map(|r| r.buf.clone()).collect();
                mix_tick(&bufs, &mut mixed);
                on_samples(&mixed);

                next += Duration::from_millis(10);
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else if now - next > Duration::from_millis(100) {
                    next = now; // atrasou muito (PC travou): recomeça o relógio
                }
            }
            for (_, r) in running.drain() {
                r.stop.store(true, Ordering::Relaxed);
                let _ = r.thread.join();
            }
            platform::com_uninit();
        })?;
        Ok(Some(Self { stop, thread: Some(thread) }))
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Programas com som que dá pra escolher no modo "Só um app".
#[derive(Debug, Clone, Serialize)]
pub struct AudioApp {
    pub pid: u32,
    pub name: String,
}

pub fn list_audio_apps() -> Vec<AudioApp> {
    platform::list_audio_apps()
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub const SUPPORTED: bool = false;
    pub fn com_init() {}
    pub fn com_uninit() {}
    pub fn wanted_roots(_m: &AudioMode) -> Vec<u32> {
        vec![]
    }
    pub fn capture_source(_pid: u32, _stop: Arc<AtomicBool>, _buf: SharedBuf) {}
    pub fn list_audio_apps() -> Vec<AudioApp> {
        vec![]
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::collections::HashSet;
    use std::mem::ManuallyDrop;
    use windows::core::{implement, Interface};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::*;
    use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
    use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
    use windows::Win32::System::Variant::VT_BLOB;

    pub const SUPPORTED: bool = true;

    const DISCORD_EXES: [&str; 4] = ["discord.exe", "discordptb.exe", "discordcanary.exe", "discorddevelopment.exe"];

    pub fn com_init() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
    }
    pub fn com_uninit() {
        unsafe { CoUninitialize() };
    }

    struct Proc {
        parent: u32,
        name: String,
    }

    fn processes() -> HashMap<u32, Proc> {
        let mut out = HashMap::new();
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            if Process32FirstW(snap, &mut e).is_ok() {
                loop {
                    let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                    out.insert(
                        e.th32ProcessID,
                        Proc { parent: e.th32ParentProcessID, name: String::from_utf16_lossy(&e.szExeFile[..len]) },
                    );
                    if Process32NextW(snap, &mut e).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
        out
    }

    /// Sobe pelos pais enquanto o nome for o mesmo (navegadores, Electron...).
    fn root_of(pid: u32, procs: &HashMap<u32, Proc>) -> u32 {
        let mut cur = pid;
        for _ in 0..32 {
            let Some(p) = procs.get(&cur) else { break };
            match procs.get(&p.parent) {
                Some(pp) if pp.name.eq_ignore_ascii_case(&p.name) && p.parent != cur => cur = p.parent,
                _ => break,
            }
        }
        cur
    }

    /// O processo (ou algum ancestral) é o Discord ou o próprio STAR Stream?
    fn is_excluded(pid: u32, procs: &HashMap<u32, Proc>) -> bool {
        let me = std::process::id();
        let mut cur = pid;
        for _ in 0..32 {
            if cur == me {
                return true;
            }
            let Some(p) = procs.get(&cur) else { return false };
            if DISCORD_EXES.contains(&p.name.to_ascii_lowercase().as_str()) {
                return true;
            }
            if p.parent == cur || p.parent == 0 {
                return false;
            }
            cur = p.parent;
        }
        false
    }

    /// PIDs de todos os programas com sessão de som aberta, em qualquer saída de áudio.
    fn session_pids() -> HashSet<u32> {
        let mut pids = HashSet::new();
        log::debug!(target: "star::audio", "lendo sessões de áudio");
        unsafe {
            let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
                return pids;
            };
            let Ok(devs) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return pids };
            let count = devs.GetCount().unwrap_or(0);
            for i in 0..count {
                let Ok(dev) = devs.Item(i) else { continue };
                let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
                let Ok(list) = mgr.GetSessionEnumerator() else { continue };
                for j in 0..list.GetCount().unwrap_or(0) {
                    let Ok(ctl) = list.GetSession(j) else { continue };
                    let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
                    if ctl.GetState().map(|s| s == AudioSessionStateExpired).unwrap_or(true) {
                        continue;
                    }
                    if let Ok(pid) = ctl2.GetProcessId() {
                        if pid != 0 {
                            pids.insert(pid);
                        }
                    }
                }
            }
        }
        pids
    }

    pub fn wanted_roots(mode: &AudioMode) -> Vec<u32> {
        let procs = processes();
        match mode {
            AudioMode::Off => vec![],
            AudioMode::App { pid } => vec![root_of(*pid, &procs)],
            AudioMode::Pc => {
                let mut roots: HashSet<u32> = HashSet::new();
                for pid in session_pids() {
                    if !is_excluded(pid, &procs) {
                        roots.insert(root_of(pid, &procs));
                    }
                }
                roots.into_iter().collect()
            }
        }
    }

    /// Lista programas pelo nome (um por nome, o processo raiz).
    pub fn list_audio_apps() -> Vec<AudioApp> {
        let procs = processes();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for (&pid, p) in &procs {
            let lname = p.name.to_ascii_lowercase();
            if !lname.ends_with(".exe") || is_excluded(pid, &procs) || root_of(pid, &procs) != pid {
                continue;
            }
            if seen.insert(lname) {
                out.push(AudioApp { pid, name: p.name.trim_end_matches(".exe").to_string() });
            }
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        out
    }

    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct Done(HANDLE);

    impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
        fn ActivateCompleted(&self, _op: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
            unsafe { SetEvent(self.0) }
        }
    }

    /// Abre a captura do som de UMA árvore de processos.
    fn activate(pid: u32) -> windows::core::Result<IAudioClient> {
        unsafe {
            let params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: pid,
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };
            let prop = PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_BLOB,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 {
                            blob: BLOB {
                                cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                                pBlobData: &params as *const _ as *mut u8,
                            },
                        },
                    }),
                },
            };
            let event = CreateEventW(None, false, false, None)?;
            let handler: IActivateAudioInterfaceCompletionHandler = Done(event).into();
            log::debug!(target: "star::audio", "processo {pid}: pedindo captura ao Windows");
            let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&prop), &handler)?;
            let w = WaitForSingleObject(event, 5_000);
            log::debug!(target: "star::audio", "processo {pid}: Windows respondeu ({})", w.0);
            let _ = CloseHandle(event);
            let mut hr = windows::core::HRESULT(0);
            let mut unk: Option<windows::core::IUnknown> = None;
            op.GetActivateResult(&mut hr, &mut unk)?;
            hr.ok()?;
            unk.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?.cast()
        }
    }

    /// Thread de captura de um programa: joga o som na fila dele até mandarem parar.
    pub fn capture_source(pid: u32, stop: Arc<AtomicBool>, buf: SharedBuf) {
        com_init();
        let r = (|| -> windows::core::Result<()> {
            let client = activate(pid)?;
            log::debug!(target: "star::audio", "processo {pid}: captura ativada");
            unsafe {
                // Process loopback exige que a gente diga o formato: float 48 kHz estéreo
                let fmt = WAVEFORMATEXTENSIBLE {
                    Format: WAVEFORMATEX {
                        wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                        nChannels: CHANNELS as u16,
                        nSamplesPerSec: SAMPLE_RATE,
                        nAvgBytesPerSec: SAMPLE_RATE * (CHANNELS as u32) * 4,
                        nBlockAlign: (CHANNELS as u16) * 4,
                        wBitsPerSample: 32,
                        cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16,
                    },
                    Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
                    dwChannelMask: 0x3,
                    SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
                };
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                    200_000,
                    0,
                    &fmt.Format,
                    None,
                )?;
                let event = CreateEventW(None, false, false, None)?;
                client.SetEventHandle(event)?;
                let capture: IAudioCaptureClient = client.GetService()?;
                client.Start()?;
                log::debug!(target: "star::audio", "processo {pid}: gravando");
                let mut tmp: Vec<f32> = Vec::with_capacity(4096);
                while !stop.load(Ordering::Relaxed) {
                    if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                        continue;
                    }
                    loop {
                        let mut data: *mut u8 = std::ptr::null_mut();
                        let mut frames = 0u32;
                        let mut flags = 0u32;
                        if capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() || frames == 0 {
                            break;
                        }
                        tmp.clear();
                        let n = frames as usize * CHANNELS;
                        if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                            tmp.resize(n, 0.0);
                        } else {
                            tmp.extend_from_slice(std::slice::from_raw_parts(data as *const f32, n));
                        }
                        let _ = capture.ReleaseBuffer(frames);
                        buf.lock().unwrap().push(&tmp);
                    }
                }
                let _ = client.Stop();
                let _ = CloseHandle(event);
            }
            Ok(())
        })();
        if let Err(e) = r {
            log::warn!(target: "star::audio", "captura do processo {pid} falhou: {}", e.message());
        }
        com_uninit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(samples: &[f32]) -> SharedBuf {
        let b: SharedBuf = Arc::default();
        b.lock().unwrap().push(samples);
        b
    }

    #[test]
    fn mistura_duas_fontes() {
        let a = src(&vec![0.25; PRIME_SAMPLES]);
        let b = src(&vec![0.5; PRIME_SAMPLES]);
        let mut out = Vec::new();
        mix_tick(&[a, b], &mut out);
        assert_eq!(out.len(), MIX_FRAMES * CHANNELS);
        assert!(out.iter().all(|v| (*v - 0.75).abs() < 1e-6));
    }

    #[test]
    fn fonte_sem_som_suficiente_vira_silencio() {
        let pouco = src(&vec![0.9; 100]);
        let mut out = Vec::new();
        mix_tick(&[pouco.clone()], &mut out);
        assert!(out.iter().all(|v| *v == 0.0));
        // não consumiu nada enquanto não encheu
        assert_eq!(pouco.lock().unwrap().q.len(), 100);
    }

    #[test]
    fn corta_no_maximo_e_limita_atraso() {
        let a = src(&vec![1.0; MAX_SAMPLES + 5000]);
        assert!(a.lock().unwrap().q.len() <= MAX_SAMPLES);
        let b = src(&vec![1.0; PRIME_SAMPLES]);
        let mut out = Vec::new();
        mix_tick(&[a, b], &mut out);
        assert!(out.iter().all(|v| *v == 1.0)); // 2.0 limitado a 1.0
    }
}

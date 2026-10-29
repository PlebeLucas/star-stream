//! Diagnóstico: grava o que o app faz (e qualquer travamento) em
//! %LOCALAPPDATA%\STAR Stream\star.log, pra dar pra descobrir o que deu errado.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

pub fn log_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("STAR Stream")
}

pub fn log_path() -> PathBuf {
    log_dir().join("star.log")
}

struct FileLog(Mutex<Option<File>>);

impl log::Log for FileLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        // Nosso código em detalhe; bibliotecas só avisos e erros
        let ours = m.target().starts_with("star");
        m.level() <= if ours { log::Level::Debug } else { log::Level::Warn }
    }
    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        write_line(&format!("{:<5} {} | {}", r.level(), r.target(), r.args()));
    }
    fn flush(&self) {}
}

static LOGGER: FileLog = FileLog(Mutex::new(None));

fn write_line(s: &str) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if let Ok(mut g) = LOGGER.0.lock() {
        if let Some(f) = g.as_mut() {
            let _ = writeln!(f, "[{secs:.3}] {s}");
            let _ = f.flush(); // grava na hora: se o app cair, a última linha fica
        }
    }
}

pub fn init() {
    let _ = std::fs::create_dir_all(log_dir());
    // Guarda só a sessão atual + a anterior
    let path = log_path();
    let _ = std::fs::rename(&path, log_dir().join("star.anterior.log"));
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) {
        *LOGGER.0.lock().unwrap() = Some(f);
    }
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);

    #[cfg(windows)]
    native::install(&path);

    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        write_line(&format!("PANIC na thread '{thread}': {info}\n{bt}"));
    }));

    write_line(&format!(
        "===== STAR Stream {} iniciado (Windows: {}) =====",
        env!("CARGO_PKG_VERSION"),
        std::env::var("OS").unwrap_or_default()
    ));
}

/// Travamentos "nativos" (fora do Rust seguro): registra o código do erro.
#[cfg(windows)]
mod native {
    use std::os::windows::io::IntoRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE};
    use windows::Win32::System::Diagnostics::Debug::{SetUnhandledExceptionFilter, EXCEPTION_POINTERS};

    unsafe extern "system" fn on_crash(info: *const EXCEPTION_POINTERS) -> i32 {
        if let Some(rec) = info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()) {
            super::write_line(&format!(
                "CRASH nativo: código 0x{:08X} no endereço {:?}",
                rec.ExceptionCode.0 as u32, rec.ExceptionAddress
            ));
        }
        0 // EXCEPTION_CONTINUE_SEARCH: deixa o Windows fechar normalmente
    }

    pub fn install(path: &std::path::Path) {
        unsafe {
            SetUnhandledExceptionFilter(Some(on_crash));
            // Mensagens de erro do próprio Rust (ex.: estouro de pilha) vão pro arquivo também
            if let Ok(f) = std::fs::OpenOptions::new().append(true).open(path) {
                let h = f.into_raw_handle();
                let _ = SetStdHandle(STD_ERROR_HANDLE, HANDLE(h));
            }
        }
    }
}

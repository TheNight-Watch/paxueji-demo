//! 日志装配（main.py `setup_logging` 的移植）。
//!
//! - 格式与 Python `logging` 对齐：`%(asctime)s %(levelname)s %(name)s: %(message)s`
//!   （时间只到时分秒）；crate 的 `log::info!` 目标名映射成 Python 风格
//!   （`o5_session::session` -> `o5.session`）。
//! - 控制台直写 stdout（headless/journald 实时可见）；
//! - 文件走后台队列线程（TF 卡偶发写入慢会阻塞发日志的业务线程）；
//! - 4MB 轮转：超过直接重置，避免占满 eMMC。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

use log::{Level, LevelFilter, Log, Metadata, Record};
use o5_config::Config;

/// 日志文件大小上限（超过直接重置）。
const ROTATE_BYTES: u64 = 4 * 1024 * 1024;

struct LoggerHandle {
    /// 文件队列的发送端（无文件时 None）。drop 后写线程排空退出。
    tx: Option<Sender<String>>,
    handle: Option<JoinHandle<()>>,
}

struct O5Logger {
    level: LevelFilter,
    file: Mutex<LoggerHandle>,
}

static LOGGER: OnceLock<O5Logger> = OnceLock::new();

impl Log for O5Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {} {}: {}",
            chrono::Local::now().format("%H:%M:%S"),
            level_name(record.level()),
            py_name(record.target()),
            record.args()
        );
        // stdout 直写（对齐 Python 的 StreamHandler）
        println!("{line}");
        if let Some(tx) = self.file.lock().unwrap().tx.as_ref() {
            let _ = tx.send(line);
        }
    }

    fn flush(&self) {
        let _ = std::io::stdout().flush();
    }
}

/// Python `logging` 的级别名（WARNING 而非 Rust 的 WARN）。
fn level_name(l: Level) -> &'static str {
    match l {
        Level::Error => "ERROR",
        Level::Warn => "WARNING",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
        Level::Trace => "TRACE",
    }
}

/// crate 日志目标名 -> Python logger 名（`o5_session::session` -> `o5.session`）。
fn py_name(target: &str) -> String {
    let last = target.rsplit("::").next().unwrap_or(target);
    if let Some(rest) = last.strip_prefix("o5_") {
        // 单段 crate 名（lib.rs / main.rs 顶层）：o5_delegation -> o5.delegation
        format!("o5.{rest}")
    } else if target.contains("::") {
        // 模块路径取末段：o5_audio::audio_io -> o5.audio_io
        format!("o5.{last}")
    } else {
        // 显式 target（本 crate 用 "o5.main" 等，对齐 main.py 的 getLogger）
        target.to_string()
    }
}

/// 解析配置里的日志级别（Python `getattr(logging, X, logging.INFO)`）。
fn parse_level(s: &str) -> LevelFilter {
    match s.trim().to_ascii_lowercase().as_str() {
        "debug" => LevelFilter::Debug,
        "info" => LevelFilter::Info,
        "warn" | "warning" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        _ => LevelFilter::Info,
    }
}

/// 日志文件写线程：收一行写一行，通道断开（shutdown 取出 sender）后退出。
fn file_writer(mut file: File, rx: Receiver<String>) {
    for line in rx {
        let _ = file.write_all(line.as_bytes());
        let _ = file.write_all(b"\n");
    }
    let _ = file.flush();
}

/// 装配日志（main.py `setup_logging`）。返回日志文件路径。
///
/// 只在首次调用时安装全局 logger（进程内重复调用返回已生效的路径）。
pub fn setup(cfg: &Config, log_file_override: Option<&str>) -> String {
    let level = parse_level(&cfg.log_level);
    let path: PathBuf = match log_file_override
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let p = cfg.log_file.trim();
            (!p.is_empty()).then(|| p.to_string())
        }) {
        Some(p) => PathBuf::from(p),
        None => {
            // 注意：/maixapp/tmp 会被 launcher 启动时清空，日志必须落在持久目录；
            // 设备部署路径 /root/o5_harness_rs/（可执行文件同目录）
            exe_dir().join("o5_harness_rs.log")
        }
    };
    install(level, &path);
    path.to_string_lossy().into_owned()
}

fn install(level: LevelFilter, path: &std::path::Path) {
    // 超过 4MB 直接重置，避免占满 eMMC
    if path.is_file() {
        if let Ok(md) = fs::metadata(path) {
            if md.len() > ROTATE_BYTES {
                let _ = fs::write(path, b"");
            }
        }
    }
    let (tx, handle): (Option<Sender<String>>, Option<JoinHandle<()>>) =
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(f) => {
                let (tx, rx) = std::sync::mpsc::channel::<String>();
                match std::thread::Builder::new()
                    .name("o5-log-file".to_string())
                    .spawn(move || file_writer(f, rx))
                {
                    Ok(h) => (Some(tx), Some(h)),
                    Err(_) => (None, None),
                }
            }
            Err(e) => {
                eprintln!("[warn] 无法写日志文件 {}: {e}", path.display());
                (None, None)
            }
        };
    let fresh = O5Logger {
        level,
        file: Mutex::new(LoggerHandle { tx, handle }),
    };
    if LOGGER.set(fresh).is_ok() {
        // 首次装配：接管全局 logger（重复 setup 只保留首份，进程单实例装配）
        let _ = log::set_logger(LOGGER.get().expect("刚放入"));
        log::set_max_level(level);
    }
}

/// 退出前排空文件队列（Python atexit 停 QueueListener 的对应物）。
pub fn shutdown() {
    let Some(logger) = LOGGER.get() else { return };
    let mut handle = logger.file.lock().unwrap();
    // 先断开发送端：写线程把队列排空后自然退出
    let tx = handle.tx.take();
    drop(tx);
    if let Some(h) = handle.handle.take() {
        let _ = h.join();
    }
    let _ = std::io::stdout().flush();
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_name_mapping() {
        assert_eq!(py_name("o5_session::session"), "o5.session");
        assert_eq!(py_name("o5_audio::audio_io"), "o5.audio_io");
        assert_eq!(py_name("o5_camera::feeder"), "o5.feeder");
        assert_eq!(py_name("o5_delegation"), "o5.delegation");
        assert_eq!(py_name("o5.main"), "o5.main");
        assert_eq!(py_name("o5_harness_rs::app"), "o5.app");
    }

    #[test]
    fn level_parsing_defaults_to_info() {
        assert_eq!(parse_level("DEBUG"), LevelFilter::Debug);
        assert_eq!(parse_level(" info "), LevelFilter::Info);
        assert_eq!(parse_level("warning"), LevelFilter::Warn);
        assert_eq!(parse_level("ERROR"), LevelFilter::Error);
        assert_eq!(parse_level("junk"), LevelFilter::Info);
    }

    #[test]
    fn level_names_match_python() {
        assert_eq!(level_name(Level::Warn), "WARNING");
        assert_eq!(level_name(Level::Info), "INFO");
    }
}

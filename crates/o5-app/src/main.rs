//! MaixCAM2 O5 Harness 入口（Python `main.py` 的 Rust 移植）。
//!
//! 前台（小脑）：o5 全双工实时语音/全模态会话 —— 屏幕触屏 UI + 麦克风 +
//! 扬声器 +（omni 模式）摄像头。
//! 后台（大脑）：delegate_task 委派 OpenAI 兼容模型，结果回填由前台口头转述。
//!
//! 用法：
//! - 默认：前台 UI 模式（MaixPy 应用 / launcher 启动）
//! - `--headless`：无屏常驻模式（systemd 部署用）
//! - `--config <path>`：指定配置文件
//! - `--mode omni`：覆盖媒体模式启动
//! - `--test-pc`：PC 联测（mock 音视频 + 本地 stub 服务端，跑一个完整会话）
//! - `--selftest`：设备硬件自检（麦克风/扬声器/显示/触摸/摄像头）
//!
//! 退出：UI 点「退出」、headless 下 SIGTERM、或会话失败。
//! Python 的 GIL 调参（setswitchinterval）/ GC 调参 / os.nice 不移植
//! （Rust 无 GIL；进程优先级由 systemd/launcher 配置负责）。

mod app;
mod cfgsave;
mod logging;
mod prompt;
mod selftest;
mod testpc;
mod watchdog;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use o5_config::Config;
use o5_perf::netmode;

use crate::app::App;

/// CLI 参数（main.py argparse 的移植；launcher 会附带传未知参数，全部忽略）。
#[derive(Default)]
struct Args {
    config: Option<String>,
    headless: bool,
    mode: Option<String>,
    log_file: Option<String>,
    test_pc: bool,
    selftest: bool,
}

const USAGE: &str = "usage: o5-harness-rs [-h] [--config CONFIG] [--headless] \
[--mode {audio,omni,video}] [--log-file LOG_FILE] [--test-pc] [--selftest]";

const HELP: &str = "MaixCAM2 O5 Harness\n\noptions:\n  \
  -h, --help            show this help message and exit\n  \
  --config CONFIG       配置文件路径（默认 /root/o5_config.json 或内置默认）\n  \
  --headless            无屏常驻模式（systemd）\n  \
  --mode {audio,omni,video}\n                        覆盖媒体模式\n  \
  --log-file LOG_FILE   覆盖日志文件路径\n  \
  --test-pc             PC 联测：mock 音视频 + 本地 stub 服务端，跑一个完整会话\n  \
  --selftest            设备硬件自检（麦克风/扬声器/显示/触摸/摄像头）";

/// 手工解析（argparse 的 parse_known_args 语义：未知参数忽略不报错；
/// --mode 非法取值按 argparse 报错退出 2）。
fn parse_args(argv: &[String]) -> Args {
    let mut args = Args::default();
    let mut i = 0;
    let take_value = |i: &mut usize, flag: &str| -> String {
        *i += 1;
        match argv.get(*i) {
            Some(v) => v.clone(),
            None => {
                eprintln!("{USAGE}\no5-harness-rs: error: argument {flag}: expected one argument");
                std::process::exit(2);
            }
        }
    };
    while i < argv.len() {
        let a = argv[i].clone();
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}\n\n{HELP}");
                std::process::exit(0);
            }
            "--config" => args.config = Some(take_value(&mut i, &a)),
            "--headless" => args.headless = true,
            "--mode" => {
                let v = take_value(&mut i, &a);
                if !matches!(v.as_str(), "audio" | "omni" | "video") {
                    eprintln!(
                        "{USAGE}\no5-harness-rs: error: argument --mode: invalid choice: '{v}' \
(choose from 'audio', 'omni', 'video')"
                    );
                    std::process::exit(2);
                }
                args.mode = Some(v);
            }
            "--log-file" => args.log_file = Some(take_value(&mut i, &a)),
            "--test-pc" => args.test_pc = true,
            "--selftest" => args.selftest = true,
            _ => {} // parse_known_args：launcher 附带传参（实测含空串），忽略
        }
        i += 1;
    }
    args
}

fn main() {
    let rc = run();
    // 排空日志文件队列（writer 线程 join 保证落盘，无需 Python 的 0.3s 缓冲）
    logging::shutdown();
    watchdog::finish();
    std::process::exit(rc);
}

/// 设备上从 launcher 图标启动的自救（03 实测两轮黑屏的根源）：
/// launcher GUI fork 本进程后随即退出，daemon 记录完也退出，systemd 清理
/// launcher.service 的 cgroup——若此时还留在 cgroup/进程组里，初始化阶段
/// （字体加载等，尚未进入 run_ui）就会被连带 SIGTERM。先确认自己在
/// launcher 的 cgroup 里，然后迁到根 cgroup + setsid 脱离进程组，最后杀
/// launcher_daemon（它残留会占住 IVPS 组 0，摄像头 vi init 失败）。
/// headless/systemd（自己的 unit）与 PC 环境不在 launcher cgroup，跳过。
fn escape_launcher_supervision() {
    let Ok(cg) = std::fs::read_to_string("/proc/self/cgroup") else {
        return;
    };
    if !cg.contains("launcher.service") {
        return; // headless systemd / PC 联测 / SSH 手动：不动
    }
    let pid = std::process::id().to_string();
    if std::fs::write("/sys/fs/cgroup/cgroup.procs", pid.as_bytes()).is_err() {
        // 迁移失败：绝不杀 daemon——systemd 清 unit 会把还在 cgroup 里的
        // 自己连带杀掉（app exit signal 15）
        log::warn!(target: "o5.main", "launcher cgroup 迁移失败，保持原状（不杀 daemon）");
        return;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::setsid(); // 脱离 GUI 的进程组（防 killpg 连坐）
    }
    // 必须 SIGKILL：daemon 的 TERM 处理器（on_signal → stop_run_app）会先把
    // 运行中的 app 按 pid 杀掉再退出（12:22 实测：cgroup+setsid 全逃了仍被
    // 杀，journal 里 daemon 死前还记了 "app exit with signal: 15"）
    let _ = std::process::Command::new("pkill")
        .args(["-9", "-f", "launcher_daemon"])
        .status();
    log::info!(target: "o5.main", "已脱离 launcher 监管（cgroup+会话），launcher_daemon 已击杀（腾摄像头管线）");
}

/// 入口主体（main.py `main()`）。
fn run() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv);

    // 设备产线/维修路径：不加载配置、不起会话
    if args.selftest {
        return selftest::run();
    }
    // PC 联测：整段流程自带配置（指向本地 stub 服务端）
    if args.test_pc {
        return testpc::run(args.log_file.as_deref());
    }

    let mut cfg = match Config::load(args.config.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("[error] 配置错误: {e}");
            return 2;
        }
    };
    if let Some(mode) = &args.mode {
        cfg.fg_mode = mode.clone();
    }
    // 网络模式：wifi=端点直连；usb=中继端口改写 + /etc/hosts 映射
    //（详见 o5-perf::netmode；失败不阻断启动，仅提示）
    let net_note = match netmode::sync_hosts(
        &cfg.net_mode,
        &cfg.net_usb_relay_host,
        &cfg.net_usb_relay_port_map,
    ) {
        Ok(()) => String::new(),
        Err(e) => format!("hosts 同步失败: {e}"),
    };

    let log_path = logging::setup(&cfg, args.log_file.as_deref());
    log::info!(
        target: "o5.main",
        "MaixCAM2 O5 Harness v{}（headless={}）",
        env!("CARGO_PKG_VERSION"),
        args.headless
    );
    log::info!(
        target: "o5.main",
        "配置: {}",
        cfg.source_path().unwrap_or("内置默认")
    );
    log::info!(
        target: "o5.main",
        "网络模式: {}{}{}",
        cfg.net_mode,
        if cfg.net_public { "（公网调用开）" } else { "" },
        if net_note.is_empty() {
            String::new()
        } else {
            format!("（{net_note}）")
        }
    );
    log::info!(
        target: "o5.main",
        "端点: {}  协议: {}  模式: {}",
        netmode::effective_fg_url(
            &cfg.fg_endpoint,
            cfg.net_public,
            &cfg.net_mode,
            &cfg.net_usb_relay_port_map
        ),
        if cfg.fg_protocol_auto {
            "auto（连接前 /v1/info 协商）".to_string()
        } else {
            format!("v{}", cfg.fg_protocol_version)
        },
        cfg.fg_mode
    );
    log::info!(target: "o5.main", "日志: {log_path}");
    escape_launcher_supervision();
    if cfg.bg_enabled {
        log::info!(
            target: "o5.main",
            "后台大脑: {} model={}",
            cfg.bg_endpoint,
            if cfg.bg_model.is_empty() { "(默认)".to_string() } else { cfg.bg_model.clone() }
        );
    } else {
        log::info!(target: "o5.main", "后台大脑: 未配置（delegate_task 不可用）");
    }

    // 音色（generate_audio 需要；assets 随可执行文件部署在 <exe_dir>/assets）
    const FALLBACK_VOICE: &str = "builtin:zh-female-bright.wav";
    let assets_dir = exe_dir().join("assets");
    if let Err(e) = cfg.load_voice(&assets_dir) {
        log::error!(target: "o5.main", "{e}");
        // 配置里的音色文件不在（比如 MiniCPM 标志音色没随仓库分发），改用仓库自带的开源音色
        cfg.fg_voice = FALLBACK_VOICE.to_string();
        if cfg.load_voice(&assets_dir).is_ok() {
            log::warn!(target: "o5.main", "改用仓库自带音色 {FALLBACK_VOICE}");
        } else {
            let has_tts = cfg.fg_init_payload.get("tts_prompt_audio").is_some_and(app::py_truthy);
            if !has_tts {
                return 2;
            }
        }
    }
    if let Some(voice) = cfg.voice().filter(|v| !v.is_empty()) {
        log::info!(
            target: "o5.main",
            "音色: {}（sha256 {}…, {} 字节 WAV）",
            voice.spec,
            voice.sha256.chars().take(12).collect::<String>(),
            voice.data.len()
        );
    }

    let cfg_path = cfg.source_path().map(str::to_string);
    let app = App::new(cfg, args.headless, cfg_path);
    let rc = if args.headless {
        app.run_headless()
    } else {
        app.run_ui()
    };
    // finally：两条 run 路径内部已 stop_session（幂等），此处兜底
    app.stop_session();
    rc
}

fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// 信号（headless：SIGTERM/SIGINT；linux 限定——Windows 无对应部署形态）
// ---------------------------------------------------------------------------

/// 收到的信号编号（handler 里只做原子写，异步信号安全）。
#[cfg(target_os = "linux")]
static SIGNAL_RECEIVED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(target_os = "linux")]
extern "C" fn on_signal(sig: libc::c_int) {
    use std::sync::atomic::Ordering;
    SIGNAL_RECEIVED.store(sig, Ordering::SeqCst);
}

/// 安装 SIGTERM/SIGINT（Python `run_headless` 的 signal.signal）。
pub(crate) fn install_signal_handlers(_shutdown: &Arc<AtomicBool>) {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
    }
}

/// 轮询信号标记（headless 主循环里调用）：置 shutdown 并记日志。
pub(crate) fn poll_signal_flag(shutdown: &Arc<AtomicBool>) {
    #[cfg(target_os = "linux")]
    {
        use std::sync::atomic::Ordering;
        let sig = SIGNAL_RECEIVED.swap(0, Ordering::SeqCst);
        if sig != 0 && !shutdown.load(Ordering::SeqCst) {
            log::info!(target: "o5.main", "收到信号 {sig}，准备退出");
            shutdown.store(true, Ordering::SeqCst);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = shutdown;
    }
}

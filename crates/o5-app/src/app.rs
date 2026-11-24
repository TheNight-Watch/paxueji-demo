//! App：应用内核（main.py `App` / `AppState` 的移植）。
//!
//! 组合全部库 crate：音频（o5-audio）、相机（o5-camera）、会话（o5-session）、
//! 转写（o5-asr）、工具（o5-tools）、后台大脑（o5-delegation）与 UI 状态
//! （o5-ui）。UI 共享状态与 Python `AppState` 同构（`SharedUiState` 即其移植），
//! 另在 App 侧维护一份转写镜像（供回声过滤 / 口播回合日志读取——
//! Python 靠 GIL 直接读 `state.transcript`）。
//!
//! 线程模型：
//!  - session 的 listener 回调全部来自 reader 线程（必须快）：on_audio 只入队，
//!    本地工具 spawn 工作线程，delegate_task 是 delegation 的快路径；
//!  - 清理（stop_session）放后台线程（阻塞步骤合计 1~3s，主线程直接跑会冻住
//!    UI 节拍），下一次 start_session 等它收尾（≤6s）防止新旧实例抢音频设备；
//!  - 相机开关由 UI 直接翻转 `state.camera_on`，本层轮询观察启停
//!    （Rust 的 CameraFeed 构造即启动、stop 后不可复用，启停 = 换新实例）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use o5_asr::AsrClient;
use o5_audio::device::platform_capture;
use o5_audio::AudioIo;
use o5_camera::CameraFeed;
use o5_config::Config;
use o5_delegation::DelegationManager;
use o5_perf::netmode;
use o5_perf::{PerfMonitor, Timing};
use o5_protocol::ParsedToolCall;
use o5_session::{O5Session, SessionListener, SessionState};
use o5_tools::ToolExecutor;
use o5_ui::{Image, SessionPhase, SharedUiState, TranscriptKind, UiEvent};
use serde_json::Value;

use crate::prompt;

mod paxueji;

/// 开口兜底节拍：回填后等模型开口的时限（cpm-harness speakNudge 口径）。
pub const NUDGE_TIMEOUT_S: f64 = 5.0;
/// 开口兜底：最多戳的次数。
pub const NUDGE_MAX: u32 = 2;
/// 转写镜像容量（Python `_trim` 的 80 条）。
const TRANSCRIPT_CAP: usize = 80;

/// 相机工厂（设备 = `CameraFeed::from_config`；`--test-pc` 注入 MockFrameSource）。
pub type CameraFactory =
    Arc<dyn Fn(&Config, Arc<Timing>) -> Option<CameraFeed> + Send + Sync>;

/// 转写镜像（Python `AppState.transcript` 的可读副本 + 行开合状态）。
#[derive(Default)]
struct TranscriptMirror {
    entries: VecDeque<(String, TranscriptKind)>,
    line_open: bool,
    user_line_open: bool,
}

/// 开口兜底状态（Python `_nudge_timer/_nudge_count/_nudge_spoken`）。
struct NudgeState {
    armed: bool,
    spoken: bool,
    count: u32,
    deadline: Instant,
}

impl Default for NudgeState {
    fn default() -> Self {
        NudgeState {
            armed: false,
            spoken: false,
            count: 0,
            deadline: Instant::now(),
        }
    }
}

/// 集成验证 / 诊断用的轻量计数（`--test-pc` 断言口径）。
#[derive(Default)]
pub struct AppStats {
    states: Mutex<Vec<SessionState>>,
    spoken_finals: AtomicU32,
    spoken_text: Mutex<String>,
    audio_chunks: AtomicU32,
    audio_rate: AtomicU32,
    tool_calls: Mutex<Vec<String>>,
}

impl AppStats {
    fn note_audio(&self, rate: u32) {
        self.audio_chunks.fetch_add(1, Ordering::Relaxed);
        self.audio_rate.store(rate, Ordering::Relaxed);
    }

    fn note_tool_call(&self, call: &ParsedToolCall) {
        self.tool_calls
            .lock()
            .unwrap()
            .push(format!("{}:{}", call.name, call.arguments));
    }

    fn note_spoken(&self, text: &str, final_: bool) {
        if final_ {
            self.spoken_finals.fetch_add(1, Ordering::Relaxed);
        }
        if !text.is_empty() {
            self.spoken_text.lock().unwrap().push_str(text);
        }
    }
}

/// 前台应用（main.py `App`）。构造后用 [`App::run_ui`] / [`App::run_headless`] 驱动。
pub struct App {
    cfg: RwLock<Config>,
    /// 自引用（回调闭包 / 线程持有 Weak，随 Arc<App> 生命周期）。
    weak: Weak<App>,
    headless: bool,
    /// 网络模式 / 音量等切换时写回配置文件。
    cfg_path: Option<String>,
    pub state: SharedUiState,
    timing: Arc<Timing>,
    /// UI 性能面板用的一份（10s `[perf]` 周期行线程在构造时启动）。
    perf: PerfMonitor,
    audio: Mutex<AudioIo>,
    camera: Mutex<Option<CameraFeed>>,
    camera_factory: CameraFactory,
    /// 相机已确认失败（feeder 拉不起）：停止自动重试直到下一次会话启动。
    camera_failed: AtomicBool,
    camera_fail_noted: AtomicBool,
    fg_tools: Arc<ToolExecutor>,
    delegation: DelegationManager,
    session: Mutex<Option<Arc<O5Session>>>,
    asr: Mutex<Option<Arc<AsrClient>>>,
    /// 当前媒体模式（Python `state.mode` 的镜像；session 构造快照取 cfg.fg_mode）。
    mode: Mutex<String>,
    muted: AtomicBool,
    /// 未拼接的原始任务提示词（start_session 每次基于它组装后写进 cfg）。
    base_task_prompt: String,
    transcript: Mutex<TranscriptMirror>,
    session_started_at: Mutex<Option<Instant>>,
    /// stop_session 幂等守卫（run 收尾与 main 的 finally 会各调一次）。
    stopped: AtomicBool,
    /// 上一轮后台清理的完成信号（start_session 前等待，防音频设备竞争）。
    stop_done: Mutex<Option<Receiver<()>>>,
    /// 同上，可等待版（--test-pc 断言优雅关闭用）。
    stop_complete: Arc<(Mutex<bool>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    exit_code: Mutex<i32>,
    last_state: Mutex<SessionState>,
    /// 服务端/内部错误文案（Rust 契约 on_state 无 detail，用它补「会话失败: …」）。
    last_error: Mutex<Option<String>>,
    nudge: Mutex<NudgeState>,
    /// 最近一条 user final 上屏时刻——迟到重复 final 去重窗口的锚点。
    last_user_final_at: Mutex<Option<Instant>>,
    pub stats: AppStats,
    paxueji: Mutex<paxueji::Paxueji>,
}

/// Python 真值语义（bool(x)：null/false/0/""/空容器为假）。
pub(crate) fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// 字符截断（Python `text[:n]` 的字符口径）。
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// SessionState -> UI 的 SessionPhase。
fn phase_of(s: SessionState) -> SessionPhase {
    match s {
        SessionState::Idle => SessionPhase::Idle,
        SessionState::Connecting => SessionPhase::Connecting,
        SessionState::Waiting => SessionPhase::Waiting,
        SessionState::Initializing => SessionPhase::Initializing,
        SessionState::Ready => SessionPhase::Ready,
        SessionState::Closing => SessionPhase::Closing,
        SessionState::Closed => SessionPhase::Closed,
        SessionState::Failed => SessionPhase::Failed,
    }
}

/// 状态字符串（Python state 机文案键）。
fn state_str(s: SessionState) -> &'static str {
    phase_of(s).as_str()
}

/// 默认相机工厂：video.enabled 时走设备 feeder（CameraFeed::from_config）。
fn default_camera_factory(cfg: &Config, timing: Arc<Timing>) -> Option<CameraFeed> {
    cfg.video_enabled.then(|| CameraFeed::from_config(cfg, timing))
}

impl App {
    /// 构造（main.py `App.__init__` 的装配部分；GC 调优 / maix 导入不移植）。
    pub fn new(cfg: Config, headless: bool, cfg_path: Option<String>) -> Arc<App> {
        Self::with_camera_factory(cfg, headless, cfg_path, None)
    }

    /// 带相机工厂的构造（`--test-pc` 注入 MockFrameSource）。
    pub fn with_camera_factory(
        cfg: Config,
        headless: bool,
        cfg_path: Option<String>,
        camera_factory: Option<CameraFactory>,
    ) -> Arc<App> {
        Arc::new_cyclic(|weak: &Weak<App>| {
            let timing = Arc::new(Timing::new());
            // [perf] 周期行（CPU/线程/wifi/收发速率/各环节耗时），常驻输出
            let perf = PerfMonitor::new(Arc::clone(&timing));
            perf.clone().spawn();
            // 纯网络 RTT 探测（TCP 握手，不含应用层）：把 ACK 拆成 网络/服务端。
            // 每轮读当前 cfg（公网调用开关切换后端点立即跟随）
            let weak_probe = weak.clone();
            netmode::NetRttProbe::new().spawn(
                move || {
                    weak_probe.upgrade().map_or_else(String::new, |app| {
                        let cfg = app.cfg.read().unwrap();
                        netmode::effective_fg_url(
                            &cfg.fg_endpoint,
                            cfg.net_public,
                            &cfg.net_mode,
                            &cfg.net_usb_relay_port_map,
                        )
                    })
                },
                Arc::clone(&timing),
                5.0,
            );

            let state = SharedUiState::new();
            state.set_network_mode(&cfg.net_mode);
            state.set_public_net(cfg.net_public);
            state.set_asr_on(cfg.asr_enabled);
            state.set_device_label(&cfg.device_label);
            state.set_mode(&cfg.fg_mode);
            state.set_camera_on(cfg.video_enabled);
            state.set_volume(cfg.audio_output_volume);
            if cfg.bg_enabled {
                state.set_background_line("后台: 已配置");
            }

            let audio = AudioIo::new(
                &cfg,
                Arc::clone(&timing),
                platform_capture(cfg.audio_input_volume),
            );
            let fg_tools = Arc::new(
                ToolExecutor::new(
                    &cfg.tools_tavily_api_key,
                    &cfg.tools_serper_api_key,
                    &cfg.tools_brave_api_key,
                    &cfg.tools_amap_api_key,
                    &cfg.tools_amap_service_host,
                )
                .with_vacuum(cfg.vacuum_enabled, &cfg.vacuum_url, &cfg.vacuum_token, &cfg.vacuum_entity_id),
            );
            // 后台大脑回填通道：结果从后台任务线程经 _backfill 回投
            // （开口兜底统一在这里挂）
            let weak_delegation = weak.clone();
            let delegation = DelegationManager::new(
                cfg.bg_enabled,
                netmode::effective_url(
                    &cfg.bg_endpoint,
                    &cfg.net_mode,
                    &cfg.net_usb_relay_port_map,
                ),
                cfg.bg_api_key.clone(),
                cfg.bg_model.clone(),
                cfg.bg_timeout_s,
                cfg.bg_max_concurrent,
                cfg.bg_system_prompt.clone(),
                cfg.bg_max_result_chars,
                cfg.bg_request_parameters.clone(),
                Some(Arc::clone(&fg_tools)),
                Box::new(move |tool_call_id: &str, text: &str| {
                    if let Some(app) = weak_delegation.upgrade() {
                        app.backfill(tool_call_id, text);
                    }
                }),
            );

            let base_task_prompt = cfg.fg_task_prompt.clone();
            let mode = cfg.fg_mode.clone();
            let app = App {
                cfg: RwLock::new(cfg),
                weak: weak.clone(),
                headless,
                cfg_path,
                state,
                timing,
                perf,
                audio: Mutex::new(audio),
                camera: Mutex::new(None),
                camera_factory: camera_factory.unwrap_or_else(|| Arc::new(default_camera_factory)),
                camera_failed: AtomicBool::new(false),
                camera_fail_noted: AtomicBool::new(false),
                fg_tools,
                delegation,
                session: Mutex::new(None),
                asr: Mutex::new(None),
                mode: Mutex::new(mode),
                muted: AtomicBool::new(false),
                base_task_prompt,
                transcript: Mutex::new(TranscriptMirror::default()),
                session_started_at: Mutex::new(None),
                stopped: AtomicBool::new(false),
                stop_done: Mutex::new(None),
                stop_complete: Arc::new((Mutex::new(true), Condvar::new())),
                shutdown: Arc::new(AtomicBool::new(false)),
                exit_code: Mutex::new(0),
                last_state: Mutex::new(SessionState::Idle),
                last_error: Mutex::new(None),
                nudge: Mutex::new(NudgeState::default()),
                last_user_final_at: Mutex::new(None),
                stats: AppStats::default(),
                paxueji: Mutex::new(paxueji::Paxueji::default()),
            };
            // 开口兜底线程（Python 的 threading.Timer 链）
            let weak_nudge = weak.clone();
            let spawned = std::thread::Builder::new()
                .name("o5-nudge".to_string())
                .spawn(move || nudge_loop(weak_nudge));
            if let Err(e) = spawned {
                log::error!(target: "o5.main", "开口兜底线程启动失败：{e}");
            }
            // 03 固件：display feeder 先 init 会占住 IVPS 组 0，camera 后起
            // 必败（vi init failed）。omni/video 模式提前拉 camera——UI 线程
            // 的 display feeder 在此之后，反向共存（02 无此问题）。
            if *app.mode.lock().unwrap() != "audio"
                && app.cfg.read().unwrap().video_enabled
                && app.state.camera_on()
            {
                app.ensure_camera();
            }
            app
        })
    }

    // ------------------------------------------------------------------
    // 只读访问
    // ------------------------------------------------------------------

    pub fn timing(&self) -> &Arc<Timing> {
        &self.timing
    }

    pub fn last_session_state(&self) -> SessionState {
        *self.last_state.lock().unwrap()
    }

    pub fn state_history(&self) -> Vec<SessionState> {
        self.stats.states.lock().unwrap().clone()
    }

    pub fn spoken_finals(&self) -> u32 {
        self.stats.spoken_finals.load(Ordering::Relaxed)
    }

    pub fn spoken_text(&self) -> String {
        self.stats.spoken_text.lock().unwrap().clone()
    }

    pub fn audio_stats(&self) -> (u32, u32) {
        (
            self.stats.audio_chunks.load(Ordering::Relaxed),
            self.stats.audio_rate.load(Ordering::Relaxed),
        )
    }

    pub fn tool_calls(&self) -> Vec<String> {
        self.stats.tool_calls.lock().unwrap().clone()
    }

    /// 会话是否进行中（Python `session.is_running`）。
    fn session_running(&self) -> bool {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| session_is_running(s))
    }

    // ------------------------------------------------------------------
    // 会话生命周期（main.py start_session / stop_session）
    // ------------------------------------------------------------------

    pub fn start_session(&self) {
        if self.session_running() {
            log::warn!(target: "o5.main", "会话已在运行，忽略重复启动");
            return;
        }
        // 等上一轮清理收尾（stop_session 已后台化）：快速重启会话时
        // 防止新旧实例竞争音频设备。上限 6s ≈ 各 join 超时之和。
        if let Some(rx) = self.stop_done.lock().unwrap().take() {
            let _ = rx.recv_timeout(Duration::from_secs(6));
        }
        self.stopped.store(false, Ordering::SeqCst);

        // 音色门槛：generate_audio 需要 tts_prompt_audio
        let (has_voice, has_tts) = {
            let cfg = self.cfg.read().unwrap();
            (
                cfg.voice().is_some_and(|v| !v.is_empty()),
                cfg.fg_init_payload
                    .get("tts_prompt_audio")
                    .is_some_and(py_truthy),
            )
        };
        if !has_voice && !has_tts {
            let msg = "generate_audio 需要 tts_prompt_audio：请在配置 foreground.voice 指定音色（默认 builtin:ref_minicpm_signature_fix.wav）";
            log::error!(target: "o5.main", "{msg}");
            self.add_sys(msg);
            if self.headless {
                *self.exit_code.lock().unwrap() = 1;
                self.shutdown.store(true, Ordering::SeqCst);
            }
            return;
        }

        // 前台工具面：本地工具 + （配置了后台时）delegate_task，按训练顺序声明
        let (tools, tool_names) = {
            let cfg = self.cfg.read().unwrap();
            prompt::build_session_tools(&cfg, &self.fg_tools)
        };

        self.audio.lock().unwrap().start(); // start_speaker
        // tts-center 实时转写（可选）：与前台共用麦克风单元（线程不可重启，随会话重建）
        if self.cfg.read().unwrap().asr_enabled {
            self.ensure_asr();
        }
        *self.session_started_at.lock().unwrap() = Some(Instant::now());

        let Some(listener_self) = self.weak.upgrade() else { return };
        let session = {
            let mut cfg = self.cfg.write().unwrap();
            // 任务提示词组装（双工协议 + 任务提示词 + 委派纪律 + 工具路由 + 委派协议）
            cfg.fg_task_prompt = prompt::compose_task_prompt(&self.base_task_prompt, &tool_names);
            O5Session::new(
                &cfg,
                listener_self,
                Some(Timing::clone(&self.timing)),
                Some(tools),
            )
        };
        let session = Arc::new(session);
        *self.session.lock().unwrap() = Some(Arc::clone(&session));
        self.add_sys("小松果老师来啦，正在连接…");
        session.start();
    }

    /// 建立并启动 ASR 客户端（一次性实例：stop 后不可重启，关→开需重建）。
    /// start_session 与设置页开关共用。
    fn ensure_asr(&self) {
        if self.asr.lock().unwrap().is_some() {
            return;
        }
        let (endpoint, api_key, service_id, max_reconnects, net_mode, port_map) = {
            let cfg = self.cfg.read().unwrap();
            (
                cfg.asr_endpoint.clone(),
                cfg.asr_api_key.clone(),
                cfg.asr_service_id,
                cfg.asr_max_reconnects,
                cfg.net_mode.clone(),
                cfg.net_usb_relay_port_map.clone(),
            )
        };
        let weak_text = self.weak.clone();
        let on_text: Box<dyn Fn(String, bool) + Send> = Box::new(move |text, final_| {
            if let Some(app) = weak_text.upgrade() {
                app.on_asr_text(text, final_);
            }
        });
        let on_status: Box<dyn Fn(&str) + Send> = Box::new(|s: &str| {
            log::info!(target: "o5.main", "[ASR] {s}");
        });
        let asr = AsrClient::new(
            netmode::effective_url(&endpoint, &net_mode, &port_map),
            api_key,
            service_id,
            max_reconnects,
            true, // tls_insecure：保持与 Python 一致
            on_text,
            on_status,
        );
        asr.start();
        *self.asr.lock().unwrap() = Some(Arc::new(asr));
    }

    pub fn stop_session(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.cancel_speak_nudge();
        self.state.close_model_line();
        // 清理放后台线程：阻塞步骤合计 1~3s（asr/session WS 关闭 + stop_mic 的
        // join(2.0) + 相机 join(2.5) + speaker join(3.0)），主线程直接跑会冻住节拍。
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        *self.stop_done.lock().unwrap() = Some(done_rx);
        *self.stop_complete.0.lock().unwrap() = false;
        let weak = self.weak.clone();
        let stop_complete = Arc::clone(&self.stop_complete);
        let spawned = std::thread::Builder::new()
            .name("o5-stop".to_string())
            .spawn(move || {
                let app = weak.upgrade();
                if let Some(app) = app.as_ref() {
                    let asr = app.asr.lock().unwrap().take();
                    if let Some(a) = asr {
                        a.stop();
                    }
                    let s = app.session.lock().unwrap().take();
                    if let Some(s) = s {
                        s.stop();
                    }
                    app.audio.lock().unwrap().stop_mic();
                    app.audio.lock().unwrap().stop_speaker();
                }
                let _ = done_tx.send(());
                let mut done = stop_complete.0.lock().unwrap();
                *done = true;
                stop_complete.1.notify_all();
            });
        if let Err(e) = spawned {
            // 线程都起不来：退化为同步清理，别让会话泄漏
            log::error!(target: "o5.main", "清理线程启动失败（同步清理）：{e}");
            let asr = self.asr.lock().unwrap().take();
            if let Some(a) = asr {
                a.stop();
            }
            let s = self.session.lock().unwrap().take();
            if let Some(s) = s {
                s.stop();
            }
            self.audio.lock().unwrap().stop_mic();
            self.audio.lock().unwrap().stop_speaker();
            *self.stop_complete.0.lock().unwrap() = true;
            self.stop_complete.1.notify_all();
        }
    }

    /// 等 stop_session 的后台清理收尾（--test-pc 断言用；Python 无此等待）。
    pub fn wait_stop_complete(&self, timeout: Duration) -> bool {
        let mut done = self.stop_complete.0.lock().unwrap();
        if *done {
            return true;
        }
        let (guard, timeout_result) = self
            .stop_complete
            .1
            .wait_timeout(done, timeout)
            .unwrap_or_else(|e| e.into_inner());
        done = guard;
        *done || !timeout_result.timed_out()
    }

    // ------------------------------------------------------------------
    // 媒体启停（main.py _start_media / _stop_media）
    // ------------------------------------------------------------------

    fn start_media(&self) {
        // 相机失败标记随会话重置（同 Python 每次 _start_media 都试 camera.start()）
        self.camera_failed.store(false, Ordering::SeqCst);
        let Some(app) = self.weak.upgrade() else { return };
        let on_unit: Box<dyn FnMut(Vec<u8>, Instant) + Send> =
            Box::new(move |pcm16, t_recorded| {
                app.on_mic_unit(pcm16, t_recorded);
            });
        self.audio.lock().unwrap().start_mic(on_unit);
        let mode = self.mode.lock().unwrap().clone();
        let camera_on = self.state.camera_on();
        if mode != "audio" && camera_on {
            self.ensure_camera();
        }
    }

    /// 拍学机：摄像头跟应用同生命周期（取景框不随会话断开而黑掉；
    /// 03 固件上中途重开摄像头还会撞 IVPS 初始化顺序），这里只停麦克风。
    fn stop_media(&self) {
        self.audio.lock().unwrap().stop_mic();
    }

    /// 启动相机（Rust 的 CameraFeed 构造即启动；失败由轮询观察补报）。
    fn ensure_camera(&self) {
        if self.camera_failed.load(Ordering::Relaxed) {
            return;
        }
        if self.camera.lock().unwrap().is_some() {
            return;
        }
        let mode = self.mode.lock().unwrap().clone();
        if mode == "audio" || !self.state.camera_on() {
            return;
        }
        let (cfg, timing) = {
            let cfg = self.cfg.read().unwrap().clone();
            (cfg, Arc::clone(&self.timing))
        };
        if !cfg.video_enabled {
            return;
        }
        match (self.camera_factory)(&cfg, timing) {
            Some(feed) => {
                *self.camera.lock().unwrap() = Some(feed);
            }
            None => {
                self.note_camera_failure();
            }
        }
    }

    /// 03 固件上 camera 与 display 谁先 init 谁占 IVPS 组 0，后到的失败。
    /// camera 的 MaixPy 喂流 import 要几秒，必须等它出第一帧再拉起 display。
    fn wait_camera_first_frame(&self, limit: Duration) {
        let t0 = Instant::now();
        while t0.elapsed() < limit {
            let ready = match self.camera.lock().unwrap().as_ref() {
                Some(c) => c.frames_read() > 0 || !c.is_alive(),
                None => true,
            };
            if ready {
                log::info!(target: "o5.main", "摄像头就绪等待 {:.1}s", t0.elapsed().as_secs_f64());
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        log::warn!(target: "o5.main", "摄像头 {}s 内没出帧，照常启动界面", limit.as_secs());
    }

    fn stop_camera(&self) {
        let cam = self.camera.lock().unwrap().take();
        if let Some(mut c) = cam {
            c.stop();
        }
        self.state.set_cam_preview(None);
    }

    /// 相机死亡（feeder 拉不起 / EOF）：提示一次并停止自动重试。
    fn note_camera_failure(&self) {
        self.stop_camera();
        self.camera_failed.store(true, Ordering::Relaxed);
        if !self.camera_fail_noted.swap(true, Ordering::Relaxed) {
            self.add_sys("摄像头启动失败（继续纯语音）");
        }
    }

    // ------------------------------------------------------------------
    // 转写镜像（Python AppState 的 add_* 语义；同步转发给 UI 状态）
    // ------------------------------------------------------------------

    fn add_sys(&self, text: &str) {
        self.state.add_sys(text);
        let mut tr = self.transcript.lock().unwrap();
        tr.line_open = false;
        tr.user_line_open = false;
        tr.entries.push_back((text.to_string(), TranscriptKind::Sys));
        trim_transcript(&mut tr);
    }

    fn add_model(&self, text: &str) {
        self.state.add_model(text);
        let mut tr = self.transcript.lock().unwrap();
        if tr.line_open && !tr.entries.is_empty() {
            tr.entries.back_mut().unwrap().0.push_str(text);
        } else {
            tr.entries
                .push_back((text.to_string(), TranscriptKind::Model));
            tr.line_open = true;
        }
        trim_transcript(&mut tr);
    }

    fn close_model_line(&self) {
        self.state.close_model_line();
        self.transcript.lock().unwrap().line_open = false;
    }

    fn add_user(&self, text: &str, final_: bool) {
        if text.is_empty() {
            return;
        }
        self.state.add_user(text, final_);
        let mut tr = self.transcript.lock().unwrap();
        if tr.user_line_open
            && tr.entries.back().map(|(_, k)| *k) == Some(TranscriptKind::User)
        {
            tr.entries.back_mut().unwrap().0 = text.to_string();
        } else {
            tr.entries
                .push_back((text.to_string(), TranscriptKind::User));
        }
        tr.user_line_open = !final_;
        if final_ {
            *self.last_user_final_at.lock().unwrap() = Some(Instant::now());
        }
        trim_transcript(&mut tr);
    }

    // ------------------------------------------------------------------
    // 回填 + 开口兜底（main.py _backfill / armSpeakNudge）
    // ------------------------------------------------------------------

    fn backfill(&self, tool_call_id: &str, text: &str) {
        if let Some(s) = self.session.lock().unwrap().as_ref() {
            s.send_tool_result(tool_call_id, text);
        }
        self.arm_speak_nudge();
    }

    fn arm_speak_nudge(&self) {
        let mut n = self.nudge.lock().unwrap();
        n.spoken = false;
        n.count = 0;
        n.armed = true;
        n.deadline = Instant::now() + Duration::from_secs_f64(NUDGE_TIMEOUT_S);
    }

    fn schedule_speak_nudge(&self) {
        let mut n = self.nudge.lock().unwrap();
        n.armed = true;
        n.deadline = Instant::now() + Duration::from_secs_f64(NUDGE_TIMEOUT_S);
    }

    fn cancel_speak_nudge(&self) {
        self.nudge.lock().unwrap().armed = false;
    }

    fn nudge_spoken(&self) {
        self.nudge.lock().unwrap().spoken = true;
    }

    fn on_turn_end(&self) {
        self.close_model_line();
        self.state.set_think_active(false);
        self.state.set_background_line(&self.delegation.status_line());
        // 没有屏幕的场景（headless / 联测复盘）把整段口播落进日志
        let last = {
            let tr = self.transcript.lock().unwrap();
            tr.entries.back().map(|(t, k)| (t.clone(), *k))
        };
        if let Some((text, kind)) = last {
            if kind == TranscriptKind::Model && !text.trim().is_empty() {
                log::info!(target: "o5.main", "[口播回合] {}", truncate_chars(&text, 200));
            }
        }
    }

    fn check_session_minutes(&self) {
        let limit = self.cfg.read().unwrap().audio_max_session_minutes;
        if limit <= 0.0 {
            return;
        }
        let started = *self.session_started_at.lock().unwrap();
        if let Some(t0) = started {
            if t0.elapsed().as_secs_f64() / 60.0 >= limit {
                self.add_sys(&format!("会话达到时长上限 {limit:.0} 分钟，已停止"));
            }
        }
    }

    // ------------------------------------------------------------------
    // 麦克风 unit -> 会话（mic 线程回调，必须快：只投递）
    // ------------------------------------------------------------------

    fn on_mic_unit(&self, pcm16: Vec<u8>, t_recorded: Instant) {
        let Some(sess) = self.session.lock().unwrap().clone() else {
            return;
        };
        let pcm16 = self.injected_unit(pcm16);
        if o5_perf::trace::enabled() {
            let n = (pcm16.len() / 2).max(1) as f64;
            let sum: f64 = pcm16
                .chunks_exact(2)
                .map(|b| f64::from(i16::from_le_bytes([b[0], b[1]])).powi(2))
                .sum();
            o5_perf::trace_ev!("mic_unit", rms = (sum / n).sqrt() as u32);
        }
        // 拍学机只在按下快门后让模型看画面：取景时不上行摄像头帧（协议层补黑帧），
        // 否则模型会自己对着取景画面讲起来，抢在照片讲解前面。
        let video: Option<String> = self.uplink_frame();
        // tts-center 转写：扬声器出声时不喂真实音频（回声抑制），打断语音
        // 不上字幕。gate 期间改喂等长静音而非断流——断流会让服务端把残余
        // 解码缓冲再 flush 一条重复 final（用户句在 AI 回复后"再出现一次"
        // 的根源，02 实测）。
        let asr = self.asr.lock().unwrap().clone();
        let feed_pcm = asr.as_ref().map(|_| pcm16.clone());
        sess.offer_unit(pcm16, video, t_recorded);
        self.flush_shutter_prompt(&sess);
        if let (Some(a), Some(p)) = (asr, feed_pcm) {
            let (gate, tail_s) = {
                let cfg = self.cfg.read().unwrap();
                (
                    cfg.asr_gate_while_playing,
                    cfg.audio_gate_tail_ms as f64 / 1000.0,
                )
            };
            let playing = gate && self.audio.lock().unwrap().playing(tail_s.max(1.2));
            if playing {
                a.feed(vec![0u8; p.len()]);
            } else {
                a.feed(p);
            }
        }
    }

    // ------------------------------------------------------------------
    // ASR 转写回调（连接线程）：回声过滤 + 字幕
    // ------------------------------------------------------------------

    fn on_asr_text(&self, text: String, final_: bool) {
        if text.is_empty() {
            return;
        }
        // partial 与 final 都过回声过滤：gate 尾沿后到达的回声尾巴会走 final
        // 路径上屏（同样造成"显示两遍"）
        if self.echo_filtered(&text) {
            if final_ {
                log::info!(target: "o5.main", "[ASR 回声过滤-final] {}", truncate_chars(&text, 60));
            }
            return;
        }
        // 迟到重复也判 partial：重复转写常以 partial 先到（先 push 成第二
        // 行），final 随后才被拦就晚了（02 实测"能看到，前面"即此类）。
        if self.user_dup_filtered(&text) {
            log::info!(target: "o5.main", "[ASR 迟到去重{}] {}", if final_ { "" } else { "-partial" }, truncate_chars(&text, 60));
            return;
        }
        self.add_user(&text, final_);
        if final_ {
            log::info!(target: "o5.main", "[ASR 用户] {}", truncate_chars(&text, 200));
        }
    }

    /// 迟到重复判定：最近一条 user 行已关闭（final 已上屏）且内容高度相似
    /// 且间隔 <4s——ASR 断流/积压场景下服务端把残余解码缓冲再 flush 一条
    /// 重复转写（partial 常先到），到达时口播常刚结束（时机门已关拦不
    /// 住），add_user 会 push 一条重复行（用户句在 AI 回复后"再出现一
    /// 次"）。间隔长/不相似的是用户真的在重复问，照常上屏。
    /// user_line_open 时 final 走覆盖路径，不会产生重复行，直接放行。
    fn user_dup_filtered(&self, text: &str) -> bool {
        let within = self
            .last_user_final_at
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < Duration::from_secs(4));
        if !within {
            return false;
        }
        let tr = self.transcript.lock().unwrap();
        if tr.user_line_open {
            return false;
        }
        // 迟到转写常在 AI 回复上屏之后到达——最近一条 user 行在 model
        // 行前面，不能只看 back()
        let Some(prev) = tr
            .entries
            .iter()
            .rev()
            .filter(|(_, k)| *k == TranscriptKind::User)
            .map(|(t, _)| t)
            .next()
        else {
            return false;
        };
        // 覆盖率（交集/min）比 Jaccard 强：回声拼接句"用户问题+AI 回答"
        // 对用户原句覆盖率 =1.0；用户改问的追问 <0.5
        text_coverage(text, prev) > 0.72
    }

    /// 回声转写判定：与 AI 最近口播内容（model 行）高度相似 = 刚出声的话被录回
    /// 转写，不上屏。
    /// 时机门 3.0s（02 实测回声转写要 2-3s 才到——转写延迟+句尾静音判定，
    /// Python 标定的 1.5s 不够）；相似度 Jaccard>0.5（长句完整回声）或
    /// 覆盖率>0.72（短前缀/拼接句——Jaccard 对"短 vs 长"结构性失效，
    /// 02 实测 partial"能看到，前面"仅 0.29）。
    fn echo_filtered(&self, text: &str) -> bool {
        if !self.audio.lock().unwrap().playing(3.0) {
            return false;
        }
        let tr = self.transcript.lock().unwrap();
        for (t, kind) in tr.entries.iter().rev().take(4) {
            if *kind == TranscriptKind::Model {
                let sim = text_similar(text, t);
                let cov = text_coverage(text, t);
                if sim > 0.5 || cov > 0.72 {
                    log::info!(target: "o5.main", "[ASR 回声过滤] sim={sim:.2} cov={cov:.2} {}", truncate_chars(text, 60));
                    return true;
                }
            }
        }
        false
    }

    // ------------------------------------------------------------------
    // 轮询（Python UI 循环里的 poll()：电平 / 后台行 / 相机预览 + 相机开关观察）
    // ------------------------------------------------------------------

    pub fn poll_tick(&self) {
        let (db, rms) = {
            let a = self.audio.lock().unwrap();
            (a.level().db(), a.level().rms())
        };
        self.state.set_mic_level(db, rms);
        // 无条件刷新：任务完成后也要把"后台任务 xN"刷回"后台空闲"
        self.state.set_background_line(&self.delegation.status_line());
        self.poll_camera();
    }

    fn poll_camera(&self) {
        // 相机开关由 UI 直接翻转 state.camera_on（设置页），这里观察启停
        if !self.state.camera_on() {
            if self.camera.lock().unwrap().is_some() {
                self.stop_camera();
            }
            return;
        }
        let mode = self.mode.lock().unwrap().clone();
        let want = mode != "audio" && self.cfg.read().unwrap().video_enabled;
        if want {
            self.ensure_camera();
        }
        // 死亡检测：feeder 拉起失败（线程立即 EOF）或运行中 EOF
        let dead = {
            let cam = self.camera.lock().unwrap();
            cam.as_ref().is_some_and(|c| !c.is_alive())
        };
        if dead {
            self.note_camera_failure();
            return;
        }
        let frame = self.camera.lock().unwrap().as_ref().and_then(|c| c.latest_preview());
        if let Some(f) = &frame {
            self.remember_preview(f);
        }
        self.state
            .set_cam_preview(frame.and_then(|p| Image::from_rgb(p.width, p.height, p.data)));
    }

    // ------------------------------------------------------------------
    // UI 事件处理（main.py ui_* 回调）
    // ------------------------------------------------------------------

    fn ui_toggle_session(&self) {
        if self.session_running() {
            self.stop_session();
        } else {
            self.start_session();
        }
    }

    fn ui_toggle_mode(&self) {
        let was_running = self.session_running();
        if was_running {
            self.stop_session();
        }
        let new_mode = {
            let cur = self.mode.lock().unwrap().clone();
            if cur == "audio" {
                let omni = self.cfg.read().unwrap().fg_omni_mode.clone();
                if omni == "omni" || omni == "video" {
                    omni
                } else {
                    "omni".to_string()
                }
            } else {
                "audio".to_string()
            }
        };
        *self.mode.lock().unwrap() = new_mode.clone();
        // Rust 的 O5Session 从 cfg 快照档位，模式切换要同步写回 cfg.fg_mode
        self.cfg.write().unwrap().fg_mode = new_mode.clone();
        self.state.set_mode(&new_mode);
        self.add_sys(&format!("── 模式切换: {new_mode} ──"));
        if new_mode != "audio" {
            // 03 固件：display feeder 已占 IVPS 组 0，camera 起不来。先杀
            // display 让位（UI 投递线程断开后 2s respawn 自愈），camera 在
            // display 回来前完成 init（反向共存顺序）。
            let _ = std::process::Command::new("pkill")
                .args(["-f", "feeder_display.py"])
                .status();
        }
        if was_running {
            self.start_session();
        }
    }

    /// 拍学机「静音」= 小松果老师不出声：立刻停播，之后的口播音频丢弃（字幕照常，
    /// 麦克风不关，孩子仍可提问）。上游的静音是关麦克风，孩子会以为按钮没用。
    fn ui_toggle_mute(&self) {
        let m = !self.muted.load(Ordering::SeqCst);
        self.muted.store(m, Ordering::SeqCst);
        self.state.set_muted(m);
        self.paxueji.lock().unwrap().mute_logged = false;
        if m {
            self.audio.lock().unwrap().interrupt_playback();
        }
        log::info!(target: "o5.main", "[拍学机] 静音: {m}");
    }

    /// 更多菜单：公网调用开关——开=前台端点域名换成公网网关 o5gw.modelbest.cn。
    /// 只换域名（协议 / 路径 / Token 等其余不变），写回配置持久化；
    /// 会话在跑则重启让连接立即走新域名（与网络模式切换同口径）。
    /// 后台 / ASR 端点本就是公网域名，不走网关改写。
    fn ui_toggle_publicnet(&self) {
        let new_val = !self.cfg.read().unwrap().net_public;
        self.cfg.write().unwrap().net_public = new_val;
        self.state.set_public_net(new_val);
        let saved = crate::cfgsave::save_public_network(self.cfg_path.as_deref(), new_val);
        self.add_sys(if new_val {
            "── 公网调用: 开 → o5gw.modelbest.cn ──"
        } else {
            "── 公网调用: 关（用回配置端点）──"
        });
        log::info!(
            target: "o5.main",
            "公网调用: {}（配置{}）",
            if new_val { "开" } else { "关" },
            if saved { "已保存" } else { "未持久化（无配置文件）" }
        );
        if self.session_running() {
            self.stop_session();
            self.start_session();
        }
    }

    /// 更多菜单：ASR 转写开关——关=完全不建 ASR 连接（含断开在跑的），
    /// 开=立即重建连接（一次性实例）。会话本身不重启，麦克风照常供前台。
    /// 性能测试用：关闭后对比 CPU/网络/时延。
    fn ui_toggle_asr(&self) {
        let new_val = !self.cfg.read().unwrap().asr_enabled;
        self.cfg.write().unwrap().asr_enabled = new_val;
        self.state.set_asr_on(new_val);
        let saved = crate::cfgsave::save_asr_enabled(self.cfg_path.as_deref(), new_val);
        self.add_sys(if new_val {
            "── ASR 转写: 开 ──"
        } else {
            "── ASR 转写: 关（不建连接）──"
        });
        log::info!(
            target: "o5.main",
            "ASR 转写: {}（配置{}）",
            if new_val { "开" } else { "关" },
            if saved { "已保存" } else { "未持久化（无配置文件）" }
        );
        if new_val {
            self.ensure_asr();
        } else {
            // stop 有 1~5s join，放后台线程防卡 UI 事件循环（与 stop_session 同口径）
            let weak = self.weak.clone();
            std::thread::Builder::new()
                .name("o5-asr-stop".to_string())
                .spawn(move || {
                    if let Some(app) = weak.upgrade() {
                        if let Some(a) = app.asr.lock().unwrap().take() {
                            a.stop();
                        }
                    }
                })
                .map(|_| ())
                .unwrap_or_else(|e| log::warn!(target: "o5.main", "ASR 停止线程启动失败: {e}"));
        }
    }

    fn ui_set_volume(&self, v: f32) {
        // 二级菜单音量调节：软件增益即时生效 + 配置持久化（UI 已钳 0-100）
        let v = v.clamp(0.0, 100.0) as u8;
        if v == self.state.volume() {
            return;
        }
        self.state.set_volume(v);
        self.cfg.write().unwrap().audio_output_volume = v;
        self.audio.lock().unwrap().set_volume(v);
        let saved = crate::cfgsave::save_audio_volume(self.cfg_path.as_deref(), v);
        log::info!(
            target: "o5.main",
            "扬声器音量: {v}%（配置{}）",
            if saved { "已保存" } else { "未持久化（无配置文件）" }
        );
    }

    // ------------------------------------------------------------------
    // 运行（main.py run_ui / run_headless）
    // ------------------------------------------------------------------

    /// UI 模式：UI 专属线程阻塞跑，本线程做事件循环 + 轮询。
    /// （launcher cgroup 逃逸 + 杀 daemon 腾摄像头管线在 main 入口做——
    /// App 构造的字体加载阶段也必须在保护伞下，见 main.rs。）
    pub fn run_ui(self: &Arc<Self>) -> i32 {
        let (tx, rx) = crossbeam_channel::unbounded::<UiEvent>();
        let ui_state = self.state.clone();
        let ui_timing = Arc::clone(&self.timing);
        let ui_perf = self.perf.clone();
        let ui_cfg = self.cfg.read().unwrap().clone();
        self.wait_camera_first_frame(Duration::from_secs(10));
        self.spawn_test_control();
        let ui_thread = std::thread::Builder::new()
            .name("o5-ui".to_string())
            .spawn(move || {
                let mut ui = o5_ui::Ui::new(&ui_cfg);
                ui.run(ui_state, ui_timing, Some(ui_perf), tx);
            })
            .expect("启动 UI 线程失败");
        // 无人值守/远程排障：启动即自动建立会话（与按钮回调同路径）
        if self.cfg.read().unwrap().fg_auto_start_session {
            log::info!(target: "o5.main", "配置 autoStartSession=true，自动建立会话");
            self.start_session();
        }
        let rc = loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(UiEvent::StartStop) => self.ui_toggle_session(),
                Ok(UiEvent::Mute) => self.ui_toggle_mute(),
                Ok(UiEvent::Mode) => self.ui_toggle_mode(),
                Ok(UiEvent::Shutter) => self.ui_shutter(),
                Ok(UiEvent::VolumeChanged(v)) => self.ui_set_volume(v),
                Ok(UiEvent::PublicNet) => self.ui_toggle_publicnet(),
                Ok(UiEvent::AsrToggle) => self.ui_toggle_asr(),
                Ok(UiEvent::CloseApp) => break 0,
                // Menu/PanelToggle 是通知（状态已由 UI 翻转，app 不重复动）
                Ok(UiEvent::Menu | UiEvent::PanelToggle | UiEvent::SessionResult(_)) => {}
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                // UI 线程已退出（异常路径）：与 CloseApp 同等收尾
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break 0,
            }
            self.poll_tick();
            if self.shutdown.load(Ordering::SeqCst) {
                break *self.exit_code.lock().unwrap();
            }
        };
        // 看门狗只在真正退出应用的路径上布防（绝不挂在 stop_session 里：
        // 会话开关 / 模式切换都会调它，40s 后会把好好运行的应用杀掉）。
        crate::watchdog::arm();
        self.stop_session();
        self.stop_camera();
        // CloseApp 已让 UI 自退；join 兜底收线程
        let _ = ui_thread.join();
        // 恢复桌面（启动时杀了 launcher_daemon）：systemd 拉起 launcher。
        // 后台执行不拖退出；PC/无 systemctl 环境静默忽略。
        let _ = std::process::Command::new("systemctl")
            .args(["start", "launcher.service"])
            .spawn();
        rc
    }

    /// headless 模式：无屏常驻（systemd 部署用）。
    pub fn run_headless(self: &Arc<Self>) -> i32 {
        crate::install_signal_handlers(&self.shutdown);
        log::info!(
            target: "o5.main",
            "headless 模式启动，媒体模式: {}",
            self.mode.lock().unwrap()
        );
        self.start_session();
        let limit_min = self.cfg.read().unwrap().audio_max_session_minutes;
        while !self.shutdown.load(Ordering::SeqCst) {
            crate::poll_signal_flag(&self.shutdown);
            // 会话挂了（headless 语义：交给 systemd 重启）。idle/connecting 等
            // 是启动瞬态，只有 failed/closed 才真正算结束；卡住的连接由会话
            // 自身的看门狗兜底。
            if !self.session_running() {
                if self.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let st = *self.last_state.lock().unwrap();
                if matches!(st, SessionState::Failed | SessionState::Closed) {
                    log::warn!(
                        target: "o5.main",
                        "会话结束（{}），headless 退出交由 systemd 拉起",
                        state_str(st)
                    );
                    *self.exit_code.lock().unwrap() = if st == SessionState::Closed { 0 } else { 1 };
                    break;
                }
            }
            if limit_min > 0.0 {
                let started = *self.session_started_at.lock().unwrap();
                if let Some(t0) = started {
                    if t0.elapsed().as_secs_f64() / 60.0 >= limit_min {
                        log::info!(target: "o5.main", "会话达到时长上限 {limit_min:.0} 分钟，重启新会话");
                        self.stop_session();
                        self.start_session();
                    }
                }
            }
            let (db, rms) = {
                let a = self.audio.lock().unwrap();
                (a.level().db(), a.level().rms())
            };
            self.state.set_mic_level(db, rms);
            std::thread::sleep(Duration::from_millis(500));
        }
        crate::watchdog::arm();
        self.stop_session();
        let rc = *self.exit_code.lock().unwrap();
        log::info!(target: "o5.main", "headless 退出，exit={rc}");
        rc
    }
}

// ---------------------------------------------------------------------------
// SessionListener（session 的 reader 线程回调，必须快）
// ---------------------------------------------------------------------------

impl SessionListener for App {
    fn on_state(&self, s: SessionState) {
        *self.last_state.lock().unwrap() = s;
        self.stats.states.lock().unwrap().push(s);
        // Python：state.session = {"state": s, "detail": ""}（整体替换，detail 清空）
        self.state.set_session_state(phase_of(s));
        self.state.set_session_detail("");
        match s {
            SessionState::Ready => {
                self.reset_reconnects();
                self.start_media()
            }
            SessionState::Failed | SessionState::Closed => {
                self.stop_media();
                let reason = self.last_error.lock().unwrap().take().unwrap_or_default();
                log::warn!(target: "o5.main", "会话{}: {reason}", if s == SessionState::Closed { "结束" } else { "失败" });
                self.add_sys(paxueji::friendly_session_end(s == SessionState::Closed, &reason));
                if s == SessionState::Failed && self.headless {
                    *self.exit_code.lock().unwrap() = 1;
                    self.shutdown.store(true, Ordering::SeqCst);
                } else if s == SessionState::Failed {
                    self.schedule_reconnect();
                }
                self.check_session_minutes();
            }
            _ => {}
        }
    }

    fn on_queue_position(&self, pos: i64) {
        // Python 把 queue_update 渲染进状态行 detail（"排队中 #N"）
        self.state.set_session_detail(&format!("排队中 #{pos}"));
    }

    fn on_audio(&self, pcm16: Vec<u8>, sample_rate: u32) {
        self.stats.note_audio(sample_rate);
        if self.muted.load(Ordering::SeqCst) {
            self.note_muted_audio();
            return;
        }
        self.audio.lock().unwrap().put_speaker(pcm16, sample_rate);
    }

    fn on_spoken_text(&self, text: &str, final_: bool) {
        self.stats.note_spoken(text, final_);
        self.nudge_spoken(); // 模型开口了，不用再戳
        if !text.is_empty() {
            self.note_speech();
            self.add_model(text);
            self.state.set_think_active(false);
        }
        if final_ {
            self.on_turn_end();
        }
    }

    fn on_think_text(&self, _text: &str) {
        self.state.set_think_active(true);
    }

    fn on_interrupted(&self) {
        o5_perf::trace_ev!("interrupt", src = "server_yield");
        // 用户打断让位（口播中没有 turn_eos 就转 listen，见 listener.rs 文档）。
        // 服务器已让出话轮、旧口播不会再续——清掉积压（服务器 ~1.5x 实时
        // 下发 + aplay 管道可达数秒）实现快停；playing(0.5) 兜底：让位
        // 收据到达时积压恰好刚播完就不必重建 aplay（免一次爆音）。
        let a = self.audio.lock().unwrap();
        if a.playing(0.5) {
            a.interrupt_playback();
            log::info!(target: "o5.main", "[打断] 让位收束，清口播积压");
        }
    }

    fn on_tool_call(&self, call: &ParsedToolCall) {
        self.stats.note_tool_call(call);
        if call.name == "show_card" {
            self.handle_show_card(call);
            return;
        }
        if self.fg_tools.foreground_names().contains(call.name.as_str()) {
            if call.name == "web_search" {
                self.on_search_start();
            }
            // 本地工具（web_search / map_search）：阻塞 HTTP 放工作线程执行，
            // 绝不占 session 的 reader 线程（音频时钟红线）
            self.state
                .set_background_line(&format!("工具: {} 执行中", call.name));
            let tools = Arc::clone(&self.fg_tools);
            let weak = self.weak.clone();
            let gen_at_call = self.photo_gen();
            let (id, name, arguments) =
                (call.id.clone(), call.name.clone(), call.arguments.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("o5-tool-{name}"))
                .spawn(move || {
                    let result = tools.execute(&name, &arguments);
                    log::info!(target: "o5.main", "前台工具 {name}: {}", truncate_chars(&result, 120));
                    if let Some(app) = weak.upgrade() {
                        if app.stale_tool_result(gen_at_call, &name) {
                            // 委派纪律第 5 条：收到【忽略】静默处理
                            app.backfill(&id, "【忽略】");
                        } else {
                            if name == "web_search" {
                                app.on_search_done(&result);
                            }
                            app.backfill(&id, &result);
                        }
                        app.state.set_background_line(&app.delegation.status_line());
                    }
                });
            if let Err(e) = spawned {
                log::error!(target: "o5.main", "工具线程启动失败（{e}），该调用不回填");
            }
            return;
        }
        if let Some(reply) = self
            .delegation
            .handle_tool_call(&call.id, &call.name, &call.arguments)
        {
            self.backfill(&call.id, &reply);
        }
        self.state.set_background_line(&self.delegation.status_line());
    }

    fn on_tool_abort(&self, tool_call_id: &str) {
        self.delegation.handle_tool_abort(tool_call_id);
        self.state.set_background_line(&self.delegation.status_line());
    }

    fn on_error(&self, code: &str, message: &str) {
        log::warn!(target: "o5.main", "会话错误: {code} {message}");
        *self.last_error.lock().unwrap() = Some(message.to_string());
    }

    fn on_closed(&self, _reason: &str) {}
}

// ---------------------------------------------------------------------------
// 开口兜底线程（Python threading.Timer 链的轮询版）
// ---------------------------------------------------------------------------

/// O5 拿到 tool_response 后由 spoken 生成自行决定开不开口，反问/报错文本会被
/// 当已知信息 no_action 沉默；回填后 N 秒没 spoken.delta 就带 force_listen 戳
/// 一下让它重新决策（最多 NUDGE_MAX 次）。
fn nudge_loop(weak: Weak<App>) {
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let Some(app) = weak.upgrade() else { return };
        let fire = {
            let mut n = app.nudge.lock().unwrap();
            if !n.armed || Instant::now() < n.deadline {
                false
            } else {
                n.armed = false;
                !(n.spoken || n.count >= NUDGE_MAX)
            }
        };
        if !fire {
            continue;
        }
        if !app.session_running() {
            continue;
        }
        if app.audio.lock().unwrap().playing(0.2) {
            // 还在口播（如承接话），等下一轮再算
            app.schedule_speak_nudge();
            continue;
        }
        let nth = {
            let mut n = app.nudge.lock().unwrap();
            n.count += 1;
            n.count
        };
        log::info!(
            target: "o5.main",
            "开口兜底：回填后 {NUDGE_TIMEOUT_S:.0}s 未开口，force_listen 第 {nth} 次戳"
        );
        if let Some(s) = app.session.lock().unwrap().as_ref() {
            s.interrupt();
        }
        app.schedule_speak_nudge();
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn trim_transcript(tr: &mut TranscriptMirror) {
    while tr.entries.len() > TRANSCRIPT_CAP {
        tr.entries.pop_front();
    }
}

/// Python `session.is_running`：线程活着且未到终态。
fn session_is_running(s: &O5Session) -> bool {
    !matches!(
        s.state(),
        SessionState::Idle | SessionState::Closed | SessionState::Failed
    )
}

/// 字符 bigram Jaccard——够区分"用户真说的话"和"AI 话的回声转写"。
fn text_similar(a: &str, b: &str) -> f64 {
    if a.chars().count() < 3 || b.chars().count() < 3 {
        return 0.0;
    }
    let (ba, bb) = bigram_sets(a, b);
    let inter = ba.intersection(&bb).count();
    let union = ba.union(&bb).count().max(1);
    inter as f64 / union as f64
}

/// 字符 bigram 覆盖率（交集 / 较小集合）——"短转写 vs 长原句"的相似度：
/// 回声转写是 AI 句的前缀/片段时覆盖率=1.0，Jaccard 却只有 ~0.3（02
/// 实测拦不住的根源）。追问语义相关句对的覆盖率 <0.5，可区分。
fn text_coverage(a: &str, b: &str) -> f64 {
    if a.chars().count() < 3 || b.chars().count() < 3 {
        return 0.0;
    }
    let (ba, bb) = bigram_sets(a, b);
    let inter = ba.intersection(&bb).count();
    let mn = ba.len().min(bb.len()).max(1);
    inter as f64 / mn as f64
}

type BigramSet = std::collections::HashSet<(char, char)>;

fn bigram_sets(a: &str, b: &str) -> (BigramSet, BigramSet) {
    let sets = |s: &str| -> std::collections::HashSet<(char, char)> {
        s.chars()
            .collect::<Vec<char>>()
            .windows(2)
            .map(|w| (w[0], w[1]))
            .collect()
    };
    (sets(a), sets(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigram_similarity_matches_python_semantics() {
        // 完全相同 -> 1.0
        assert!((text_similar("你好世界", "你好世界") - 1.0).abs() < 1e-9);
        // 完全不同 -> 0
        assert_eq!(text_similar("abcdef", "uvwxyz"), 0.0);
        // 短文本 <3 -> 0
        assert_eq!(text_similar("ab", "ab"), 0.0);
        // 部分重叠：手算 bigram
        // "abcdef" -> ab bc cd de ef (5)；"abcdeg" -> ab bc cd de eg (5)
        // 交集 4，并集 6 -> 0.6667
        assert!((text_similar("abcdef", "abcdeg") - 4.0 / 6.0).abs() < 1e-9);
    }

    #[test]
    fn late_dup_threshold_on_real_utterances() {
        // 02 实测语料：原句 vs 服务端断流后残余 flush 的"重复 final"（转写
        // 略有差异），覆盖率必须 >0.72 拦下
        assert!(
            text_coverage("你现在能什么？现在能看到什么？", "你现在能看到什么？") > 0.72
        );
        assert!(text_coverage("能看到什么？", "能看到什么？") > 0.72);
        // 回声拼接句（重连补发"用户问题+AI 回答"整段转写）对用户原句：
        // 前缀完全包含 -> 覆盖率 1.0
        assert!(
            text_coverage(
                "你好，能看到前面有什么？是一张白色的办公桌。配音",
                "你好，能看到前面有什么"
            ) > 0.72
        );
        // 用户改问（新内容）：放行
        assert!(text_coverage("办公桌和什么？", "椅子是什么颜色？") < 0.5);
        // 部分重叠但语义不同的追问也放行
        assert!(text_coverage("还能看到什么？", "那边墙上有什么？") < 0.5);
        // 追问对 model 行的覆盖率同样低（echo_filtered 误杀检查）
        assert!(
            text_coverage("这是什么手势？", "这个手势我也没认出来，所以没报。") < 0.5
        );
    }

    #[test]
    fn coverage_vs_jaccard_on_echo_prefix() {
        // 回声短转写是 AI 句前缀：覆盖率 1.0、Jaccard 很低（02 实测
        // partial"能看到，前面"拦不住的根源）
        let echo = "能看到，前面";
        let model = "能看到，前面是一张白色的办公桌";
        assert!(text_coverage(echo, model) > 0.9);
        assert!(text_similar(echo, model) < 0.5);
        // AI 整句回声对自身：两者都高
        assert!(text_similar(model, model) > 0.99);
        // 短文本保护：<3 字符一律 0
        assert_eq!(text_coverage("ab", "ab"), 0.0);
    }

    #[test]
    fn phase_mapping_roundtrip() {
        for (st, ph) in [
            (SessionState::Idle, SessionPhase::Idle),
            (SessionState::Connecting, SessionPhase::Connecting),
            (SessionState::Waiting, SessionPhase::Waiting),
            (SessionState::Initializing, SessionPhase::Initializing),
            (SessionState::Ready, SessionPhase::Ready),
            (SessionState::Closing, SessionPhase::Closing),
            (SessionState::Closed, SessionPhase::Closed),
            (SessionState::Failed, SessionPhase::Failed),
        ] {
            assert_eq!(phase_of(st), ph);
        }
    }

    #[test]
    fn py_truthy_python_semantics() {
        assert!(py_truthy(&serde_json::json!("x")));
        assert!(!py_truthy(&serde_json::json!("")));
        assert!(!py_truthy(&serde_json::json!(0)));
        assert!(!py_truthy(&serde_json::json!(null)));
        assert!(py_truthy(&serde_json::json!({"a": 1})));
        assert!(!py_truthy(&serde_json::json!({})));
        assert!(py_truthy(&serde_json::json!([1])));
        assert!(!py_truthy(&serde_json::json!([])));
    }

    #[test]
    fn truncate_by_chars() {
        assert_eq!(truncate_chars("你好世界", 2), "你好");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }
}

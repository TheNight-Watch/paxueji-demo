//! O5Session 实现：线程模型、状态机、ACK 流控、看门狗与优雅关闭。
//!
//! 线程与锁（**锁序约定：ws 锁与 flow 锁绝不嵌套持有**）：
//! - reader 线程：连接（TCP + 可选 TLS + WS 握手，受 connect_timeout 约束）
//!   → 循环读帧 → 状态机推进 → listener 回调（回调必须快）。
//! - writer 线程：从 crossbeam channel 收发送任务；空闲 20s 发 WS Ping
//!   （keepalive），pong 20s 未回则强断（对齐 websockets ping_timeout=20）。
//! - 共享 `Arc<Mutex<WebSocket>>`：tungstenite sync 没有内建 split，reader
//!   的读带 25ms 超时（轮询式让出 ws 锁），writer 的发送短暂持锁——两端
//!   全双工交替，语义等价 Python 的单事件循环协程并发。
//! - 强断句柄：连接后 `try_clone` 一份 TCP socket 存进 `ctl`，任何线程
//!   `shutdown(Both)` 即刻中断对端阻塞的读写（socket 选项跨句柄共享，
//!   读超时也通过它在线程安全地调整）。

use std::collections::{HashMap, VecDeque};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use log::{debug, info, warn};
use o5_config::Config;
use o5_perf::Timing;
use o5_protocol::{
    build_input_append, build_session_close, build_session_init, build_standalone_event,
    build_tool_result, build_tool_result_slices, parse_server_frame, ParsedToolCall, ServerFrame,
    UNIT_BYTES,
};
use serde_json::Value;
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Connector, Message, WebSocket};

use crate::listener::{SessionListener, SessionState};
use crate::{COLD_INFLIGHT_LIMIT, OUTBOX_CAP, STARTED_MAP_CAP, WARM_INFLIGHT_LIMIT};

/// WS 类型别名（ws:// 或 wss:// 的 tungstenite 客户端）。
pub(super) type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

/// reader 读超时：兼作「让出 ws 锁」的轮询周期（stop/终态响应 ≤ 25ms）。
const READ_POLL: Duration = Duration::from_millis(25);
/// writer 轮询周期（发送任务的最大排队延迟）。
const WRITER_POLL: Duration = Duration::from_millis(25);
/// 空闲多久后发 keepalive Ping（对齐 Python ping_interval=20）。
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// Ping 后多久没收到 Pong 判定连接死亡（对齐 Python ping_timeout=20）。
const PING_TIMEOUT: Duration = Duration::from_secs(20);
/// stop() 后等 session.closed 的上限（Python _graceful_close 的 5s）。
const CLOSE_WAIT: Duration = Duration::from_secs(5);
/// stop() join reader 的预算（Python stop(timeout=10) 的大头）。
const READER_JOIN_BUDGET: Duration = Duration::from_millis(9_500);
/// stop() join writer 的收尾预算（合计 ≤ 10s）。
const WRITER_JOIN_BUDGET: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// 公共 API
// ---------------------------------------------------------------------------

/// O5 实时会话客户端（一次性会话，不自动重连；`Drop` 时自动 `stop()`）。
///
/// 用 [`O5Session::new`] 构造、[`O5Session::start`] 启动线程；
/// 所有方法线程安全。
pub struct O5Session {
    inner: Arc<SessionInner>,
}

impl O5Session {
    /// 从配置构造会话（不连接）。
    ///
    /// - URL：`fg_endpoint`（先过 USB 中继端口映射）加 `?mode=` 参数——
    ///   audio 档固定 audio，omni 档取 `fg_omni_mode`（video/omni，缺省 omni），
    ///   video 档固定 video（照 session.py 224-230 与 main.py 的档位解析）；
    /// - `init_tools`：前景工具 JSON（app 传入，session 只透传进 session.init）；
    /// - 音色：`voice()` 有值时 `tts_prompt_audio` 与（`fg_ref_audio_segment`
    ///   打开时的）参考音色段都取 `voice.data_b64`，`voice_sha` 用于
    ///   session.created 的 resume 对账日志。
    pub fn new(
        cfg: &Config,
        listener: Arc<dyn SessionListener>,
        timing: Option<Timing>,
        init_tools: Option<Value>,
    ) -> O5Session {
        // 前台端点完整解析（Python effective_fg_url）：公网调用开时域名先换
        // 公网网关 o5gw.modelbest.cn，再按网络模式改写（usb 中继端口）
        let endpoint = o5_perf::netmode::effective_fg_url(
            &cfg.fg_endpoint,
            cfg.net_public,
            &cfg.net_mode,
            &cfg.net_usb_relay_port_map,
        );
        // URL mode 参数：audio 档固定 audio；omni 档按配置取 omni/video
        //（非法值回落 omni，对齐 main.py 的切换解析）
        let mode_param = match cfg.fg_mode.as_str() {
            "video" => "video",
            "omni" => {
                if cfg.fg_omni_mode == "video" {
                    "video"
                } else {
                    "omni"
                }
            }
            _ => "audio",
        };
        let voice = cfg.voice().filter(|v| !v.is_empty());
        let voice_b64 = voice.map(|v| v.data_b64.clone());
        let voice_ref_b64 = if cfg.fg_ref_audio_segment {
            voice_b64.clone()
        } else {
            None
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        O5Session {
            inner: Arc::new(SessionInner {
                endpoint,
                mode_param: mode_param.to_string(),
                media_mode: cfg.fg_mode.clone(),
                with_video: cfg.fg_mode != "audio",
                api_key: cfg.fg_api_key.clone(),
                protocol_version: cfg.fg_protocol_version,
                protocol_auto: cfg.fg_protocol_auto,
                // 协商前兜底 = 配置值（auto 时为 4）；reader 连接前覆盖
                effective_version: AtomicU8::new(cfg.fg_protocol_version),
                queue_gate: cfg.fg_queue_gate,
                progress_timeout: cfg.fg_progress_timeout_s,
                ack_timeout: cfg.fg_ack_timeout_s,
                connect_timeout: cfg.fg_connect_timeout_s,
                tls_insecure: cfg.fg_tls_insecure_skip_verify,
                task_prompt: cfg.fg_task_prompt.clone(),
                shell_prompt: cfg.fg_shell_prompt.clone(),
                init_payload: Some(cfg.fg_init_payload.clone()),
                tools: init_tools,
                voice_b64,
                voice_sha: voice.map(|v| v.sha256.clone()).unwrap_or_default(),
                voice_ref_b64,
                listener,
                timing,
                flow: Mutex::new(Flow::new()),
                turn_speaking: AtomicBool::new(false),
                tx,
                rx: Mutex::new(Some(rx)),
                ctl: Mutex::new(None),
                stop_requested: AtomicBool::new(false),
                started: AtomicBool::new(false),
                terminal: AtomicBool::new(false),
                reader_id: Mutex::new(None),
                writer_id: Mutex::new(None),
                threads: Mutex::new(None),
            }),
        }
    }

    /// 启动 reader/writer 线程（幂等；重复调用为 no-op——一次性会话不支持
    /// 重启，重建请重新 `new`）。
    pub fn start(&self) {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(rx) = self
            .inner
            .rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return;
        };
        let inner = self.inner.clone();
        let reader = thread::Builder::new()
            .name("o5-session".into())
            .spawn(move || reader_main(inner))
            .expect("spawn reader 线程失败");
        let inner = self.inner.clone();
        let writer = thread::Builder::new()
            .name("o5-session-writer".into())
            .spawn(move || writer_main(inner, rx))
            .expect("spawn writer 线程失败");
        *self.inner.threads.lock().unwrap_or_else(|e| e.into_inner()) = Some((reader, writer));
    }

    /// 优雅关闭（幂等）：置停止标志 ->（v4+）发 session.close -> 5s 内等
    /// session.closed/failed -> 强断 -> join ≤10s。
    ///
    /// v3 老 dialect：服务端不认 session.close，直接强断（照 Python
    /// `_graceful_close` 的缩进语义：跳过 close 帧时连 5s 等待一起跳过）。
    pub fn stop(&self) {
        self.inner.stop_requested.store(true, Ordering::SeqCst);
        // writer 不再需要发送（session.close 由 reader 直发），允许它退出
        self.inner.terminal.store(true, Ordering::SeqCst);
        let Some((reader, writer)) = self
            .inner
            .threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return;
        };
        let me = thread::current().id();
        // 会话线程内回调（listener -> app -> stop()）不允许自 join
        if *self
            .inner
            .reader_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            != Some(me)
        {
            join_bounded(reader, READER_JOIN_BUDGET);
        }
        if *self
            .inner
            .writer_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            != Some(me)
        {
            join_bounded(writer, WRITER_JOIN_BUDGET);
        }
    }

    /// 当前状态。
    pub fn state(&self) -> SessionState {
        self.inner
            .flow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
    }

    /// 投递 1s unit（跨线程无阻塞）。ready 之外丢弃；长度异常的 unit
    /// debug 构建断言、release 构建记日志丢弃（照 Python 的防御性丢弃）。
    ///
    /// `video_jpeg_b64`：本 unit 的视频帧；None 时取 [`O5Session::set_video_frame`]
    /// 槽里最新一帧；都没有则 pump 时用黑帧占位（仅 omni/video 模式；
    /// audio 模式绝不带 video_frames）。
    pub fn offer_unit(&self, pcm16: Vec<u8>, video_jpeg_b64: Option<String>, t_recorded: Instant) {
        let mut flow = self.inner.flow.lock().unwrap_or_else(|e| e.into_inner());
        if flow.state != SessionState::Ready {
            return; // ready 门控：created 之前的音频直接丢弃
        }
        debug_assert_eq!(
            pcm16.len(),
            UNIT_BYTES,
            "unit 必须是恰好 1 秒 PCM16（{UNIT_BYTES} 字节）"
        );
        if pcm16.len() != UNIT_BYTES {
            warn!("丢弃长度异常的 unit: {} 字节", pcm16.len());
            return;
        }
        // 视频帧在投递时快照绑定（不随后来的 set_video_frame 改变）
        let video = if self.inner.with_video {
            video_jpeg_b64.or_else(|| flow.video_slot.clone())
        } else {
            None
        };
        flow.outbox.push_back(OutboxItem {
            pcm16,
            video,
            t_recorded,
        });
        while flow.outbox.len() > OUTBOX_CAP {
            flow.outbox.pop_front();
            if let Some(t) = &self.inner.timing {
                t.unit_dropped();
            }
            warn!("outbox 溢出，丢弃最旧 unit（保持实时）");
        }
        self.inner.pump(&mut flow);
    }

    /// 工具结果回填（线程安全）。v4+ 原子回填；v3 超限走 delta/done 分片
    /// （分片在内部处理）。
    pub fn send_tool_result(&self, tool_call_id: &str, content: &str) {
        self.inner.submit_tool_result(tool_call_id, content);
    }

    /// 原始回填（与 [`O5Session::send_tool_result`] 同路径；Python 只有一个
    /// 入口，契约保留两个名字，v3 分片同样在内部处理）。
    pub fn send_tool_result_raw(&self, tool_call_id: &str, content: &str) {
        self.inner.submit_tool_result(tool_call_id, content);
    }

    /// v4+ 文字输入通道（session 只透传；v5 发裸文本、v4 用包装文案，
    /// 形态由 protocol 层按生效版本定型）。
    pub fn send_standalone_event(&self, text: &str) {
        let payload =
            build_standalone_event(text, self.inner.effective_version.load(Ordering::Relaxed));
        let _ = self.inner.tx.send(SendTask::Text {
            label: "standalone_event",
            payload,
        });
    }

    /// 打断：force_listen 挂到下一个产出的 unit（打不断口播中的 unit）。
    pub fn interrupt(&self) {
        self.inner
            .flow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .force_listen_pending = true;
        info!("interrupt：下一个 unit 将带 force_listen=true");
        o5_perf::trace_ev!("force_listen");
    }

    /// 更新视频帧槽（omni/video 模式每 unit 附最新帧；无帧用黑帧占位）。
    pub fn set_video_frame(&self, jpeg_b64: String) {
        self.inner
            .flow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .video_slot = Some(jpeg_b64);
    }
}

impl Drop for O5Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// join 带超时（JoinHandle 自身无超时；超时后线程脱离管理，同 Python
/// `Thread.join(timeout)` 语义——挂起的线程随进程退出回收）。
fn join_bounded(handle: JoinHandle<()>, timeout: Duration) {
    let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);
    let _ = thread::spawn(move || {
        let _ = handle.join();
        let _ = done_tx.send(());
    });
    let _ = done_rx.recv_timeout(timeout);
}

// ---------------------------------------------------------------------------
// 内部结构
// ---------------------------------------------------------------------------

/// writer 的发送任务。
enum SendTask {
    /// 连接建立后 reader 递来的 ws（writer 在此之前只空转）。
    Attach(Arc<Mutex<Ws>>),
    /// 一个音频 unit（payload 预构建；t0/t_json 是 JSON 序列化耗时口径）。
    Unit {
        input_id: String,
        payload: String,
        t0: Instant,
        t_json: Instant,
        t_recorded: Instant,
    },
    /// 普通文本帧（session.init / tool_result / session.close 等）。
    Text {
        label: &'static str,
        payload: String,
    },
}

/// 延迟到锁外派发的 listener 回调（保证回调永不持有 flow 锁，
/// 且回调里的 session 重入不会死锁）。
enum Cb {
    State(SessionState),
    QueuePosition(i64),
    Audio(Vec<u8>, u32),
    Spoken(String, bool),
    /// 用户打断让位（口播中没有 turn_eos 就转 listen，cpm-harness 口径）：
    /// 服务器让出话轮——调用方应清播放积压实现快停。
    Interrupted,
    Think(String),
    ToolCall(ParsedToolCall),
    ToolAbort(String),
    Error(String, String),
    Closed(String),
}

/// outbox 中待发的一个 unit。
struct OutboxItem {
    pcm16: Vec<u8>,
    video: Option<String>,
    t_recorded: Instant,
}

/// 流控状态（仅持 flow 锁访问；对应 Python 仅 asyncio 线程访问的字段）。
struct Flow {
    state: SessionState,
    /// 仅用于日志（契约回调不带 detail）。
    detail: String,
    fail_reason: Option<String>,
    close_reason: Option<String>,

    seq: u64,
    outbox: VecDeque<OutboxItem>,
    /// 有序映射 input_id -> t_sent（最旧在队头，看门狗对账用）。
    unacked: VecDeque<(String, Instant)>,
    /// v4 反查表：unit_index -> input_id（cap STARTED_MAP_CAP，丢最旧）。
    started_map: VecDeque<(i64, String)>,
    warmed: bool,
    force_listen_pending: bool,
    init_sent: bool,
    ready: bool,
    progress_deadline: Instant,
    /// tool_call_id -> 累积参数文本（begin/delta 拼装）。
    tool_buf: HashMap<String, String>,
    last_summary: Instant,
    created_logged: bool,
    close_started: bool,
    /// 最近一次收到 Pong 的时刻（keepalive 判活）。
    last_pong: Option<Instant>,
    /// 视频帧槽（set_video_frame / offer_unit 的 Some 参数）。
    video_slot: Option<String>,
}

impl Flow {
    fn new() -> Self {
        Flow {
            state: SessionState::Idle,
            detail: String::new(),
            fail_reason: None,
            close_reason: None,
            seq: 0,
            outbox: VecDeque::new(),
            unacked: VecDeque::new(),
            started_map: VecDeque::new(),
            warmed: false,
            force_listen_pending: false,
            init_sent: false,
            ready: false,
            progress_deadline: far_future(),
            tool_buf: HashMap::new(),
            last_summary: Instant::now(),
            created_logged: false,
            close_started: false,
            last_pong: None,
            video_slot: None,
        }
    }
}

/// Python `time.monotonic() + 1e9` 的「无限期」哨兵。
fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(60 * 60 * 24 * 365)
}

struct SessionInner {
    // —— 配置快照（构造时定死）——
    endpoint: String,
    mode_param: String,
    media_mode: String,
    with_video: bool,
    api_key: String,
    protocol_version: u8,
    /// `protocolVersion: "auto"`：连接前 GET `/v1/info` 协商（见 `deploy_probe`）。
    protocol_auto: bool,
    /// 生效协议版本（auto 协商结果；非 auto 恒等于 `protocol_version`）。
    /// reader 线程连接前写入，`send_init` / URL 构造读取。
    effective_version: AtomicU8,
    queue_gate: bool,
    progress_timeout: f64,
    ack_timeout: f64,
    connect_timeout: f64,
    tls_insecure: bool,
    task_prompt: String,
    shell_prompt: String,
    init_payload: Option<Value>,
    tools: Option<Value>,
    voice_b64: Option<String>,
    voice_sha: String,
    voice_ref_b64: Option<String>,

    listener: Arc<dyn SessionListener>,
    timing: Option<Timing>,

    flow: Mutex<Flow>,
    /// 回合口播中（spoken.delta 置 true；**只有** turn_eos / 让位 listen 复位，
    /// slot_eos 等口播中的 unit 收据不碰它——见 SpokenEnd 处理）。
    turn_speaking: AtomicBool,
    tx: Sender<SendTask>,
    rx: Mutex<Option<Receiver<SendTask>>>,
    /// 强断/调超时用的 TCP 句柄克隆（socket 选项跨句柄共享）。
    ctl: Mutex<Option<TcpStream>>,
    stop_requested: AtomicBool,
    started: AtomicBool,
    /// 终态（closed/failed 或 stop）：writer 停止发送并退出。
    terminal: AtomicBool,
    /// reader/writer 自身线程 id（stop 防自 join）。
    reader_id: Mutex<Option<ThreadId>>,
    writer_id: Mutex<Option<ThreadId>>,
    threads: Mutex<Option<(JoinHandle<()>, JoinHandle<()>)>>,
}

impl SessionInner {
    // —— 发送侧 -----------------------------------------------------------

    /// v3 超限回填走 delta/done 分片；v4+ 原子；状态门控照 Python。
    fn submit_tool_result(&self, tool_call_id: &str, content: &str) {
        {
            let flow = self.flow.lock().unwrap_or_else(|e| e.into_inner());
            if !matches!(flow.state, SessionState::Ready | SessionState::Initializing) {
                warn!("会话不在进行中，丢弃 tool_result（{tool_call_id}）");
                return;
            }
        }
        if self.protocol_version == 3 && content.len() > o5_protocol::TOOL_RESULT_MAX_BYTES {
            for payload in build_tool_result_slices(tool_call_id, content) {
                let _ = self.tx.send(SendTask::Text {
                    label: "tool_result.delta",
                    payload,
                });
            }
            return;
        }
        let payload = build_tool_result(tool_call_id, content, self.protocol_version);
        let _ = self.tx.send(SendTask::Text {
            label: "tool_result",
            payload,
        });
    }

    /// outbox -> 在途（限制窗口内才发）。**调用方必须持 flow 锁**；
    /// 只做非阻塞 channel 投递，绝不碰 ws 锁。
    fn pump(&self, flow: &mut Flow) {
        let limit = if flow.warmed {
            WARM_INFLIGHT_LIMIT
        } else {
            COLD_INFLIGHT_LIMIT
        };
        while !flow.outbox.is_empty() && flow.unacked.len() < limit {
            let item = flow.outbox.pop_front().expect("outbox 非空");
            flow.seq += 1;
            let input_id = format!("unit_{:03}", flow.seq);
            let force = flow.force_listen_pending;
            flow.force_listen_pending = false;
            let t0 = Instant::now();
            // 视频槽不许空：最后一帧，否则黑帧占位（audio 模式 None 绝不带）
            let video = if self.with_video {
                Some(item.video.as_deref().unwrap_or(""))
            } else {
                None
            };
            let payload =
                build_input_append(&input_id, &item.pcm16, video, force, self.protocol_version);
            let t_json = Instant::now();
            // 预占在途名额（committed 不可能在 send 完成前到达，
            // writer 发送完成后回填真实 t_sent）
            flow.unacked.push_back((input_id.clone(), t_json));
            let _ = self.tx.send(SendTask::Unit {
                input_id,
                payload,
                t0,
                t_json,
                t_recorded: item.t_recorded,
            });
        }
    }

    /// 发 session.init（构造 + 置 initializing；**调用方必须持 flow 锁**）。
    fn send_init(&self, flow: &mut Flow, cbs: &mut Vec<Cb>) {
        if flow.init_sent {
            return;
        }
        flow.init_sent = true;
        let effective = self.effective_version.load(Ordering::Relaxed);
        // v5 媒体契约声明（audio/omni，video 并入 omni）；v3/v4 不带
        let input_mode = (effective == 5)
            .then(|| if self.media_mode == "audio" { "audio" } else { "omni" });
        let payload = build_session_init(
            effective,
            &self.task_prompt,
            &self.shell_prompt,
            self.voice_ref_b64.as_deref(),
            self.voice_b64.as_deref(),
            self.tools.as_ref(),
            self.init_payload.as_ref(),
            input_mode,
        );
        set_state(
            flow,
            SessionState::Initializing,
            "等待 session.created",
            cbs,
        );
        let _ = self.tx.send(SendTask::Text {
            label: "session.init",
            payload,
        });
    }

    // —— 状态/失败 --------------------------------------------------------

    /// 失败（一次性语义）。任意线程可调；回调在调用线程派发。
    fn fail(&self, reason: String) {
        let mut cbs = Vec::new();
        {
            let mut flow = self.flow.lock().unwrap_or_else(|e| e.into_inner());
            self.fail_locked(&mut flow, "", &reason, &reason, &mut cbs);
        }
        dispatch(&self.listener, cbs);
        self.force_shutdown();
    }

    /// 失败（**调用方必须持 flow 锁**）。`code`/`message` 进 on_error，
    /// `reason` 进日志与 on_state(Failed) 的口径。
    fn fail_locked(
        &self,
        flow: &mut Flow,
        code: &str,
        message: &str,
        reason: &str,
        cbs: &mut Vec<Cb>,
    ) {
        if matches!(flow.state, SessionState::Failed | SessionState::Closed) {
            return;
        }
        log::error!("会话失败: {reason}");
        flow.fail_reason = Some(reason.to_string());
        cbs.push(Cb::Error(code.to_string(), message.to_string()));
        set_state(flow, SessionState::Failed, reason, cbs);
    }

    /// 立刻中断底层连接（任何线程；未连接时 no-op）。
    fn force_shutdown(&self) {
        if let Some(sock) = self.ctl.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = sock.shutdown(Shutdown::Both);
        }
    }
}

/// 状态迁移（记录日志 + on_state 回调；Closed 时附带 on_closed）。
fn set_state(flow: &mut Flow, new: SessionState, detail: &str, cbs: &mut Vec<Cb>) {
    info!("state -> {new:?} ({detail})");
    flow.state = new;
    flow.detail = detail.to_string();
    cbs.push(Cb::State(new));
    if new == SessionState::Closed {
        cbs.push(Cb::Closed(
            flow.close_reason
                .clone()
                .unwrap_or_else(|| "连接结束".to_string()),
        ));
    }
}

/// 回调派发（等价 Python `_safe`：listener 异常吞掉记日志，不杀线程）。
fn dispatch(listener: &Arc<dyn SessionListener>, cbs: Vec<Cb>) {
    for cb in cbs {
        let l = listener.clone();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || match cb {
            Cb::State(s) => l.on_state(s),
            Cb::QueuePosition(p) => l.on_queue_position(p),
            Cb::Audio(pcm16, rate) => l.on_audio(pcm16, rate),
            Cb::Spoken(text, final_) => l.on_spoken_text(&text, final_),
            Cb::Interrupted => l.on_interrupted(),
            Cb::Think(text) => l.on_think_text(&text),
            Cb::ToolCall(call) => l.on_tool_call(&call),
            Cb::ToolAbort(id) => l.on_tool_abort(&id),
            Cb::Error(code, message) => l.on_error(&code, &message),
            Cb::Closed(reason) => l.on_closed(&reason),
        }));
        if r.is_err() {
            log::error!("listener 回调异常（panic 已吞）");
        }
    }
}

// ---------------------------------------------------------------------------
// reader 线程：连接 + 读循环 + 状态机 + 看门狗 + 优雅关闭驱动
// ---------------------------------------------------------------------------

fn reader_main(inner: Arc<SessionInner>) {
    // 先登记自身 id（任何 listener -> app -> stop() 的重入都发生在之后）
    *inner.reader_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread::current().id());

    // 协议版本 auto：连接前 GET <端点>/v1/info 协商（阻塞 ≤5s，此刻
    // recv/watchdog 还没起，不影响任何节拍；失败回退配置兜底值）。
    if inner.protocol_auto {
        let info = crate::deploy_probe::fetch_info(
            &inner.endpoint,
            &inner.api_key,
            inner.tls_insecure,
        );
        let (v, note) = crate::deploy_probe::negotiate_protocol_version(info.as_ref());
        inner.effective_version.store(v, Ordering::Relaxed);
        info!("协议协商: v{v}（{note}）");
    }
    // v5 只认 audio/omni（媒体契约由 init 的 input_mode 声明，video 并入 omni）
    let effective = inner.effective_version.load(Ordering::Relaxed);
    let mode_param = if effective == 5 {
        if inner.media_mode == "audio" {
            "audio"
        } else {
            "omni"
        }
    } else {
        inner.mode_param.as_str()
    };

    info!(
        "连接 {}（mode={}, protocol={}{}）",
        build_url(&inner.endpoint, mode_param),
        inner.media_mode,
        effective,
        if inner.protocol_auto { "（auto 协商）" } else { "" }
    );
    if let Some(t) = &inner.timing {
        t.connect_started();
    }
    {
        let mut cbs = Vec::new();
        let mut flow = inner.flow.lock().unwrap_or_else(|e| e.into_inner());
        set_state(&mut flow, SessionState::Connecting, "连接中", &mut cbs);
        drop(flow);
        dispatch(&inner.listener, cbs);
    }

    // —— 连接阶段（TCP -> 可选 TLS -> WS 握手，超时 = connect_timeout）——
    let ws = match connect(&inner, mode_param) {
        Ok(ws) => ws,
        Err(e) => {
            inner.fail(format!("连接失败: {e}"));
            reader_tail(&inner, None);
            return;
        }
    };
    let ws = Arc::new(Mutex::new(ws));
    let _ = inner.tx.send(SendTask::Attach(ws.clone()));

    {
        let mut cbs = Vec::new();
        let mut flow = inner.flow.lock().unwrap_or_else(|e| e.into_inner());
        flow.progress_deadline = Instant::now() + secs(inner.progress_timeout);
        info!("WebSocket 已连接，等待排队/会话");
        if !inner.queue_gate {
            inner.send_init(&mut flow, &mut cbs);
        }
        drop(flow);
        dispatch(&inner.listener, cbs);
    }

    let mut close_deadline: Option<Instant> = None;
    loop {
        // 1) 终态退出；2) stop 请求 -> 发起优雅关闭
        let stop_begin = {
            let flow = inner.flow.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(flow.state, SessionState::Closed | SessionState::Failed) {
                break;
            }
            inner.stop_requested.load(Ordering::Relaxed) && !flow.close_started
        };
        if stop_begin {
            let (deadline, cbs) = begin_close(&inner, &ws);
            close_deadline = Some(deadline);
            dispatch(&inner.listener, cbs);
        }
        // 3) 优雅关闭等待超时 -> 强断（读循环随后的错误即静默退出）
        if let Some(deadline) = close_deadline {
            if Instant::now() > deadline {
                inner.force_shutdown();
            }
        }
        // 4) 看门狗（Python 1s tick；25ms 轮询只是检查得更勤）
        watchdog_tick(&inner);

        // 5) 读一帧（25ms 超时轮询，让 writer 有机会持 ws 锁发送）
        let msg = {
            let mut g = ws.lock().unwrap_or_else(|e| e.into_inner());
            g.read()
        };
        match msg {
            Ok(Message::Text(text)) => {
                fault::delay_rx();
                let s = text.as_str();
                if let Some(t) = &inner.timing {
                    t.rx_bytes(s.len() as u64);
                }
                match parse_server_frame(s) {
                    None => warn!("无法解析的帧: {:.120}", s),
                    Some(frame) => {
                        let mut cbs = Vec::new();
                        {
                            let mut flow = inner.flow.lock().unwrap_or_else(|e| e.into_inner());
                            flow.progress_deadline = Instant::now() + secs(inner.progress_timeout);
                            handle_frame(&inner, &mut flow, frame, &mut cbs);
                        }
                        dispatch(&inner.listener, cbs);
                    }
                }
            }
            Ok(Message::Ping(_)) => {} // tungstenite 已排队自动回 pong
            Ok(Message::Pong(_)) => {
                inner
                    .flow
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_pong = Some(Instant::now());
            }
            Ok(Message::Binary(_)) => {} // JSON 文本协议，忽略二进制帧
            Ok(Message::Close(_)) => {}  // 回包已由 tungstenite 排队；下一轮 read 收尾
            Ok(Message::Frame(_)) => {}
            Err(e) if is_poll_timeout(&e) => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => {
                let quiet = {
                    let flow = inner.flow.lock().unwrap_or_else(|e| e.into_inner());
                    matches!(
                        flow.state,
                        SessionState::Closed | SessionState::Failed | SessionState::Closing
                    )
                };
                if !quiet {
                    inner.fail(format!("接收中断: {e}"));
                }
                break;
            }
        }
    }

    reader_tail(&inner, Some(ws));
}

/// reader 收尾：ws.close 尽力（对齐 Python `_force_close_ws` 的 `ws.close()`）
/// + 强断兜底 + 状态兜底 + 结束日志。
fn reader_tail(inner: &Arc<SessionInner>, ws: Option<Arc<Mutex<Ws>>>) {
    if let Some(ws) = ws {
        let mut g = ws.lock().unwrap_or_else(|e| e.into_inner());
        let _ = g.close(None);
    }
    inner.force_shutdown();
    let mut cbs = Vec::new();
    {
        let mut flow = inner.flow.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(flow.state, SessionState::Closed | SessionState::Failed) {
            if flow.close_reason.is_none() {
                flow.close_reason = Some("连接结束".to_string());
            }
            set_state(&mut flow, SessionState::Closed, "连接结束", &mut cbs);
        }
        info!(
            "session 结束: state={:?} detail={}",
            flow.state, flow.detail
        );
    }
    inner.terminal.store(true, Ordering::SeqCst);
    dispatch(&inner.listener, cbs);
}

/// stop 请求的优雅关闭（reader 线程内联驱动）。
///
/// 返回（强断 deadline, 待派发回调）。v4+：发 session.close 后至多 5s 等
/// session.closed；v3：服务端不认 session.close，立即强断。
fn begin_close(inner: &Arc<SessionInner>, ws: &Arc<Mutex<Ws>>) -> (Instant, Vec<Cb>) {
    let mut cbs = Vec::new();
    {
        let mut flow = inner.flow.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(flow.state, SessionState::Closed | SessionState::Failed) {
            return (Instant::now(), cbs);
        }
        flow.close_started = true;
        set_state(&mut flow, SessionState::Closing, "正在关闭", &mut cbs);
    }
    if inner.protocol_version != 3 {
        let payload = build_session_close();
        let n = payload.len();
        let sent = {
            let mut g = ws.lock().unwrap_or_else(|e| e.into_inner());
            g.send(Message::Text(payload.into()))
        };
        match sent {
            Ok(()) => {
                if let Some(t) = &inner.timing {
                    t.tx_bytes(n as u64);
                }
            }
            Err(e) => warn!("发送 session.close 失败: {e}"),
        }
        (Instant::now() + CLOSE_WAIT, cbs)
    } else {
        inner.force_shutdown();
        (Instant::now(), cbs)
    }
}

/// 看门狗 tick（reader 线程，每次循环调；语义与 Python 1s tick 等价）。
fn watchdog_tick(inner: &Arc<SessionInner>) {
    let now = Instant::now();
    let mut fail_reason: Option<String> = None;
    let mut summary = false;
    {
        let mut flow = inner.flow.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(
            flow.state,
            SessionState::Connecting | SessionState::Waiting | SessionState::Initializing
        ) && now > flow.progress_deadline
        {
            fail_reason = Some(format!(
                "会话建立超时（{:.0}s 无进展）",
                inner.progress_timeout
            ));
        }
        if fail_reason.is_none() {
            if let Some((_, oldest)) = flow.unacked.front() {
                if now.duration_since(*oldest) > secs(inner.ack_timeout) {
                    fail_reason = Some(format!("音频 unit 确认超时（>{:.0}s）", inner.ack_timeout));
                }
            }
        }
        if now.duration_since(flow.last_summary) > Duration::from_secs(10) {
            flow.last_summary = now;
            summary = true;
        }
    }
    if let Some(reason) = fail_reason {
        inner.fail(reason);
    }
    if summary {
        if let Some(t) = &inner.timing {
            info!("[timing] {}", t.summary_line());
        }
    }
}

// ---------------------------------------------------------------------------
// 帧处理（状态机推进；**调用方必须持 flow 锁**，回调收集进 cbs 锁外派发）
// ---------------------------------------------------------------------------

fn handle_frame(inner: &Arc<SessionInner>, flow: &mut Flow, frame: ServerFrame, cbs: &mut Vec<Cb>) {
    match frame {
        ServerFrame::Queued => {
            set_state(flow, SessionState::Waiting, "排队中", cbs);
        }
        ServerFrame::QueueUpdate { position } => {
            set_state(
                flow,
                SessionState::Waiting,
                &format!("排队中 #{position}"),
                cbs,
            );
            cbs.push(Cb::QueuePosition(position));
        }
        ServerFrame::QueueDone => {
            if !flow.init_sent {
                info!("排队完成，发送 session.init");
                inner.send_init(flow, cbs);
            }
        }
        ServerFrame::Created {
            session_id,
            resume_sha256,
        } => {
            if !flow.init_sent {
                // 直连 worker：没有队列帧时 created 先到，补发 init
                info!("session.created 先于 queue_done 到达，补发 init");
                inner.send_init(flow, cbs);
            }
            if !flow.created_logged {
                flow.created_logged = true;
                if !inner.voice_sha.is_empty() {
                    if let Some(theirs) = &resume_sha256 {
                        let verdict = if theirs == &inner.voice_sha {
                            "一致"
                        } else {
                            "不一致（服务端替换了参考音色？）"
                        };
                        info!(
                            "音色 sha256 对账: {}（本机 {}... 服务端 {}...）",
                            verdict,
                            head12(&inner.voice_sha),
                            head12(theirs)
                        );
                    }
                }
                info!("session.created: id={session_id}");
            }
            if !flow.ready {
                flow.ready = true;
                set_state(flow, SessionState::Ready, "会话就绪", cbs);
                if let Some(t) = &inner.timing {
                    t.session_ready();
                }
            }
        }
        ServerFrame::UnitStarted {
            input_id,
            unit_index,
        } => {
            if let Some(id) = input_id.filter(|s| !s.is_empty()) {
                map_insert(&mut flow.started_map, unit_index, id);
            }
            // standalone 回执 / tool_events 台账只做日志（协议层已归一丢弃）
        }
        ServerFrame::UnitCommitted {
            input_id,
            unit_index,
        } => {
            // v4 committed 不带 input_id，用 started 的映射补
            let input_id = if let Some(id) = input_id.filter(|s| !s.is_empty()) {
                Some(id)
            } else {
                map_remove(&mut flow.started_map, unit_index)
            };
            match input_id
                .filter(|id| flow.unacked.iter().any(|(i, _)| i == id))
                .map(|id| {
                    let idx = flow
                        .unacked
                        .iter()
                        .position(|(i, _)| *i == id)
                        .expect("刚确认存在");
                    (id, flow.unacked.remove(idx).expect("刚确认存在").1)
                }) {
                Some((id, t_sent)) => {
                    if !flow.warmed {
                        flow.warmed = true;
                        info!(
                            "首个 committed ACK（冷启动结束，在途限制 {} -> {}）",
                            COLD_INFLIGHT_LIMIT, WARM_INFLIGHT_LIMIT
                        );
                    }
                    o5_perf::trace_ev!("ack", id = id, rtt_ms = t_sent.elapsed().as_millis());
                    if let Some(t) = &inner.timing {
                        t.send_to_ack(&id, t_sent);
                    }
                    inner.pump(flow);
                }
                None => debug!("committed 无法匹配在途 unit: unit_index={unit_index}"),
            }
        }
        ServerFrame::SpokenDelta { text, audio } => {
            // 口播中（回合活跃标志，见 SpokenEnd 的收束判定）
            inner.turn_speaking.store(true, Ordering::Relaxed);
            if !text.is_empty() {
                cbs.push(Cb::Spoken(text, false));
            }
            if let Some((pcm16, rate)) = audio {
                o5_perf::trace_ev!("dl", bytes = pcm16.len(), rate = rate);
                // 按回合口径：Timing 内部只在回合首块记 response 指标
                if let Some(t) = &inner.timing {
                    t.audio_received();
                }
                cbs.push(Cb::Audio(pcm16, rate));
            }
        }
        ServerFrame::SpokenEnd { reason } => {
            o5_perf::trace_ev!(
                "spoken_end",
                reason = reason,
                speaking = inner.turn_speaking.load(Ordering::Relaxed)
            );
            if reason == "turn_eos" {
                // 回合结束（契约用 on_spoken_text(final=true) 表达 on_turn_end）。
                // 正常说完：speaking 复位，**不清**播放缓冲（积压自然排空）。
                cbs.push(Cb::Spoken(String::new(), true));
                inner.turn_speaking.store(false, Ordering::Relaxed);
                if let Some(t) = &inner.timing {
                    t.turn_ended();
                }
            } else if reason == "listen" && inner.turn_speaking.load(Ordering::Relaxed) {
                // 让位：口播回合没有 turn_eos 就直接转 listen = 被用户插话
                // 打断后让出话轮（cpm-harness 口径：listen 语义是"本 unit 在听、
                // 不产生口播"）。此刻清播放积压实现快停——服务器 ~1.5x 实时
                // 下发，队列+aplay 管道压着数秒旧音频，不清的话打断后还要
                // 等积压排完才静音。
                inner.turn_speaking.store(false, Ordering::Relaxed);
                cbs.push(Cb::Interrupted);
            }
            // 其余忽略：listen 且空闲（speaking=false）= 每 unit 的常规收据
            // （空闲期每秒一条，曾误当打断触发"说一半就没了"）；slot_eos/
            // tts_pad = unit 内收束，口播延续（v5 透明池口播中每 unit 一条，
            // 曾误复位 speaking 导致每秒误清缓冲、"重复一句话"+aplay 重建爆音）。
        }
        ServerFrame::ThinkBegin => {}
        ServerFrame::ThinkDelta { text } => {
            if !text.is_empty() {
                cbs.push(Cb::Think(text));
            }
        }
        // think.end 的文本在协议层归一为无字段变体（契约折衷），这里无内容可交付
        ServerFrame::ThinkEnd => {}
        ServerFrame::ToolCallBegin { id } => {
            if !id.is_empty() {
                flow.tool_buf.insert(id, String::new());
            }
        }
        ServerFrame::ToolCallDelta { id, arguments } => {
            if let Some(id) = id.filter(|s| !s.is_empty()) {
                flow.tool_buf.entry(id).or_default().push_str(&arguments);
            }
        }
        ServerFrame::ToolCallDone { call } => {
            match call {
                None => {
                    // 错误帧：id 已在协议层丢失（无法按 id 清缓冲），忽略不执行
                    info!("tool_call 错误帧，忽略不执行");
                }
                Some(mut call) => {
                    let acc = flow.tool_buf.remove(&call.id).unwrap_or_default();
                    if call.arguments.is_empty() {
                        call.arguments = if acc.is_empty() {
                            "{}".to_string()
                        } else {
                            acc
                        };
                    }
                    info!(
                        "tool_call: {} {}",
                        call.name,
                        &call.arguments[..call.arguments.len().min(120)]
                    );
                    cbs.push(Cb::ToolCall(call));
                }
            }
        }
        ServerFrame::ToolCallAbort { tool_call_id } => {
            flow.tool_buf.remove(&tool_call_id);
            info!("tool_call 放弃: {tool_call_id}");
            cbs.push(Cb::ToolAbort(tool_call_id));
        }
        ServerFrame::NonSpokenEnd => {}
        ServerFrame::Closed { reason, diagnostic } => {
            if inner.stop_requested.load(Ordering::Relaxed) || reason == "client_closed" {
                flow.close_reason = Some(reason);
                set_state(flow, SessionState::Closed, "会话已关闭", cbs);
            } else {
                let full = format!(
                    "会话被服务端关闭: {reason} {}",
                    diagnostic.unwrap_or_default()
                )
                .trim()
                .to_string();
                inner.fail_locked(flow, "", &full, &full, cbs);
            }
        }
        ServerFrame::Error { code, message } => {
            let reason = format!("服务端错误: {code} {message}").trim().to_string();
            inner.fail_locked(flow, &code, &message, &reason, cbs);
        }
        ServerFrame::Unknown { type_ } => {
            // 未知帧只记录（语义 v3 真机对未知上行敏感，下行未知帧忽略即可）
            debug!("未处理的帧: {type_}");
        }
    }
}

/// 有序映射插入（超容量丢最旧，对应 OrderedDict popitem(last=False)）。
fn map_insert(map: &mut VecDeque<(i64, String)>, key: i64, value: String) {
    map.retain(|(k, _)| *k != key);
    map.push_back((key, value));
    while map.len() > STARTED_MAP_CAP {
        map.pop_front();
    }
}

/// 有序映射按键取走。
fn map_remove(map: &mut VecDeque<(i64, String)>, key: i64) -> Option<String> {
    let idx = map.iter().position(|(k, _)| *k == key)?;
    map.remove(idx).map(|(_, v)| v)
}

/// sha256 摘要的前 12 个字符（不足则全取）。
fn head12(s: &str) -> &str {
    &s[..s.len().min(12)]
}

// ---------------------------------------------------------------------------
// writer 线程：发送任务 + keepalive
// ---------------------------------------------------------------------------

fn writer_main(inner: Arc<SessionInner>, rx: Receiver<SendTask>) {
    *inner.writer_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread::current().id());
    let mut ws: Option<Arc<Mutex<Ws>>> = None;
    let mut last_send = Instant::now();
    let mut ping_sent: Option<Instant> = None;
    loop {
        if inner.terminal.load(Ordering::Relaxed) {
            break;
        }
        let task = match rx.recv_timeout(WRITER_POLL) {
            Ok(task) => Some(task),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        match task {
            Some(SendTask::Attach(w)) => {
                ws = Some(w);
                last_send = Instant::now();
            }
            Some(SendTask::Unit {
                input_id,
                payload,
                t0,
                t_json,
                t_recorded,
            }) => {
                if let Some(ws) = &ws {
                    fault::delay_tx_unit();
                    let n = payload.len();
                    let sent = {
                        let mut g = ws.lock().unwrap_or_else(|e| e.into_inner());
                        g.send(Message::Text(payload.into()))
                    };
                    match sent {
                        Ok(()) => {
                            let now = Instant::now();
                            {
                                // 回填真实发送完成时刻（committed 不可能更早到）
                                let mut flow = inner.flow.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(slot) =
                                    flow.unacked.iter_mut().find(|(id, _)| *id == input_id)
                                {
                                    slot.1 = now;
                                }
                            }
                            o5_perf::trace_ev!(
                                "ul_sent",
                                id = input_id,
                                bytes = n,
                                send_ms = now.duration_since(t0).as_millis()
                            );
                            if let Some(t) = &inner.timing {
                                t.unit_send_done(t0, t_json);
                                t.unit_sent(t_recorded);
                                t.tx_bytes(n as u64);
                            }
                        }
                        Err(e) => inner.fail(format!("发送 unit 失败: {e}")),
                    }
                    last_send = Instant::now();
                }
            }
            Some(SendTask::Text { label, payload }) => {
                if let Some(ws) = &ws {
                    let n = payload.len();
                    let sent = {
                        let mut g = ws.lock().unwrap_or_else(|e| e.into_inner());
                        g.send(Message::Text(payload.into()))
                    };
                    match sent {
                        Ok(()) => {
                            if let Some(t) = &inner.timing {
                                t.tx_bytes(n as u64);
                            }
                        }
                        Err(e) => inner.fail(format!("发送 {label} 失败: {e}")),
                    }
                    last_send = Instant::now();
                }
            }
            None => {}
        }

        // keepalive：空闲 20s 发 Ping；pong 20s 未回判死强断
        //（对齐 Python websockets ping_interval=20 / ping_timeout=20；
        // 强断后 reader 的读循环会以「接收中断」失败，等价 websockets 的
        // keepalive 超时关闭语义）
        let now = Instant::now();
        if let Some(ws) = &ws {
            if now.duration_since(last_send) >= PING_INTERVAL {
                last_send = now;
                let sent = {
                    let mut g = ws.lock().unwrap_or_else(|e| e.into_inner());
                    g.send(Message::Ping(b"o5-keepalive".to_vec().into()))
                };
                match sent {
                    Ok(()) => ping_sent = Some(now),
                    Err(e) => inner.fail(format!("发送 ping 失败: {e}")),
                }
            }
            if let Some(t) = ping_sent {
                let ponged = inner
                    .flow
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_pong
                    .is_some_and(|p| p > t);
                if ponged {
                    ping_sent = None;
                } else if now.duration_since(t) > PING_TIMEOUT {
                    ping_sent = None;
                    inner.force_shutdown();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 连接与 URL
// ---------------------------------------------------------------------------

/// 连接 URL：endpoint + `?mode=`（替换已有 mode 参数，其余原样透传；
/// 保留 fragment）。
fn build_url(endpoint: &str, mode_param: &str) -> String {
    let (rest, fragment) = match endpoint.split_once('#') {
        Some((r, f)) => (r, Some(f)),
        None => (endpoint, None),
    };
    let (base, query) = match rest.split_once('?') {
        Some((b, q)) => (b, q),
        None => (rest, ""),
    };
    let mut url = String::with_capacity(endpoint.len() + mode_param.len() + 8);
    url.push_str(base);
    url.push('?');
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("mode="))
        .collect();
    for pair in &kept {
        url.push_str(pair);
        url.push('&');
    }
    url.push_str("mode=");
    url.push_str(mode_param);
    if let Some(f) = fragment {
        url.push('#');
        url.push_str(f);
    }
    url
}

/// TCP 连接 + TLS + WS 握手。失败返回给用户的错误文案。
/// `mode_param` 由 reader 按生效协议版本算好（v5 只认 audio/omni）。
fn connect(inner: &Arc<SessionInner>, mode_param: &str) -> Result<Ws, String> {
    let url = build_url(&inner.endpoint, mode_param);
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("{e}"))?;
    // 无 path 的 URL（如 "ws://host:port?mode=audio"）：http::Uri 保留空 path，
    // tungstenite 会写出非法请求行 "GET ?mode=audio"。Python websockets 会
    // 把空 path 规范成 "/"，这里对齐（"GET /?mode=audio"）。
    {
        let uri = request.uri().clone();
        if let Some(query) = uri
            .path_and_query()
            .and_then(|pq| pq.as_str().strip_prefix('?'))
        {
            let fixed = tungstenite::http::Uri::builder()
                .scheme(uri.scheme_str().unwrap_or("ws"))
                .authority(uri.authority().map(|a| a.as_str()).unwrap_or("localhost"))
                .path_and_query(format!("/?{query}"))
                .build()
                .map_err(|e| format!("URL 非法: {e}"))?;
            *request.uri_mut() = fixed;
        }
    }
    if !inner.api_key.is_empty() {
        let v = tungstenite::http::HeaderValue::from_str(&format!("Bearer {}", inner.api_key))
            .map_err(|e| format!("Authorization 头非法: {e}"))?;
        request.headers_mut().insert("Authorization", v);
    }

    let uri = request.uri().clone();
    let is_tls = uri
        .scheme_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("wss"));
    let host = uri.host().ok_or("URL 缺少主机名")?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let port = uri.port_u16().unwrap_or(if is_tls { 443 } else { 80 });

    let timeout = secs(inner.connect_timeout);
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("域名解析失败: {e}"))?;
    let mut last_err = format!("{host}:{port} 无可用地址");
    let mut stream = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = format!("{e}"),
        }
    }
    let stream = stream.ok_or(last_err)?;
    // Windows 上 connect_timeout 会把 socket 留在非阻塞模式（读立刻
    // WouldBlock，握手直接 Interrupted），必须显式恢复阻塞
    stream.set_nonblocking(false).ok();
    stream.set_nodelay(true).ok();
    // 读超时直接设在正主句柄上：Windows 的 try_clone 句柄不共享 socket
    // 选项（clone 上 set 无效）。握手期也用 25ms 轮询档，靠带期限的
    // Interrupted 续握手兜底（= open_timeout 语义）。
    stream
        .set_read_timeout(Some(READ_POLL))
        .map_err(|e| format!("设置读超时失败: {e}"))?;
    // 强断句柄（shutdown 作用于连接本身，跨句柄有效）
    let ctl = stream
        .try_clone()
        .map_err(|e| format!("复制 socket 句柄失败: {e}"))?;

    // 压缩不启用（tungstenite 无 permessage-deflate；max 16MB 对齐 max_size）
    let config = WebSocketConfig::default()
        .max_message_size(Some(16 * 1024 * 1024))
        .max_frame_size(Some(16 * 1024 * 1024));
    let connector = Connector::Rustls(crate::tls::client_config(inner.tls_insecure));
    let handshake =
        tungstenite::client_tls_with_config(request, stream, Some(config), Some(connector));
    let deadline = Instant::now() + timeout;
    let (ws, _response) = match handshake {
        Ok(x) => x,
        Err(tungstenite::HandshakeError::Failure(e)) => return Err(format!("{e}")),
        // 读超时被 tungstenite 当 WouldBlock（慢服务端）：带期限续握手
        Err(tungstenite::HandshakeError::Interrupted(mid)) => {
            let mut mid = mid;
            loop {
                match mid.handshake() {
                    Ok(x) => break x,
                    Err(tungstenite::HandshakeError::Failure(e)) => return Err(format!("{e}")),
                    Err(tungstenite::HandshakeError::Interrupted(m)) => {
                        if Instant::now() > deadline {
                            return Err("连接超时（WS 握手未完成）".to_string());
                        }
                        mid = m;
                        thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }
    };
    // 存强断句柄（读超时已在正主句柄上，握手前就设好了）
    *inner.ctl.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctl);
    Ok(ws)
}

/// 读超时/让出锁的轮询错误判定（Windows 超时 = TimedOut，unix 非阻塞 = WouldBlock）。
fn is_poll_timeout(e: &tungstenite::Error) -> bool {
    matches!(
        e,
        tungstenite::Error::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
            )
    )
}

/// f64 秒 -> Duration（下限 50ms 防呆）。
fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s.max(0.05))
}


/// 卡顿归因的故障注入（只给真机测试用；PAXUEJI_FAULT 不设就什么都不做）：
/// `net_jitter:<毫秒>` 每个下行帧处理前随机延迟 0..N ms（模拟网络抖动）；
/// `ul_slow:<毫秒>` 每个上行音频单元发送前固定延迟 N ms（模拟上行拖慢）。
mod fault {
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn spec() -> &'static Option<(String, u64)> {
        static SPEC: OnceLock<Option<(String, u64)>> = OnceLock::new();
        SPEC.get_or_init(|| {
            let v = std::env::var("PAXUEJI_FAULT").ok()?;
            let (kind, ms) = v.split_once(':')?;
            Some((kind.to_string(), ms.parse().ok()?))
        })
    }

    pub fn delay_rx() {
        if let Some((k, ms)) = spec() {
            if k == "net_jitter" && *ms > 0 {
                let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
                std::thread::sleep(Duration::from_millis(u64::from(nanos) % ms));
            }
        }
    }

    pub fn delay_tx_unit() {
        if let Some((k, ms)) = spec() {
            if k == "ul_slow" {
                std::thread::sleep(Duration::from_millis(*ms));
            }
        }
    }
}

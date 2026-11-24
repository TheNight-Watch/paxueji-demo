//! tts-center 实时 ASR 客户端：用户语音转写（UI 字幕显示用）。
//!
//! 协议对齐眼镜参考实现（Glass_Omni_Cpm_Harness_Demo 的 TtsCenterAsr.java）：
//!
//! ```text
//! wss://tts-center.ea.modelbest.co/api/v1/asr/realtime
//! open             -> {"type":"auth","token":<apiKey>}          （auth 是帧不是头）
//! session.created  -> {"type":"session.update","service_id":<N>}
//! session.updated  -> ready；之后 {"type":"input_audio_buffer.append","audio":<b64 PCM16>}
//! 下行 transcript.text.delta（delta = 当前整句，覆盖式）-> on_text(text, false)
//!      transcript.text.done （text  = 定稿）          -> on_text(text, true)
//!      transcript.done    （会话结束）               -> 自动重连
//!      error（retryable=true 或 code=overloaded 时重连，否则永久停止）
//! ```
//!
//! `service_id` 是 tts-center 侧的 ASR 服务编号（配置默认 2 是基准测试测速后选定的）。
//! 与 o5 前台共用同一路麦克风的 1 秒单元（16k 单声道 PCM16），feed 门控在 app 层。
//! 与参考实现的差异：session.updated 成功后重置退避计数（periodic
//! transcript.done 重连不会耗尽重连次数）。
//!
//! 线程模型（与 Python 版同为单连接线程，把 asyncio 事件循环换成同步轮询）：
//! 一条连接线程负责 connect → auth 帧 → 读循环；`feed()` 通过无界 crossbeam
//! channel 非阻塞投递，连接线程在读间隙（50ms 轮询粒度）排空命令：ready 时
//! 直接发 append 帧，未 ready 先缓冲（最多 10 个单元，丢最旧，对齐 Python
//! `deque(maxlen=10)`）。`start()`/`stop()` 幂等；`stop()` 在就绪态先发
//! `input_audio_buffer.commit` 再 close(1000, "Client stopped")。
//!
//! TLS：默认 rustls + webpki 根证书校验；端点为 IP/自签证书时由调用方传
//! `tls_insecure = true`（app 层默认传 true 以保持与 Python 一致的默认行为）。

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine as _;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use serde_json::{json, Value};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::CloseFrame;
use tungstenite::{HandshakeError, Message, WebSocket};

/// ready 前的音频缓冲上限（Python `deque(maxlen=10)`，自动丢最旧）。
const FEED_BUF_CAP: usize = 10;
/// 连接超时（Python `connect_timeout=15.0`，对应 websockets 的 open_timeout）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 写超时：防止对端黑洞时 send 无限阻塞。
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// 读循环轮询粒度：读 50ms 超时后回去排空 feed/stop/kick 命令。
const READ_POLL: Duration = Duration::from_millis(50);
/// 空闲 ping 间隔（Python websockets `ping_interval=20`）。
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// ping 后等 pong 的时限（websockets `ping_timeout=20`）。
const PING_TIMEOUT: Duration = Duration::from_secs(20);
/// `stop()` 等连接线程退出的上限（Python `stop(timeout=5.0)`）。
const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// 退避上限：`min(1 << retries, 8)` 秒。
const MAX_BACKOFF_SECS: u64 = 8;
/// 发出 close 帧后等对端 echo 的上限（websockets close_timeout 的缩减版，
/// 需在 stop() 的 5s join 预算内）。
const CLOSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// 空闲 ping 载荷（控制帧须 <125 字节）。
const PING_PAYLOAD: &[u8] = b"o5-asr-keepalive";

/// 连接线程收到的命令（feed / 停止 / 换端点踢连接）。
enum Cmd {
    /// 麦克风线程投递的 1 秒 PCM16 单元 + feed 时刻（重连后丢弃陈旧用）。
    Feed(Vec<u8>, Instant),
    /// 停止（closed 标志置位后由读循环执行优雅关闭）。
    Stop,
    /// 端点已更新：关闭当前连接触发重连。
    Kick,
}

/// 转写回调（text, final）。
type TextFn = Box<dyn Fn(String, bool) + Send>;
/// 状态回调。
type StatusFn = Box<dyn Fn(&str) + Send>;
/// 测试注入的退避 sleep。
#[cfg(test)]
type SleepFn = Box<dyn Fn(Duration) + Send + Sync>;

/// 跨线程共享的内部状态（回调放进 Mutex 以满足 Sync；回调本身只要求 Send）。
struct Inner {
    api_key: String,
    service_id: i64,
    max_reconnects: u32,
    tls_insecure: bool,
    on_text: Mutex<TextFn>,
    on_status: Mutex<StatusFn>,

    tx: Sender<Cmd>,
    rx: Receiver<Cmd>,
    endpoint: Mutex<String>,
    closed: AtomicBool,
    started: AtomicBool,
    finished: AtomicBool,
    ready: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// 连接线程退出信号（stop() 的限时 join 用）。
    done_tx: Sender<()>,
    done_rx: Receiver<()>,
    /// 测试注入的退避 sleep（记录退避序列）。
    #[cfg(test)]
    sleeper: Mutex<SleepFn>,
}

impl Inner {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn set_closed(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    fn set_ready(&self, v: bool) {
        self.ready.store(v, Ordering::SeqCst);
    }

    fn current_endpoint(&self) -> String {
        self.endpoint.lock().expect("asr 内部锁中毒").clone()
    }

    /// 连接状态变化 + 日志（对齐 Python `_status`：先 log.info 再回调）。
    fn status(&self, text: &str) {
        log::info!("{}", text);
        let cb = self.on_status.lock().expect("asr 内部锁中毒");
        cb(text);
    }

    /// 转写文本回调（对齐 Python `_emit`；回调约定快且不 panic、不再调 stop）。
    fn emit_text(&self, text: String, final_: bool) {
        let cb = self.on_text.lock().expect("asr 内部锁中毒");
        cb(text, final_);
    }
}

/// 重连退避 sleep（测试注入用）。
#[cfg(not(test))]
fn sleep_backoff(_inner: &Inner, d: Duration) {
    std::thread::sleep(d);
}

#[cfg(test)]
fn sleep_backoff(inner: &Inner, d: Duration) {
    let f = inner.sleeper.lock().expect("asr 测试 sleeper 锁中毒");
    f(d);
}

/// tts-center 实时 ASR 连接。start/stop 一次性使用，会话重启请重建实例
/// （app 层 start_session/stop_session 里做，与 Python main 一致）。
pub struct AsrClient {
    inner: Arc<Inner>,
}

impl AsrClient {
    /// 新建客户端。参数：
    /// - `endpoint`：`ws://` 或 `wss://` URL（`http(s)://` 宽容接受）
    /// - `api_key`：auth 帧的 token
    /// - `service_id`：tts-center 服务编号（配置默认 2，测速后选定）
    /// - `max_reconnects`：重连次数上限（默认 3；session.updated 成功会归零）
    /// - `tls_insecure`：跳过证书校验（IP/自签端点用；app 层默认传 true
    ///   保持与 Python 行为一致）
    ///
    /// 回调大多在连接线程触发（stop() 的“ASR 已停止”在 stop 调用方线程），
    /// 要求快速返回、不 panic、不要再调用本客户端的 stop/start。
    pub fn new(
        endpoint: String,
        api_key: String,
        service_id: i64,
        max_reconnects: u32,
        tls_insecure: bool,
        on_text: Box<dyn Fn(String, bool) + Send>,
        on_status: Box<dyn Fn(&str) + Send>,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        AsrClient {
            inner: Arc::new(Inner {
                api_key,
                service_id,
                max_reconnects,
                tls_insecure,
                on_text: Mutex::new(on_text),
                on_status: Mutex::new(on_status),
                tx,
                rx,
                endpoint: Mutex::new(endpoint),
                closed: AtomicBool::new(false),
                started: AtomicBool::new(false),
                finished: AtomicBool::new(false),
                ready: AtomicBool::new(false),
                thread: Mutex::new(None),
                done_tx,
                done_rx,
                #[cfg(test)]
                sleeper: Mutex::new(Box::new(std::thread::sleep)),
            }),
        }
    }

    /// 启动连接线程（幂等；已停止的实例再 start 是 no-op——会话重启请重建实例）。
    pub fn start(&self) {
        if self.inner.is_closed() {
            return;
        }
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let inner = self.inner.clone();
        let spawned = std::thread::Builder::new()
            .name("o5-asr".to_string())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    thread_main(&inner)
                }));
                if let Err(p) = r {
                    log::error!("ASR 线程异常退出: {:?}", p);
                }
                inner.finished.store(true, Ordering::SeqCst);
                let _ = inner.done_tx.send(());
            });
        match spawned {
            Ok(h) => *self.inner.thread.lock().expect("asr 内部锁中毒") = Some(h),
            Err(e) => log::error!("ASR 连接线程启动失败: {}", e),
        }
    }

    /// 优雅停止（幂等）：就绪态先发 `{"type":"input_audio_buffer.commit"}`，
    /// 然后 ws.close(1000, "Client stopped")；等连接线程退出（≤5s；退避/连接
    /// 阻塞时放弃等待，线程后台自行结束）。最后回调 “ASR 已停止”。
    pub fn stop(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.inner.tx.send(Cmd::Stop);
        let handle = self.inner.thread.lock().expect("asr 内部锁中毒").take();
        if let Some(handle) = handle {
            match self.inner.done_rx.recv_timeout(STOP_JOIN_TIMEOUT) {
                Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    let _ = handle.join();
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    log::warn!(
                        "ASR 线程未在 {}s 内退出，放弃等待",
                        STOP_JOIN_TIMEOUT.as_secs()
                    );
                }
            }
        }
        self.inner.status("ASR 已停止");
    }

    /// 麦克风线程调用：喂 1 秒 PCM16 单元（16k 单声道）。非阻塞投递；未
    /// ready 时缓冲（≤10，丢最旧）。启动前/停止后的 feed 直接丢弃（对齐
    /// Python `_loop is None` / `_closed` 的行为）。
    pub fn feed(&self, pcm16: Vec<u8>) {
        if self.inner.is_closed() || pcm16.is_empty() {
            return;
        }
        if !self.inner.started.load(Ordering::SeqCst) {
            return;
        }
        let _ = self.inner.tx.try_send(Cmd::Feed(pcm16, Instant::now()));
    }

    /// 网络模式切换：换端点并立即重连（旧连接被关闭后由重连循环用新端点重建）。
    pub fn update_endpoint(&self, url: String) {
        if self.inner.is_closed() || url.is_empty() {
            return;
        }
        let mut ep = self.inner.endpoint.lock().expect("asr 内部锁中毒");
        if url == *ep {
            return;
        }
        *ep = url;
        drop(ep);
        let _ = self.inner.tx.send(Cmd::Kick);
    }

    /// 会话是否就绪（session.updated 之后）。
    pub fn ready(&self) -> bool {
        self.inner.is_ready()
    }

    /// 连接线程是否还在运行。
    pub fn running(&self) -> bool {
        self.inner.started.load(Ordering::SeqCst)
            && !self.inner.finished.load(Ordering::SeqCst)
    }

    /// 测试注入退避 sleep。
    #[cfg(test)]
    #[doc(hidden)]
    pub fn set_sleeper_for_test(&self, f: Box<dyn Fn(Duration) + Send + Sync>) {
        *self.inner.sleeper.lock().expect("asr 测试 sleeper 锁中毒") = f;
    }
}

// ---------------------------------------------------------------------------
// 连接线程
// ---------------------------------------------------------------------------

/// 连接线程主循环（对应 Python `_run`）：connect → auth → 读循环 → 退避重连。
fn thread_main(inner: &Inner) {
    let mut retries: u32 = 0;
    // ready 前的缓冲（cap 10 丢最旧）；只承载"本连接"的未就绪音频
    // （reader_loop 开头清空 + t_conn 过滤，断线期间积压的陈旧一律丢）。
    let mut feed_buf: VecDeque<Vec<u8>> = VecDeque::with_capacity(FEED_BUF_CAP);
    loop {
        if inner.is_closed() {
            break;
        }
        match run_connection(inner, &mut retries, &mut feed_buf) {
            // reader 正常返回：transcript.done / retryable error / 换端点 kick /
            // 服务器主动关闭 → 走重连逻辑。
            Ok(()) => {}
            Err(e) => {
                if !inner.is_closed() {
                    log::warn!("ASR 连接异常: {}", e);
                }
            }
        }
        if inner.is_closed() {
            break;
        }
        if retries >= inner.max_reconnects {
            inner.status("ASR 重连次数用尽，字幕停用");
            inner.emit_text(String::new(), true); // 通知上层收束
            break;
        }
        // Python：delay = min(1 << retries, 8)，先算 delay 再自增，状态里显示自增后的计数。
        let delay_secs = if retries >= 3 {
            MAX_BACKOFF_SECS
        } else {
            1u64 << retries
        };
        retries += 1;
        inner.status(&format!(
            "ASR {}s 后重连（{}/{}）",
            delay_secs, retries, inner.max_reconnects
        ));
        sleep_backoff(inner, Duration::from_secs(delay_secs));
    }
    inner.set_ready(false);
}

/// 一次完整的连接尝试（对应 Python `_run` 的 `async with` 块）。
fn run_connection(
    inner: &Inner,
    retries: &mut u32,
    feed_buf: &mut VecDeque<Vec<u8>>,
) -> Result<(), String> {
    let endpoint = inner.current_endpoint();
    let url = parse_endpoint(&endpoint)?;
    let sock = tcp_connect(&url)?;
    sock.set_nodelay(true).map_err(|e| e.to_string())?;
    // 握手期间用 15s 超时，握手完成后切到 50ms 轮询。
    sock.set_read_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|e| e.to_string())?;
    sock.set_write_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let stream = if url.secure {
        Conn::Tls(Box::new(tls_wrap(sock, &url, inner.tls_insecure)?))
    } else {
        Conn::Plain(sock)
    };
    let (mut ws, _response) = tungstenite::client(url.request.as_str(), stream).map_err(
        |e| match e {
            HandshakeError::Failure(err) => err.to_string(),
            HandshakeError::Interrupted(_) => "WS 握手未完成".to_string(),
        },
    )?;
    ws.get_ref()
        .set_timeouts(Some(READ_POLL), Some(WRITE_TIMEOUT))
        .map_err(|e| e.to_string())?;

    inner.set_ready(false);
    inner.status("ASR 连接中");
    send_json(&mut ws, &json!({ "type": "auth", "token": inner.api_key }).to_string())?;
    // 本连接的时间边界：此前 feed 的都是断线/退避期间攒的陈旧音频，
    // 补发给服务端只会转写出跨越用户句+AI 口播回声的拼接长句（02 实测
    // "用户的话在 AI 回复后原样再来一遍"的根源），一律丢弃。
    let t_conn = Instant::now();
    let r = reader_loop(inner, &mut ws, retries, feed_buf, t_conn);
    inner.set_ready(false);
    r
}

/// 读循环（对应 Python `_reader`）：排空 feed/stop/kick 命令 + 处理下行帧。
fn reader_loop(
    inner: &Inner,
    ws: &mut Ws,
    retries: &mut u32,
    feed_buf: &mut VecDeque<Vec<u8>>,
    t_conn: Instant,
) -> Result<(), String> {
    let mut last_ping = Instant::now();
    let mut ping_outstanding_since: Option<Instant> = None;
    let mut stale_dropped: usize = 0;
    // 上一连接 ready 前攒的残留同样是陈旧音频，清掉（本连接的会在排空
    // rx 时按 t_conn 过滤后重新入队）。
    feed_buf.clear();
    loop {
        // 1) 排空命令通道（feed 在 ready 时直接发，否则入缓冲；早于本
        //    连接建立的陈旧 feed 丢弃——见 run_connection 的 t_conn 注释）。
        let mut kicked = false;
        loop {
            match inner.rx.try_recv() {
                Ok(Cmd::Feed(pcm16, t)) => {
                    if t < t_conn {
                        stale_dropped += 1;
                        continue;
                    }
                    if inner.is_ready() {
                        if let Err(e) = send_audio(ws, &pcm16) {
                            log::warn!("ASR 发送失败: {}", e);
                        }
                    } else {
                        if feed_buf.len() == FEED_BUF_CAP {
                            feed_buf.pop_front(); // 丢最旧
                        }
                        feed_buf.push_back(pcm16);
                    }
                }
                Ok(Cmd::Kick) => kicked = true,
                Ok(Cmd::Stop) => {} // closed 标志接管
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if stale_dropped > 0 {
            log::info!("ASR 重连后丢弃陈旧音频 {stale_dropped} unit（断线期间积压）");
            stale_dropped = 0;
        }
        if kicked {
            // 端点已更新：关闭当前连接，重连循环会用新端点重建。
            close_ws(ws, 1000, "endpoint changed");
            return Ok(());
        }
        // 2) 停止：就绪态先发 commit，再 close(1000, "Client stopped")。
        if inner.is_closed() {
            if inner.is_ready() {
                let _ = send_json(ws, &json!({ "type": "input_audio_buffer.commit" }).to_string());
            }
            close_ws(ws, 1000, "Client stopped");
            return Ok(());
        }
        // 3) 周期 ping（对齐 websockets ping_interval=20，不管流量照发）。
        if last_ping.elapsed() >= PING_INTERVAL {
            if let Err(e) = ws.send(Message::Ping(PING_PAYLOAD.into())) {
                log::warn!("ASR ping 发送失败: {}", e);
            }
            let _ = ws.flush();
            if ping_outstanding_since.is_none() {
                ping_outstanding_since = Some(Instant::now());
            }
            last_ping = Instant::now();
        }
        // 4) 死链检测：ping 发出后 PING_TIMEOUT 内没有任何下行（含 pong）。
        if let Some(t0) = ping_outstanding_since {
            if t0.elapsed() >= PING_TIMEOUT {
                return Err("keepalive 超时（ping 后无 pong）".to_string());
            }
        }
        // 5) 读（50ms 轮询超时后回到 1）。
        match ws.read() {
            Ok(msg) => {
                ping_outstanding_since = None;
                match msg {
                    Message::Text(text) => {
                        if handle_frame(inner, ws, text.as_str(), retries, feed_buf)? {
                            return Ok(()); // transcript.done / error → 结束本连接
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => {}
                    Message::Close(_) => return Ok(()), // 服务器主动关闭 → 重连
                    Message::Frame(_) => {}
                }
            }
            Err(e) if is_poll_timeout(&e) => {}
            Err(tungstenite::Error::ConnectionClosed)
            | Err(tungstenite::Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// 处理一条下行 JSON 帧；返回 true 表示结束本连接（走重连/停止逻辑）。
fn handle_frame(
    inner: &Inner,
    ws: &mut Ws,
    text: &str,
    retries: &mut u32,
    feed_buf: &mut VecDeque<Vec<u8>>,
) -> Result<bool, String> {
    let frame: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Ok(false), // 解析失败忽略
    };
    let type_ = frame.get("type").and_then(Value::as_str).unwrap_or("");
    match type_ {
        "session.created" => {
            send_json(
                ws,
                &json!({ "type": "session.update", "service_id": inner.service_id }).to_string(),
            )?;
        }
        "session.updated" => {
            inner.set_ready(true);
            *retries = 0; // 成功就绪即重置退避计数
            inner.status("ASR 实时识别就绪");
            while let Some(pcm16) = feed_buf.pop_front() {
                // 把未就绪期间攒的音频补上（保持 FIFO）。
                if let Err(e) = send_audio(ws, &pcm16) {
                    log::warn!("ASR 发送失败: {}", e);
                }
            }
        }
        "transcript.text.delta" => {
            let t = field_text(&frame, "delta");
            if !t.is_empty() {
                inner.emit_text(t, false);
            }
        }
        "transcript.text.done" => {
            let t = field_text(&frame, "text");
            if !t.is_empty() {
                inner.emit_text(t, true);
            }
        }
        "transcript.done" => {
            inner.status("ASR 会话结束，重连");
            return Ok(true);
        }
        "error" => {
            let retryable = py_truthy(frame.get("retryable"))
                || frame.get("code").and_then(Value::as_str) == Some("overloaded");
            if retryable {
                inner.status("ASR 服务忙，重连");
                return Ok(true);
            }
            let code = frame
                .get("code")
                .map(value_py_str)
                .unwrap_or_else(|| "None".to_string());
            let message: String = field_text(&frame, "message").chars().take(80).collect();
            inner.status(&format!("ASR 错误（不再重连）: {} {}", code, message));
            inner.set_closed();
            return Ok(true);
        }
        _ => {} // 其他类型忽略
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// 帧发送
// ---------------------------------------------------------------------------

/// 类型别名：底层连接（明文 TCP 或 rustls 包装的 TCP）上的 WebSocket。
type Ws = WebSocket<Conn>;

fn send_json(ws: &mut Ws, s: &str) -> Result<(), String> {
    ws.send(Message::text(s)).map_err(|e| e.to_string())?;
    ws.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// 发送 `input_audio_buffer.append`（失败只告警不重连，对齐 Python `_send_audio`）。
fn send_audio(ws: &mut Ws, pcm16: &[u8]) -> Result<(), String> {
    let audio = base64::engine::general_purpose::STANDARD.encode(pcm16);
    send_json(
        ws,
        &json!({ "type": "input_audio_buffer.append", "audio": audio }).to_string(),
    )
}

/// 关闭连接：发送 close 帧，驱动关闭握手（等对端 echo，≤2s）后丢弃 socket。
/// （对齐 Python websockets 的关闭语义；也避免发完帧立刻 drop 触发 RST）
fn close_ws(ws: &mut Ws, code: u16, reason: &str) {
    let _ = ws.close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    }));
    let _ = ws.flush();
    let deadline = Instant::now() + CLOSE_DRAIN_TIMEOUT;
    while Instant::now() < deadline {
        match ws.read() {
            Ok(_) => {}
            Err(e) if is_poll_timeout(&e) => {}
            Err(_) => break,
        }
    }
}

fn is_poll_timeout(e: &tungstenite::Error) -> bool {
    matches!(
        e,
        tungstenite::Error::Io(ref io)
            if io.kind() == io::ErrorKind::WouldBlock || io.kind() == io::ErrorKind::TimedOut
    )
}

/// `str(frame.get(key) or "")`：字符串原样；非零数字按 Python str()；其余 falsy → 空。
fn field_text(frame: &Value, key: &str) -> String {
    match frame.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => {
            if n.as_f64().is_some_and(|f| f != 0.0) {
                n.to_string()
            } else {
                String::new()
            }
        }
        Some(Value::Bool(true)) => "True".to_string(),
        _ => String::new(),
    }
}

/// JSON 值 → Python `str()` 近似（错误帧展示用）。
fn value_py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// Python 真值语义（`bool(x)`）。
fn py_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

// ---------------------------------------------------------------------------
// 连接建立（URL 解析 / TCP / TLS）
// ---------------------------------------------------------------------------

/// 解析后的端点（scheme 归一化为 ws/wss 的完整请求 URL + 连接用 host/port）。
struct EndpointUrl {
    secure: bool,
    host: String,
    port: u16,
    request: String,
}

/// 极简 ws/wss URL 解析（scheme://[userinfo@]host[:port]/path?query）。
/// http/https 宽容地按 ws/wss 处理；端口缺省 80/443。
fn parse_endpoint(url: &str) -> Result<EndpointUrl, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("ASR 端点为空".to_string());
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("ASR 端点缺少 scheme: {}", url))?;
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "ws" | "http" => false,
        "wss" | "https" => true,
        other => return Err(format!("ASR 端点 scheme 不支持: {}", other)),
    };
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let hostport = match authority.rsplit_once('@') {
        Some((_, h)) => h, // 保守丢弃 userinfo（端点一般不带）
        None => authority,
    };
    let default_port = if secure { 443 } else { 80 };
    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        // IPv6 字面量 [::1]:port
        let (h, p) = rest
            .split_once(']')
            .ok_or_else(|| format!("ASR 端点 IPv6 地址格式错误: {}", url))?;
        let port = match p.strip_prefix(':') {
            Some(p) => p
                .parse::<u16>()
                .map_err(|_| format!("ASR 端点端口非法: {}", p))?,
            None => default_port,
        };
        (h.to_string(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => {
                let port = p
                    .parse::<u16>()
                    .map_err(|_| format!("ASR 端点端口非法: {}", p))?;
                (h.to_string(), port)
            }
            None => (hostport.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err(format!("ASR 端点 host 为空: {}", url));
    }
    let request = format!(
        "{}://{}{}",
        if secure { "wss" } else { "ws" },
        authority,
        path
    );
    Ok(EndpointUrl {
        secure,
        host,
        port,
        request,
    })
}

fn tcp_connect(url: &EndpointUrl) -> Result<TcpStream, String> {
    let addrs: Vec<SocketAddr> = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS 解析失败 {}: {}", url.host, e))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("DNS 解析无结果: {}", url.host));
    }
    let mut last_err = String::from("无地址");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = format!("{}: {}", addr, e),
        }
    }
    Err(format!("TCP 连接失败 {}", last_err))
}

fn tls_wrap(
    sock: TcpStream,
    url: &EndpointUrl,
    insecure: bool,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS 协议版本初始化失败: {}", e))?;
    let config = if insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(InsecureNoVerify(provider)))
            .with_no_client_auth()
    } else {
        builder
            .with_root_certificates(rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            })
            .with_no_client_auth()
    };
    let name = rustls::pki_types::ServerName::try_from(url.host.clone())
        .map_err(|_| format!("TLS ServerName 非法: {}", url.host))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|e| format!("TLS 连接初始化失败: {}", e))?;
    Ok(rustls::StreamOwned::new(conn, sock))
}

/// tls_insecure 用的全放行证书校验器（IP/自签端点）。
#[derive(Debug)]
struct InsecureNoVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for InsecureNoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// 底层连接：明文 TCP 或 rustls 包装。
enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Conn {
    fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>) -> io::Result<()> {
        let tcp = match self {
            Conn::Plain(s) => s,
            Conn::Tls(s) => &s.sock,
        };
        tcp.set_read_timeout(read)?;
        tcp.set_write_timeout(write)
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

// ---------------------------------------------------------------------------
// 测试：tungstenite 同步桩服务器，脚本化帧序
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    use tungstenite::accept;

    /// 桩服务器单步脚本。
    #[derive(Debug)]
    enum Step {
        /// 等待并记录客户端下一条文本帧。
        Recv,
        /// 向客户端发送一条文本帧。
        Send(String),
        /// 等待客户端 close（记录 code/reason；期间到达的文本帧也记录）。
        ExpectClose,
        /// 挂起指定毫秒（期间继续读/应答，记录到达的帧）。
        SleepMs(u64),
    }

    /// 桩服务器观测（客户端行为）。
    #[derive(Debug)]
    enum Obs {
        Connected,
        Text(String),
        Closed(u16, String),
    }

    /// 启动脚本化桩服务器：每个连接执行一段脚本，观测送入 channel。
    fn spawn_stub(scripts: Vec<Vec<Step>>) -> (String, mpsc::Receiver<Obs>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("桩服务器绑定失败");
        let url = format!("ws://{}/", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("asr-stub".to_string())
            .spawn(move || {
                for script in scripts {
                    let Ok((stream, _)) = listener.accept() else { break };
                    let _ = stream.set_nodelay(true);
                    let Ok(mut ws) = accept(stream) else { continue };
                    let _ = tx.send(Obs::Connected);
                    let _ = ws
                        .get_ref()
                        .set_read_timeout(Some(Duration::from_millis(10)));
                    run_script(&mut ws, &script, &tx);
                }
            })
            .expect("启动桩服务器线程失败");
        (url, rx)
    }

    fn run_script(ws: &mut WebSocket<TcpStream>, script: &[Step], tx: &mpsc::Sender<Obs>) {
        for step in script {
            match step {
                Step::Recv => {
                    // 等下一条客户端文本帧（期间到达的 close 也记录）。
                    loop {
                        match pump(ws, tx) {
                            Pump::Text => break,
                            Pump::Other | Pump::Idle => {}
                            Pump::Dead => return,
                        }
                    }
                }
                Step::Send(s) => {
                    if ws.send(Message::text(s.clone())).is_err() || ws.flush().is_err() {
                        return;
                    }
                }
                Step::ExpectClose => {
                    loop {
                        match pump(ws, tx) {
                            Pump::Text | Pump::Other | Pump::Idle => {}
                            Pump::Dead => return,
                        }
                    }
                }
                Step::SleepMs(ms) => {
                    let until = Instant::now() + Duration::from_millis(*ms);
                    while Instant::now() < until {
                        match pump(ws, tx) {
                            Pump::Idle => thread::sleep(Duration::from_millis(5)),
                            Pump::Text | Pump::Other => {}
                            Pump::Dead => return,
                        }
                    }
                }
            }
        }
    }

    /// 读一条事件：Text=客户端文本帧（已记录），Other=控制帧，Idle=读超时，
    /// Dead=连接结束（close 帧已记录并驱动关闭握手，或对端断链）。
    fn pump(ws: &mut WebSocket<TcpStream>, tx: &mpsc::Sender<Obs>) -> Pump {
        match ws.read() {
            Ok(Message::Text(t)) => {
                let _ = tx.send(Obs::Text(t.to_string()));
                Pump::Text
            }
            Ok(Message::Ping(_)) => {
                let _ = ws.flush(); // pong 自动排队，flush 发出
                Pump::Other
            }
            Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => Pump::Other,
            Ok(Message::Close(Some(f))) => {
                let _ = tx.send(Obs::Closed(u16::from(f.code), f.reason.to_string()));
                drain_until_closed(ws);
                Pump::Dead
            }
            Ok(_) => Pump::Dead,
            Err(e) if is_poll_timeout(&e) => Pump::Idle,
            Err(_) => Pump::Dead,
        }
    }

    /// pump 的结果分类。
    #[derive(Debug, PartialEq)]
    enum Pump {
        Text,
        Other,
        Idle,
        Dead,
    }

    /// 驱动关闭握手（tungstenite 读循环里自动回 close echo）。
    fn drain_until_closed(ws: &mut WebSocket<TcpStream>) {
        loop {
            match ws.read() {
                Ok(_) => {}
                Err(e) if is_poll_timeout(&e) => {}
                Err(_) => return,
            }
        }
    }

    // ---- 测试客户端工具 --------------------------------------------------

    struct Harness {
        text_rx: mpsc::Receiver<(String, bool)>,
        status_rx: mpsc::Receiver<String>,
        sleeps: Arc<Mutex<Vec<Duration>>>,
        client: AsrClient,
    }

    fn make_client(endpoint: &str, max_reconnects: u32) -> Harness {
        let (ttx, text_rx) = mpsc::channel();
        let (stx, status_rx) = mpsc::channel();
        let sleeps = Arc::new(Mutex::new(Vec::new()));
        let s2 = sleeps.clone();
        let client = AsrClient::new(
            endpoint.to_string(),
            "test-key".to_string(),
            7,
            max_reconnects,
            false,
            Box::new(move |t, f| {
                ttx.send((t, f)).unwrap();
            }),
            Box::new(move |s| {
                stx.send(s.to_string()).unwrap();
            }),
        );
        client.set_sleeper_for_test(Box::new(move |d| {
            s2.lock().unwrap().push(d);
            thread::sleep(Duration::from_millis(1));
        }));
        Harness {
            text_rx,
            status_rx,
            sleeps,
            client,
        }
    }

    impl Harness {
        fn status(&self) -> String {
            self.status_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("等状态回调超时")
        }

        fn text(&self) -> (String, bool) {
            self.text_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("等转写回调超时")
        }

        fn obs_json(&self, obs: &mpsc::Receiver<Obs>) -> Value {
            loop {
                match obs.recv_timeout(Duration::from_secs(5)).expect("等客户端帧超时") {
                    Obs::Text(t) => return serde_json::from_str(&t).expect("客户端帧应为 JSON"),
                    Obs::Connected => continue, // 连接建立事件，不是帧
                    other => panic!("期望文本帧，得到 {:?}", other),
                }
            }
        }

        fn obs_close(&self, obs: &mpsc::Receiver<Obs>) -> (u16, String) {
            loop {
                match obs.recv_timeout(Duration::from_secs(5)).expect("等关闭帧超时") {
                    Obs::Closed(c, r) => return (c, r),
                    Obs::Connected => continue,
                    other => panic!("期望关闭帧，得到 {:?}", other),
                }
            }
        }

        /// 等连接线程自行退出（error/耗尽路径不经过 stop() 的 join）。
        fn wait_thread_exit(&self) {
            for _ in 0..300 {
                if !self.client.running() {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("连接线程未在 3s 内退出");
        }
    }

    fn decode_b64(s: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .expect("append 帧的 audio 应为合法 base64")
    }

    /// auth → created → update → updated 的前半段脚本（auth 后停在哪一步由调用方拼接）。
    fn handshake_script() -> Vec<Step> {
        vec![
            Step::Recv, // auth
            Step::Send(r#"{"type":"session.created"}"#.into()),
            Step::Recv, // session.update
        ]
    }

    /// 全套握手帧序（立即就绪，无缓冲窗口）。
    fn ready_script() -> Vec<Step> {
        let mut s = handshake_script();
        s.push(Step::Send(r#"{"type":"session.updated"}"#.into()));
        s
    }

    const CREATED: &str = r#"{"type":"session.created"}"#;
    const UPDATED: &str = r#"{"type":"session.updated"}"#;

    // ---- 测试 --------------------------------------------------------

    /// 完整帧序：auth → created → update → updated → 缓冲 flush（FIFO）→
    /// transcript 回调 → stop() 的 commit + close(1000)。
    #[test]
    fn full_protocol_flow() {
        let mut script = handshake_script();
        script.push(Step::SleepMs(400)); // 窗口：测试线程此刻喂 3 个单元（未 ready）
        script.push(Step::Send(UPDATED.into()));
        script.push(Step::Recv);
        script.push(Step::Recv);
        script.push(Step::Recv); // flush 的 3 个 append
        script.push(Step::Send(
            r#"{"type":"transcript.text.delta","delta":"你好"}"#.into(),
        ));
        script.push(Step::Send(
            r#"{"type":"transcript.text.done","text":"你好世界"}"#.into(),
        ));
        script.push(Step::SleepMs(200));
        script.push(Step::ExpectClose); // stop()
        let (url, obs) = spawn_stub(vec![script]);

        let h = make_client(&url, 3);
        h.client.start();
        assert_eq!(h.status(), "ASR 连接中");

        // auth 帧（token 在帧里，不是头）
        let auth = h.obs_json(&obs);
        assert_eq!(auth["type"], "auth");
        assert_eq!(auth["token"], "test-key");

        // created → 客户端回 session.update（service_id 原样带回）
        let upd = h.obs_json(&obs);
        assert_eq!(upd["type"], "session.update");
        assert_eq!(upd["service_id"], 7);

        // ready 前缓冲 3 个单元
        h.client.feed(b"u0".to_vec());
        h.client.feed(b"u1".to_vec());
        h.client.feed(b"u2".to_vec());

        assert_eq!(h.status(), "ASR 实时识别就绪");
        assert!(h.client.ready());

        // flush 顺序：FIFO，base64 PCM16 原样
        for i in 0..3u8 {
            let f = h.obs_json(&obs);
            assert_eq!(f["type"], "input_audio_buffer.append");
            assert_eq!(decode_b64(f["audio"].as_str().unwrap()), [b'u', i + b'0']);
        }

        // 转写回调：delta 是覆盖式整句，done 是定稿
        assert_eq!(h.text(), ("你好".to_string(), false));
        assert_eq!(h.text(), ("你好世界".to_string(), true));

        // stop：就绪态先 commit 再 close(1000, "Client stopped")
        h.client.stop();
        let commit = h.obs_json(&obs);
        assert_eq!(commit["type"], "input_audio_buffer.commit");
        let (code, reason) = h.obs_close(&obs);
        assert_eq!((code, reason.as_str()), (1000, "Client stopped"));
        assert_eq!(h.status(), "ASR 已停止");
        assert!(!h.client.running());

        // 幂等：再 stop 无新状态
        h.client.stop();
        assert!(h.status_rx.try_recv().is_err());
        // 停止后的 feed 丢弃（不 panic、无帧）
        h.client.feed(b"late".to_vec());
        thread::sleep(Duration::from_millis(150));
        assert!(obs.try_recv().is_err());
    }

    /// 未 ready 时缓冲 ≤10（丢最旧，保持 FIFO）。
    #[test]
    fn buffer_caps_at_ten_drops_oldest() {
        let mut script = handshake_script();
        script.push(Step::SleepMs(500)); // 窗口：喂 12 个单元
        script.push(Step::Send(UPDATED.into()));
        for _ in 0..10 {
            script.push(Step::Recv);
        }
        script.push(Step::SleepMs(200));
        script.push(Step::ExpectClose);
        let (url, obs) = spawn_stub(vec![script]);

        let h = make_client(&url, 3);
        h.client.start();
        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs); // auth
        let _ = h.obs_json(&obs); // session.update

        for i in 0..12u8 {
            h.client.feed(format!("unit-{i:02}").into_bytes());
        }

        assert_eq!(h.status(), "ASR 实时识别就绪");
        // 只 flush 最后 10 个，顺序保持
        for i in 2..12u8 {
            let f = h.obs_json(&obs);
            assert_eq!(f["type"], "input_audio_buffer.append");
            assert_eq!(
                decode_b64(f["audio"].as_str().unwrap()),
                format!("unit-{i:02}").into_bytes()
            );
        }

        h.client.stop();
        let commit = h.obs_json(&obs);
        assert_eq!(commit["type"], "input_audio_buffer.commit");
        let (code, reason) = h.obs_close(&obs);
        assert_eq!((code, reason.as_str()), (1000, "Client stopped"));
        assert_eq!(h.status(), "ASR 已停止");
        // 无第 11 个 append
        assert!(obs.try_recv().is_err());
    }

    /// transcript.done 触发重连且 session.updated 重置退避；垃圾帧被忽略；
    /// 不可重试 error 永久停止（不触发 on_text("", true)）。
    #[test]
    fn transcript_done_reconnect_resets_backoff_and_fatal_error_stops() {
        let scripts = vec![
            vec![
                Step::Recv,
                Step::Send("这不是 JSON".into()), // 解析失败忽略
                Step::Send(CREATED.into()),
                Step::Recv,
                Step::Send(UPDATED.into()),
                Step::Send(r#"{"type":"transcript.done"}"#.into()),
            ],
            vec![
                Step::Recv,
                Step::Send(CREATED.into()),
                Step::Recv,
                Step::Send(UPDATED.into()),
                Step::Send(r#"{"type":"transcript.done"}"#.into()),
            ],
            vec![
                Step::Recv,
                Step::Send(CREATED.into()),
                Step::Recv,
                Step::Send(UPDATED.into()),
                Step::Send(r#"{"type":"error","code":"boom","message":"炸了"}"#.into()),
                Step::SleepMs(300),
            ],
        ];
        let (url, obs) = spawn_stub(scripts);

        let h = make_client(&url, 3);
        h.client.start();

        // 连接 1
        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs); // auth
        let _ = h.obs_json(&obs); // session.update
        assert_eq!(h.status(), "ASR 实时识别就绪");
        assert_eq!(h.status(), "ASR 会话结束，重连");
        assert_eq!(h.status(), "ASR 1s 后重连（1/3）");

        // 连接 2（退避计数已重置：仍是 1s）
        let _ = h.obs_json(&obs);
        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs);
        assert_eq!(h.status(), "ASR 实时识别就绪");
        assert_eq!(h.status(), "ASR 会话结束，重连");
        assert_eq!(h.status(), "ASR 1s 后重连（1/3）");

        // 连接 3：不可重试 error → 永久停止
        let _ = h.obs_json(&obs);
        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs);
        assert_eq!(h.status(), "ASR 实时识别就绪");
        assert_eq!(h.status(), "ASR 错误（不再重连）: boom 炸了");

        // 退避序列：两次 1s（重置生效）
        assert_eq!(*h.sleeps.lock().unwrap(), vec![Duration::from_secs(1), Duration::from_secs(1)]);
        // 不触发收束回调（那是重连耗尽才有的）
        assert!(h.text_rx.try_recv().is_err());
        h.wait_thread_exit();
        assert!(!h.client.running());
        // error 置 closed：stop() 幂等早退，不再回“ASR 已停止”
        h.client.stop();
        assert!(h.status_rx.try_recv().is_err());
        // feed 也被丢弃
        h.client.feed(b"x".to_vec());
    }

    /// error retryable=true / code=overloaded 都重连。
    #[test]
    fn retryable_and_overloaded_errors_reconnect() {
        let scripts = vec![
            {
                let mut s = ready_script();
                s.push(Step::Send(r#"{"type":"error","retryable":true}"#.into()));
                s
            },
            {
                let mut s = ready_script();
                s.push(Step::Send(r#"{"type":"error","code":"overloaded"}"#.into()));
                s
            },
            {
                let mut s = ready_script();
                s.push(Step::Send(
                    r#"{"type":"transcript.text.delta","delta":"hi"}"#.into(),
                ));
                s.push(Step::Send(
                    r#"{"type":"transcript.text.done","text":"hi"}"#.into(),
                ));
                s.push(Step::SleepMs(200));
                s.push(Step::ExpectClose);
                s
            },
        ];
        let (url, obs) = spawn_stub(scripts);

        let h = make_client(&url, 3);
        h.client.start();

        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs);
        let _ = h.obs_json(&obs);
        assert_eq!(h.status(), "ASR 实时识别就绪");
        assert_eq!(h.status(), "ASR 服务忙，重连");
        assert_eq!(h.status(), "ASR 1s 后重连（1/3）");

        let _ = h.obs_json(&obs);
        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs);
        assert_eq!(h.status(), "ASR 实时识别就绪");
        assert_eq!(h.status(), "ASR 服务忙，重连");
        assert_eq!(h.status(), "ASR 1s 后重连（1/3）");

        let _ = h.obs_json(&obs);
        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs);
        assert_eq!(h.status(), "ASR 实时识别就绪");
        assert_eq!(h.text(), ("hi".to_string(), false));
        assert_eq!(h.text(), ("hi".to_string(), true));

        h.client.stop();
        let commit = h.obs_json(&obs);
        assert_eq!(commit["type"], "input_audio_buffer.commit");
        let (code, reason) = h.obs_close(&obs);
        assert_eq!((code, reason.as_str()), (1000, "Client stopped"));
        assert_eq!(h.status(), "ASR 已停止");
    }

    /// 重连后：断线退避期间 feed 的陈旧音频必须丢弃（不得补发给新连接——
    /// 02 实测"用户句+AI 口播回声拼成一条转写再来一遍"的根源）；重连就绪
    /// 后新 feed 正常发送。
    #[test]
    fn stale_feed_dropped_after_reconnect() {
        let scripts = vec![
            {
                let mut s = ready_script();
                s.push(Step::Send(r#"{"type":"error","retryable":true}"#.into()));
                s
            },
            {
                let mut s = ready_script();
                s.push(Step::SleepMs(300)); // 观测窗口：多余的陈旧 append 也会被记录
                s.push(Step::ExpectClose);
                s
            },
        ];
        let (url, obs) = spawn_stub(scripts);
        let h = make_client(&url, 3);
        // 退避拉长到 400ms，制造"断线期间 feed 陈旧音频"的窗口
        h.client
            .set_sleeper_for_test(Box::new(|_| thread::sleep(Duration::from_millis(400))));
        h.client.start();

        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs); // auth
        let _ = h.obs_json(&obs); // session.update
        assert_eq!(h.status(), "ASR 实时识别就绪");
        assert_eq!(h.status(), "ASR 服务忙，重连");
        assert_eq!(h.status(), "ASR 1s 后重连（1/3）");

        // 退避 400ms 窗口内 feed 陈旧音频（此刻无连接，必然早于下个连接）
        h.client.feed(vec![0xAA; 3200]);
        h.client.feed(vec![0xBB; 3200]);

        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs); // auth
        let _ = h.obs_json(&obs); // session.update
        assert_eq!(h.status(), "ASR 实时识别就绪");

        // 就绪后 feed 新音频：只有它应被发送（陈旧的已丢弃）
        h.client.feed(vec![0xCC; 3200]);
        let frame = h.obs_json(&obs);
        assert_eq!(frame["type"], "input_audio_buffer.append");
        assert_eq!(
            decode_b64(frame["audio"].as_str().unwrap()),
            vec![0xCC; 3200]
        );

        h.client.stop();
        let commit = h.obs_json(&obs);
        assert_eq!(commit["type"], "input_audio_buffer.commit");
        let (code, reason) = h.obs_close(&obs);
        assert_eq!((code, reason.as_str()), (1000, "Client stopped"));
        assert_eq!(h.status(), "ASR 已停止");

        // 陈旧 0xAA/0xBB 全程不得补发：obs 里不应再有 append 帧
        thread::sleep(Duration::from_millis(200));
        if let Ok(o) = obs.try_recv() {
            panic!("陈旧音频被补发: {o:?}");
        }
    }

    /// 连不上：退避 1/2/4 后耗尽 → on_text("", true) 收束 + 字幕停用状态。
    #[test]
    fn backoff_sequence_and_exhaust() {
        // 找一个没有监听的端口
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let url = format!("ws://127.0.0.1:{}/", port);

        let h = make_client(&url, 3);
        h.client.start();

        // 连接失败不发“ASR 连接中”（对齐 Python：status 在 open 成功后才发）
        assert_eq!(h.status(), "ASR 1s 后重连（1/3）");
        assert_eq!(h.status(), "ASR 2s 后重连（2/3）");
        assert_eq!(h.status(), "ASR 4s 后重连（3/3）");
        assert_eq!(h.status(), "ASR 重连次数用尽，字幕停用");
        assert_eq!(h.text(), (String::new(), true));
        h.wait_thread_exit();
        assert!(!h.client.running());

        // 退避序列精确 1s/2s/4s
        assert_eq!(
            *h.sleeps.lock().unwrap(),
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4)
            ]
        );

        // 耗尽后 stop() 仍回“ASR 已停止”（closed 尚未置位，Python 语义）
        h.client.stop();
        assert_eq!(h.status(), "ASR 已停止");
    }

    /// 退避上限 8s：max_reconnects=6 时序列 1/2/4/8/8/8。
    #[test]
    fn backoff_caps_at_eight_seconds() {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let url = format!("ws://127.0.0.1:{}/", port);

        let h = make_client(&url, 6);
        h.client.start();
        for (n, expect) in [1, 2, 4, 8, 8, 8].into_iter().enumerate() {
            assert_eq!(h.status(), format!("ASR {expect}s 后重连（{}/6）", n + 1));
        }
        assert_eq!(h.status(), "ASR 重连次数用尽，字幕停用");
        assert_eq!(
            *h.sleeps.lock().unwrap(),
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(8),
                Duration::from_secs(8)
            ]
        );
    }

    /// update_endpoint：同 URL 无动作；换 URL 关闭旧连接（1000/"endpoint
    /// changed"）并重连到新端点。
    #[test]
    fn update_endpoint_switches_url() {
        let (url_a, obs_a) = spawn_stub(vec![{
            let mut s = ready_script();
            s.push(Step::ExpectClose); // 被踢
            s
        }]);
        let (url_b, obs_b) = spawn_stub(vec![{
            let mut s = ready_script();
            s.push(Step::SleepMs(200));
            s.push(Step::ExpectClose); // stop()
            s
        }]);

        let h = make_client(&url_a, 3);
        h.client.start();
        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs_a);
        let _ = h.obs_json(&obs_a);
        assert_eq!(h.status(), "ASR 实时识别就绪");

        // 同 URL：不触发任何动作
        h.client.update_endpoint(url_a.clone());
        thread::sleep(Duration::from_millis(200));
        assert!(obs_a.try_recv().is_err());

        // 换 URL：旧连接被关闭
        h.client.update_endpoint(url_b.clone());
        let (code, reason) = h.obs_close(&obs_a);
        assert_eq!((code, reason.as_str()), (1000, "endpoint changed"));
        assert_eq!(h.status(), "ASR 1s 后重连（1/3）");

        // 新端点重建并就绪
        let _ = h.obs_json(&obs_b);
        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs_b);
        assert_eq!(h.status(), "ASR 实时识别就绪");

        h.client.stop();
        let commit = h.obs_json(&obs_b);
        assert_eq!(commit["type"], "input_audio_buffer.commit");
        let (code, reason) = h.obs_close(&obs_b);
        assert_eq!((code, reason.as_str()), (1000, "Client stopped"));
        assert_eq!(h.status(), "ASR 已停止");
    }

    /// 未就绪时 stop()：不发 commit，直接 close(1000)。
    #[test]
    fn stop_before_ready_skips_commit() {
        let mut script = handshake_script();
        script.push(Step::SleepMs(400)); // 停在未就绪窗口
        script.push(Step::ExpectClose);
        let (url, obs) = spawn_stub(vec![script]);

        let h = make_client(&url, 3);
        h.client.start();
        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs); // auth
        let _ = h.obs_json(&obs); // session.update
        h.client.feed(b"pending".to_vec()); // 未 ready：只入缓冲

        h.client.stop();
        // 未就绪：没有 commit 帧，直接关闭
        let (code, reason) = h.obs_close(&obs);
        assert_eq!((code, reason.as_str()), (1000, "Client stopped"));
        assert_eq!(h.status(), "ASR 已停止");
        assert!(obs.try_recv().is_err());
    }

    /// 启动前的 feed 被丢弃（Python `_loop is None`）。
    #[test]
    fn feed_before_start_is_dropped() {
        let mut script = ready_script();
        script.push(Step::SleepMs(300));
        script.push(Step::ExpectClose);
        let (url, obs) = spawn_stub(vec![script]);

        let h = make_client(&url, 3);
        h.client.feed(b"dropped".to_vec()); // 未 start
        h.client.start();
        assert_eq!(h.status(), "ASR 连接中");
        let _ = h.obs_json(&obs); // auth
        let _ = h.obs_json(&obs); // session.update
        assert_eq!(h.status(), "ASR 实时识别就绪");

        // 就绪后没有 append flush（缓冲为空）
        thread::sleep(Duration::from_millis(350));
        assert!(obs.try_recv().is_err());

        // 就绪态停止：仍有 commit + close
        h.client.stop();
        let commit = h.obs_json(&obs);
        assert_eq!(commit["type"], "input_audio_buffer.commit");
        let (code, reason) = h.obs_close(&obs);
        assert_eq!((code, reason.as_str()), (1000, "Client stopped"));
        assert_eq!(h.status(), "ASR 已停止");
    }

    /// URL 解析：scheme/端口缺省/IPv6/非法输入。
    #[test]
    fn parse_endpoint_cases() {
        let u = parse_endpoint("wss://tts-center.ea.modelbest.co/api/v1/asr/realtime").unwrap();
        assert!(u.secure);
        assert_eq!(u.host, "tts-center.ea.modelbest.co");
        assert_eq!(u.port, 443);
        assert_eq!(
            u.request,
            "wss://tts-center.ea.modelbest.co/api/v1/asr/realtime"
        );

        let u = parse_endpoint("ws://127.0.0.1:9000/x?y=1").unwrap();
        assert!(!u.secure);
        assert_eq!((u.host.as_str(), u.port), ("127.0.0.1", 9000));
        assert_eq!(u.request, "ws://127.0.0.1:9000/x?y=1");

        let u = parse_endpoint("wss://user:pw@[::1]:8443/p").unwrap();
        assert!(u.secure);
        assert_eq!(u.host, "::1");
        assert_eq!(u.port, 8443);

        let u = parse_endpoint("HTTP://Example.com").unwrap(); // 大小写宽容
        assert!(!u.secure);
        assert_eq!(u.port, 80);

        assert!(parse_endpoint("").is_err());
        assert!(parse_endpoint("tts-center.ea.modelbest.co").is_err()); // 无 scheme
        assert!(parse_endpoint("ftp://x/").is_err());
        assert!(parse_endpoint("ws://:9000/").is_err()); // host 空
        assert!(parse_endpoint("ws://h:notaport/").is_err());
    }

    /// o5-config 字段接线：默认端点可解析，类型对得上。
    #[test]
    fn config_wiring() {
        let cfg = o5_config::Config::defaults();
        assert!(parse_endpoint(&cfg.asr_endpoint).is_ok());
        let _ = AsrClient::new(
            cfg.asr_endpoint.clone(),
            cfg.asr_api_key.clone(),
            cfg.asr_service_id,
            cfg.asr_max_reconnects,
            true,
            Box::new(|_, _| {}),
            Box::new(|_| {}),
        );
    }
}

//! PC 集成测试：O5Session 对着内嵌的最小 WS 桩服务器跑通全链路
//!（桩行为脚本化自 Python `tools/stub_server.py`）。
//!
//! 覆盖：
//! 1. queue gate（queued/queue_update/queue_done）-> init -> created -> ready 状态机
//! 2. 1s unit 上行（v4 audio_base64 / v3 audio 字段）、committed ACK
//!    （无 input_id 的 v4 映射 + 带 input_id 直配两条路）
//! 3. 冷启动 30 -> 预热 2 的在途窗口翻转与 ACK 释放
//! 4. omni 模式 video_frames 恰好一帧绑定（黑帧占位 / set_video_frame 槽）
//! 5. delegate_task 工具调用拼装 -> tool_result 回填 -> 口播转述 + 24k 音频
//! 6. force_listen 注入（interrupt 只挂下一个 unit）
//! 7. 优雅关闭（session.close -> session.closed -> closed；5s 超时强断；v3 直断）
//! 8. 超时触发 failed（progress 超时 / ack 超时，小超时验证）
//! 9. 服务端 error 帧失败路径

// accept_hdr 的握手回调签名由 tungstenite 定死（Err = ErrorResponse，体积
// 大于阈值），无法按 clippy 建议装箱，这里豁免 result_large_err。
#![allow(clippy::result_large_err)]

use std::net::TcpListener;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use crossbeam_channel::{Receiver, Sender};
use o5_config::Config;
use o5_perf::Timing;
use o5_protocol::{ParsedToolCall, BLACK_JPEG_B64, UNIT_BYTES};
use o5_session::{O5Session, SessionListener, SessionState};
use serde_json::{json, Value};
use tungstenite::handshake::server::Request;
use tungstenite::{accept_hdr, Message};

/// 测试日志（stderr；O5_TEST_LOG=1 时启用）。
struct PrintLogger;

impl log::Log for PrintLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        eprintln!("[{}] {}", r.level(), r.args());
    }
    fn flush(&self) {}
}

fn init_logger() {
    static ONCE: OnceLock<()> = OnceLock::new();
    static LOGGER: PrintLogger = PrintLogger;
    if ONCE.set(()).is_ok() && std::env::var("O5_TEST_LOG").is_ok() {
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(log::LevelFilter::Debug);
    }
}

// ---------------------------------------------------------------------------
// 最小 WS 桩（脚本化 stub_server.py）
// ---------------------------------------------------------------------------

enum StubCmd {
    Send(String),
    SetAutoAck(bool),
    /// 收到 session.close 时自动回 session.closed 并关 WS（stub_server.py
    /// 第 137-140 行的行为；优雅关闭链路的默认桩行为）。
    SetAutoClose(bool),
    CloseWs,
}

enum StubEvt {
    Connected,
    Text(String),
    WsClosed,
}

struct Stub {
    port: u16,
    evt: Receiver<StubEvt>,
    cmd: Sender<StubCmd>,
    auth: Arc<Mutex<Option<String>>>,
    path: Arc<Mutex<Option<String>>>,
    #[allow(dead_code)]
    handle: std::thread::JoinHandle<()>,
}

impl Drop for Stub {
    fn drop(&mut self) {
        // cmd 通道断开 -> 桩线程退出（连接层面各自超时/关闭）
        let _ = self.cmd.send(StubCmd::CloseWs);
    }
}

impl Stub {
    fn spawn() -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定 127.0.0.1:0 失败");
        let port = listener.local_addr().expect("读取端口失败").port();
        let (evt_tx, evt_rx) = crossbeam_channel::unbounded();
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let auth: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let path: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let a2 = auth.clone();
        let p2 = path.clone();
        let handle = std::thread::Builder::new()
            .name("stub-ws".into())
            .spawn(move || {
                // 循环 accept：非升级连接（如 protocolVersion=auto 的 /v1/info
                // HTTP 探测）握手失败就丢弃续接，等真正的 WS 客户端
                let mut ws = loop {
                    let (stream, _) = match listener.accept() {
                        Ok(x) => x,
                        Err(e) => {
                            eprintln!("[stub] TCP accept 失败: {e}");
                            return;
                        }
                    };
                    let _ = stream.set_nodelay(true);
                    let a3 = a2.clone();
                    let p3 = p2.clone();
                    match accept_hdr(stream, move |req: &Request, resp| {
                        *a3.lock().unwrap() = req
                            .headers()
                            .get("Authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        *p3.lock().unwrap() =
                            req.uri().path_and_query().map(|p| p.as_str().to_string());
                        Ok(resp)
                    }) {
                        Ok(ws) => break ws,
                        Err(e) => {
                            eprintln!("[stub] 非升级连接丢弃（{e}），续接 accept");
                        }
                    }
                };
                // 读超时在握手完成后再启用（握手必须阻塞等完）
                let _ = ws
                    .get_ref()
                    .set_read_timeout(Some(Duration::from_millis(20)));
                let _ = evt_tx.send(StubEvt::Connected);
                let mut auto_ack = false;
                let mut auto_close = false;
                let mut unit_index: i64 = 0;
                loop {
                    // 命令驱力（测试线程脚本化下发）
                    loop {
                        match cmd_rx.try_recv() {
                            Ok(StubCmd::Send(text)) => {
                                if ws.send(Message::Text(text.as_str().into())).is_err() {
                                    let _ = evt_tx.send(StubEvt::WsClosed);
                                    return;
                                }
                            }
                            Ok(StubCmd::SetAutoAck(b)) => auto_ack = b,
                            Ok(StubCmd::SetAutoClose(b)) => auto_close = b,
                            Ok(StubCmd::CloseWs) => {
                                let _ = ws.close(None);
                            }
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => return,
                        }
                    }
                    match ws.read() {
                        Ok(Message::Text(t)) => {
                            let _ = evt_tx.send(StubEvt::Text(t.to_string()));
                            if auto_close {
                                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                                    if v["type"] == "session.close" {
                                        // 对齐 stub_server.py：回 session.closed 后主动关 WS
                                        let _ = ws.send(Message::Text(
                                            json!({
                                                "type": "session.closed",
                                                "reason": "client_closed",
                                            })
                                            .to_string()
                                            .into(),
                                        ));
                                        let _ = ws.close(None);
                                    }
                                }
                            }
                            if auto_ack {
                                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                                    if v["type"] == "input.append" {
                                        let input_id = v["input"]["input_id"]
                                            .as_str()
                                            .unwrap_or("")
                                            .to_string();
                                        let idx = unit_index;
                                        unit_index += 1;
                                        // started 带 input_id，committed 故意不带
                                        //（v4 真机行为，强制客户端走 unit_index 映射）
                                        let _ = ws.send(Message::Text(
                                            json!({
                                                "type": "response.unit.started",
                                                "input_id": input_id,
                                                "unit_index": idx,
                                            })
                                            .to_string()
                                            .into(),
                                        ));
                                        let _ = ws.send(Message::Text(
                                            json!({
                                                "type": "response.unit.committed",
                                                "unit_index": idx,
                                            })
                                            .to_string()
                                            .into(),
                                        ));
                                    }
                                }
                            }
                        }
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => {}
                        Ok(Message::Close(_)) => {
                            let _ = evt_tx.send(StubEvt::WsClosed);
                            return;
                        }
                        Ok(Message::Frame(_)) => {}
                        Err(e) if is_idle(&e) => {}
                        Err(e) => {
                            eprintln!("[stub] WS 读失败: {e}");
                            let _ = evt_tx.send(StubEvt::WsClosed);
                            return;
                        }
                    }
                }
            })
            .expect("启动桩线程失败");
        Stub {
            port,
            evt: evt_rx,
            cmd: cmd_tx,
            auth,
            path,
            handle,
        }
    }

    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }

    fn send_json(&self, v: Value) {
        let _ = self.cmd.send(StubCmd::Send(v.to_string()));
    }

    fn set_auto_ack(&self, on: bool) {
        let _ = self.cmd.send(StubCmd::SetAutoAck(on));
    }

    fn set_auto_close(&self, on: bool) {
        let _ = self.cmd.send(StubCmd::SetAutoClose(on));
    }

    fn auth(&self) -> Option<String> {
        self.auth.lock().unwrap().clone()
    }

    fn path(&self) -> Option<String> {
        self.path.lock().unwrap().clone()
    }

    /// 等连接建立。
    fn wait_connected(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            match self.evt.recv_timeout(deadline - now) {
                Ok(StubEvt::Connected) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }

    /// 等下一条满足谓词的 JSON 帧（跳过其他帧；连接关闭则提前失败）。
    fn wait_json(&self, timeout: Duration, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            match self.evt.recv_timeout(deadline - now) {
                Ok(StubEvt::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        if pred(&v) {
                            return Some(v);
                        }
                    }
                }
                Ok(StubEvt::Connected) => continue,
                Ok(StubEvt::WsClosed) => return None,
                Err(_) => return None,
            }
        }
    }

    /// 收集直到静默 quiet 时长（观察「到此刻为止到达了什么」）。
    fn drain_json(&self, quiet: Duration) -> Vec<Value> {
        let mut out = Vec::new();
        loop {
            match self.evt.recv_timeout(quiet) {
                Ok(StubEvt::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        out.push(v);
                    }
                }
                Ok(_) => {}
                Err(_) => return out,
            }
        }
    }
}

fn is_idle(e: &tungstenite::Error) -> bool {
    matches!(
        e,
        tungstenite::Error::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
    )
}

// ---------------------------------------------------------------------------
// 测试辅助
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Collector {
    states: Mutex<Vec<SessionState>>,
    queue_positions: Mutex<Vec<i64>>,
    audio: Mutex<Vec<(Vec<u8>, u32)>>,
    spoken: Mutex<Vec<(String, bool)>>,
    think: Mutex<Vec<String>>,
    tools: Mutex<Vec<ParsedToolCall>>,
    aborts: Mutex<Vec<String>>,
    errors: Mutex<Vec<(String, String)>>,
    closed: Mutex<Vec<String>>,
    /// on_interrupted 触发次数（让位打断）。
    interrupted: Mutex<u32>,
}

impl SessionListener for Collector {
    fn on_state(&self, s: SessionState) {
        self.states.lock().unwrap().push(s);
    }
    fn on_queue_position(&self, pos: i64) {
        self.queue_positions.lock().unwrap().push(pos);
    }
    fn on_audio(&self, pcm16: Vec<u8>, rate: u32) {
        self.audio.lock().unwrap().push((pcm16, rate));
    }
    fn on_spoken_text(&self, text: &str, final_: bool) {
        self.spoken.lock().unwrap().push((text.to_string(), final_));
    }
    fn on_think_text(&self, text: &str) {
        self.think.lock().unwrap().push(text.to_string());
    }
    fn on_tool_call(&self, call: &ParsedToolCall) {
        self.tools.lock().unwrap().push(call.clone());
    }
    fn on_tool_abort(&self, tool_call_id: &str) {
        self.aborts.lock().unwrap().push(tool_call_id.to_string());
    }
    fn on_error(&self, code: &str, message: &str) {
        self.errors
            .lock()
            .unwrap()
            .push((code.to_string(), message.to_string()));
    }
    fn on_closed(&self, reason: &str) {
        self.closed.lock().unwrap().push(reason.to_string());
    }
    fn on_interrupted(&self) {
        *self.interrupted.lock().unwrap() += 1;
    }
}

fn test_cfg(url: &str, version: u8, mode: &str) -> Config {
    let mut cfg = Config::defaults();
    cfg.fg_endpoint = url.to_string();
    cfg.fg_protocol_version = version;
    cfg.fg_mode = mode.to_string();
    cfg.fg_queue_gate = true;
    cfg.fg_progress_timeout_s = 30.0;
    cfg.fg_ack_timeout_s = 30.0;
    cfg.fg_connect_timeout_s = 5.0;
    cfg
}

/// 1 秒静音 unit。
fn silence() -> Vec<u8> {
    vec![0u8; UNIT_BYTES]
}

/// 24kHz float32 LE 正弦波 b64（模拟下行 TTS 音频）。
fn sine_b64(freq: f32, seconds: f32) -> String {
    let rate = 24000.0f32;
    let n = (rate * seconds) as usize;
    let mut raw = Vec::with_capacity(n * 4);
    for i in 0..n {
        let t = i as f32 / rate;
        let v = 0.2 * (2.0 * std::f32::consts::PI * freq * t).sin();
        raw.extend_from_slice(&v.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(raw)
}

fn wait_state(sess: &O5Session, want: SessionState, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if sess.state() == want {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    sess.state() == want
}

fn wait_until<T>(timeout: Duration, f: impl Fn() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(v) = f() {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    f()
}

/// 走到 ready（queue gate + created）。
fn drive_to_ready(stub: &Stub) {
    stub.send_json(json!({"type": "session.queued", "position": 3}));
    stub.send_json(json!({"type": "session.queue_update", "position": 2}));
    stub.send_json(json!({"type": "session.queue_done"}));
}

// ---------------------------------------------------------------------------
// 测试用例
// ---------------------------------------------------------------------------

/// v4 音频模式全链路：queue gate -> init -> created -> ready -> unit 上行 ->
/// committed 映射 -> tool_call 拼装 -> tool_result -> 口播音频 + turn_eos ->
/// 优雅关闭到 closed（对齐 test_session_pc.py 的 audio 场景）。
#[test]
fn full_flow_v4_audio() {
    init_logger();
    let stub = Stub::spawn();
    let cfg = test_cfg(&stub.url(), 4, "audio");
    let collector = Arc::new(Collector::default());
    let timing = Timing::new();
    let tools = json!([{
        "type": "function",
        "function": {"name": "delegate_task", "parameters": {"type": "object"}}
    }]);
    let sess = O5Session::new(&cfg, collector.clone(), Some(timing.clone()), Some(tools));

    // ready 之前（未 start / 非就绪）投递的音频直接丢弃
    sess.offer_unit(silence(), None, Instant::now());
    sess.start();
    assert!(stub.wait_connected(Duration::from_secs(3)), "客户端未连接");

    // queue gate：queued / queue_update -> waiting，queue_done -> init
    drive_to_ready(&stub);
    assert!(wait_state(
        &sess,
        SessionState::Initializing,
        Duration::from_secs(3)
    ));
    let init = stub
        .wait_json(Duration::from_secs(3), |v| v["type"] == "session.init")
        .expect("未收到 session.init");
    assert_eq!(init["payload"]["protocol_version"], "4");
    assert_eq!(init["payload"]["mode"], "full_duplex");
    assert_eq!(init["payload"]["fc_duplex"], true);
    assert_eq!(init["payload"]["generate_audio"], true);
    assert_eq!(
        init["payload"]["config"]["decoding_policy"],
        "constrained_decoding"
    );
    assert_eq!(
        init["payload"]["system"]["tools"][0]["function"]["name"],
        "delegate_task"
    );
    // 未配置音色：无 tts_prompt_audio；task 段在 segments 末尾
    assert!(init["payload"].get("tts_prompt_audio").is_none());
    assert_eq!(
        init["payload"]["system"]["segments"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["text"],
        Value::from(cfg.fg_task_prompt.as_str())
    );

    stub.send_json(json!({
        "type": "session.created",
        "session_id": "stub-session-1",
        "resume": {"tts_prompt_audio_sha256": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"},
    }));
    assert!(wait_state(
        &sess,
        SessionState::Ready,
        Duration::from_secs(3)
    ));

    // 想在 Initializing 中途投的音频也被丢弃（上面已投过一次，此处再验一次门控）
    // —— 喂 5 个 unit（静默 1s 节奏的加速版），桩自动 started/committed
    stub.set_auto_ack(true);
    let mut arrived = Vec::new();
    for i in 0..5 {
        // 第 2 个 unit 前注入 force_listen（桩上会回显出来核对）
        if i == 1 {
            sess.interrupt();
        }
        sess.offer_unit(silence(), None, timing.unit_recorded());
        // 等这个 unit 到达桩（保持顺序断言的确定性）
        let want = format!("unit_{:03}", i + 1);
        let got = stub.wait_json(Duration::from_secs(3), |v| {
            v["type"] == "input.append" && v["input"]["input_id"] == want.as_str()
        });
        assert!(got.is_some(), "未等到 {want}");
        arrived.push(got.unwrap());
    }
    // v4 字段：audio_base64 + sample_rate；audio 模式绝不带 video_frames
    for (i, frame) in arrived.iter().enumerate() {
        let entry = &frame["input"];
        assert!(entry["audio_base64"].is_string(), "v4 应带 audio_base64");
        assert_eq!(entry["sample_rate"], 16000);
        assert!(entry.get("audio").is_none(), "v4 不用 audio 字段");
        assert!(
            entry.get("video_frames").is_none(),
            "audio 模式禁带 video_frames"
        );
        // force_listen 只出现在第 2 个 unit
        if i == 1 {
            assert_eq!(
                entry["force_listen"], true,
                "interrupt 后的 unit 应带 force_listen"
            );
        } else {
            assert!(
                entry.get("force_listen").is_none(),
                "其余 unit 不应带 force_listen"
            );
        }
    }

    // 第 2 个 unit 后桩下发 delegate_task（begin/delta/done，done 缺 arguments）
    stub.send_json(json!({
        "type": "response.tool_call.begin", "tool_call_id": "tc-stub-1", "unit_index": 1,
    }));
    stub.send_json(json!({
        "type": "response.tool_call.delta", "tool_call_id": "tc-stub-1",
        "steps": [{"kind": "text", "text": "{\"message\": "},
                  {"kind": "text", "text": "\"帮我算 23 乘 47\"}"}],
    }));
    stub.send_json(json!({
        "type": "response.tool_call.done", "tool_call_id": "tc-stub-1",
        "call": {"name": "delegate_task"},
    }));
    let call = wait_until(Duration::from_secs(3), || {
        collector.tools.lock().unwrap().first().cloned()
    })
    .expect("未收到 tool_call");
    // done 缺 arguments 时用 delta 缓冲拼装
    assert_eq!(call.id, "tc-stub-1");
    assert_eq!(call.name, "delegate_task");
    assert_eq!(call.arguments, "{\"message\": \"帮我算 23 乘 47\"}");

    // 回填 tool_result（v4 原子：content 字符串）
    sess.send_tool_result("tc-stub-1", "23 乘 47 等于 1081");
    let tr = stub
        .wait_json(Duration::from_secs(3), |v| v["type"] == "input.tool_result")
        .expect("未收到 tool_result");
    assert_eq!(tr["tool_call_id"], "tc-stub-1");
    assert_eq!(tr["content"], "23 乘 47 等于 1081");

    // think 流
    stub.send_json(json!({"type": "response.think.begin"}));
    stub.send_json(json!({"type": "response.think.delta", "text": "思考中"}));
    stub.send_json(json!({"type": "response.think.end"}));
    assert_eq!(
        wait_until(Duration::from_secs(3), || {
            collector.think.lock().unwrap().first().cloned()
        }),
        Some("思考中".to_string())
    );

    // 口播转述 + 24k 音频 + turn_eos（对齐 stub_server.py 第 4/5 unit 行为）
    stub.send_json(json!({
        "type": "response.spoken.delta", "unit_index": 4,
        "steps": [{"kind": "text", "text": "后台的结果是23 乘 47 等于 1081。"}],
        "audio": sine_b64(660.0, 0.3), "sample_rate": 24000,
    }));
    stub.send_json(json!({
        "type": "response.spoken.end", "unit_index": 4, "reason": "turn_eos",
    }));
    // slot_eos 不关回合
    stub.send_json(json!({
        "type": "response.spoken.end", "unit_index": 4, "reason": "slot_eos",
    }));

    let (pcm, rate) = wait_until(Duration::from_secs(3), || {
        collector.audio.lock().unwrap().first().cloned()
    })
    .expect("未收到音频");
    assert_eq!(rate, 24000);
    assert_eq!(pcm.len(), 7200 * 2, "0.3s @24k 的 pcm16 应为 14400 字节");
    {
        let spoken = collector.spoken.lock().unwrap();
        let last = spoken.last().unwrap().clone();
        assert_eq!(last, (String::new(), true), "turn_eos 应触发 final");
        assert!(
            spoken.iter().any(|(t, f)| !*f && t.contains("1081")),
            "口播文本应包含结果，got {spoken:?}"
        );
    }

    // 优雅关闭：session.close -> session.closed(client_closed) -> closed。
    // stop() 会 join reader 直到 closed/failed（Python 的 Thread.join 语义），
    // 服务端回包必须由桩自动完成（对齐 stub_server.py 的自动回 session.closed）。
    stub.set_auto_close(true);
    let t0 = Instant::now();
    sess.stop();
    let close_frame = stub
        .wait_json(Duration::from_secs(3), |v| v["type"] == "session.close")
        .expect("未收到 session.close");
    assert_eq!(close_frame["type"], "session.close");
    assert!(wait_state(
        &sess,
        SessionState::Closed,
        Duration::from_secs(3)
    ));
    assert!(t0.elapsed() < Duration::from_secs(5), "优雅关闭不应超过 5s");
    {
        let closed = collector.closed.lock().unwrap();
        assert_eq!(closed.last().unwrap(), "client_closed");
    }

    // 状态机序列（queue_update 也会重复 Waiting，照 Python 的逐帧回调）
    let states = collector.states.lock().unwrap().clone();
    let expected = vec![
        SessionState::Connecting,
        SessionState::Waiting,
        SessionState::Waiting,
        SessionState::Initializing,
        SessionState::Ready,
        SessionState::Closing,
        SessionState::Closed,
    ];
    assert_eq!(states, expected, "状态机序列不符");
    assert_eq!(collector.queue_positions.lock().unwrap().as_slice(), &[2]);

    // 时序口径：units_sent / ack RTT / 回合计数
    let snap = timing.snapshot();
    assert!(snap.units_sent >= 5, "units_sent={}", snap.units_sent);
    assert!(snap.ack_rtt.is_some(), "应有 committed ACK RTT");
    assert_eq!(snap.turns, 1);
    assert!(snap.audio_chunks >= 1);
    drop(stub);
}

/// 冷启动 30 -> 预热 2 的在途窗口（含 committed 带 input_id 的直配路径）。
/// 打断/收束语义（cpm-harness interaction-bridge 口径，逐一回归真机事故）：
/// - turn_eos：回合正常结束，**不清**播放（积压自然排空）
/// - listen && 口播中：让位（被插话）→ Interrupted（清播放积压快停）
/// - listen && 空闲：每 unit 常规收据，忽略（曾每秒误清 →「说一半就没了」）
/// - slot_eos / tts_pad：unit 内收束，口播延续（v5 口播中每 unit 一条，
///   曾误判回合结束 → 每秒误清 →「重复一句话」+ aplay 重建爆音）
#[test]
fn spoken_end_interruption_semantics() {
    init_logger();
    let stub = Stub::spawn();
    let cfg = test_cfg(&stub.url(), 4, "audio");
    let collector = Arc::new(Collector::default());
    let sess = O5Session::new(&cfg, collector.clone(), None, None);
    sess.start();
    assert!(stub.wait_connected(Duration::from_secs(3)));
    drive_to_ready(&stub);
    stub.send_json(json!({
        "type": "session.created", "session_id": "s",
        "resume": {"tts_prompt_audio_sha256": "x"},
    }));
    assert!(wait_state(
        &sess,
        SessionState::Ready,
        Duration::from_secs(3)
    ));
    let interrupted = || *collector.interrupted.lock().unwrap();

    // 空闲期 listen 收据（每 unit 一条）：忽略
    for ui in 1..=3 {
        stub.send_json(json!({
            "type": "response.spoken.end", "unit_index": ui, "reason": "listen",
        }));
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(interrupted(), 0, "空闲 listen 是常规收据，不能当打断");

    // 回合 A 开口（口播中）
    stub.send_json(json!({
        "type": "response.spoken.delta", "unit_index": 4,
        "steps": [{"kind": "text", "text": "回合A"}],
    }));
    assert_eq!(
        wait_until(Duration::from_secs(3), || {
            collector.spoken.lock().unwrap().first().cloned()
        }),
        Some(("回合A".to_string(), false)),
        "delta 文本应上屏"
    );

    // 口播中的 slot_eos（v5 透明池每 unit 一条）：unit 内收束，忽略
    stub.send_json(json!({
        "type": "response.spoken.end", "unit_index": 4, "reason": "slot_eos",
    }));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(interrupted(), 0, "slot_eos 是 unit 内收束，不是回合结束");

    // 让位：口播中没有 turn_eos 就转 listen -> Interrupted（清播放积压）
    stub.send_json(json!({
        "type": "response.spoken.end", "unit_index": 5, "reason": "listen",
    }));
    assert_eq!(
        wait_until(Duration::from_secs(3), || {
            let n = interrupted();
            (n >= 1).then_some(n)
        }),
        Some(1),
        "口播中转 listen = 让位打断，应触发 Interrupted"
    );

    // 让位后的空闲 listen（speaking 已复位）：回到常规收据，忽略
    stub.send_json(json!({
        "type": "response.spoken.end", "unit_index": 6, "reason": "listen",
    }));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(interrupted(), 1, "让位后的 listen 收据不应再触发");

    // 回合 B 正常说完（turn_eos）：不清播放（无 Interrupted），final 口播
    stub.send_json(json!({
        "type": "response.spoken.delta", "unit_index": 7,
        "steps": [{"kind": "text", "text": "回合B"}],
    }));
    std::thread::sleep(Duration::from_millis(200));
    stub.send_json(json!({
        "type": "response.spoken.end", "unit_index": 7, "reason": "turn_eos",
    }));
    assert_eq!(
        wait_until(Duration::from_secs(3), || {
            collector
                .spoken
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(_, f)| *f)
                .cloned()
        }),
        Some((String::new(), true)),
        "turn_eos 应触发 final 口播"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        interrupted(),
        1,
        "turn_eos 是正常收束，不应触发打断清空"
    );

    // 空闲期再来 listen：仍忽略
    stub.send_json(json!({
        "type": "response.spoken.end", "unit_index": 8, "reason": "listen",
    }));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(interrupted(), 1);

    sess.stop();
    drop(stub);
}

#[test]
fn inflight_window_cold_to_warm() {
    let stub = Stub::spawn();
    let cfg = test_cfg(&stub.url(), 4, "audio");
    let collector = Arc::new(Collector::default());
    let sess = O5Session::new(&cfg, collector.clone(), None, None);
    sess.start();
    assert!(stub.wait_connected(Duration::from_secs(3)));
    drive_to_ready(&stub);
    stub.send_json(json!({
        "type": "session.created", "session_id": "s",
        "resume": {"tts_prompt_audio_sha256": "x"},
    }));
    assert!(wait_state(
        &sess,
        SessionState::Ready,
        Duration::from_secs(3)
    ));

    // 冷启动：不 ACK 也应把 5 个 unit 全部发出去（限 30）
    for _ in 0..5 {
        sess.offer_unit(silence(), None, Instant::now());
        std::thread::sleep(Duration::from_millis(10));
    }
    let cold = stub.drain_json(Duration::from_millis(400));
    let cold_units: Vec<String> = cold
        .iter()
        .filter(|v| v["type"] == "input.append")
        .filter_map(|v| v["input"]["input_id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(cold_units.len(), 5, "冷启动应放行全部 5 个: {cold_units:?}");
    assert_eq!(cold_units[0], "unit_001");

    // ACK unit_002（committed 直接带 input_id 的路径）-> 首个 committed 翻转 warm
    stub.send_json(json!({
        "type": "response.unit.started", "input_id": "unit_002", "unit_index": 1,
    }));
    stub.send_json(json!({
        "type": "response.unit.committed", "input_id": "unit_002", "unit_index": 1,
    }));
    std::thread::sleep(Duration::from_millis(200));

    // warm（限 2）：再投 5 个，此刻在途 4 -> 一个都不该出去
    for _ in 0..5 {
        sess.offer_unit(silence(), None, Instant::now());
        std::thread::sleep(Duration::from_millis(10));
    }
    let blocked = stub.drain_json(Duration::from_millis(400));
    assert!(
        !blocked.iter().any(|v| v["type"] == "input.append"),
        "在途 4 时 warm 限 2 不应再发: {blocked:?}"
    );

    // ACK unit_003/004/005（在途 4 -> 1）-> 恰好再放 1 个出去（凑满 2）
    for idx in [3, 4, 5] {
        stub.send_json(json!({
            "type": "response.unit.started", "input_id": format!("unit_{idx:03}"),
            "unit_index": idx,
        }));
        stub.send_json(json!({
            "type": "response.unit.committed", "unit_index": idx,
        }));
    }
    let one = stub.drain_json(Duration::from_millis(400));
    let one_units: Vec<String> = one
        .iter()
        .filter(|v| v["type"] == "input.append")
        .filter_map(|v| v["input"]["input_id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(one_units, vec!["unit_006".to_string()], "只应放行 1 个");

    // ACK unit_001/unit_006（在途 -> 0，两次各放 1）-> 放行 2 个（unit_007/unit_008），剩余阻塞
    for idx in [1, 6] {
        stub.send_json(json!({
            "type": "response.unit.started", "input_id": format!("unit_{idx:03}"),
            "unit_index": idx,
        }));
        stub.send_json(json!({
            "type": "response.unit.committed", "unit_index": idx,
        }));
    }
    let two = stub.drain_json(Duration::from_millis(400));
    let two_units: Vec<String> = two
        .iter()
        .filter(|v| v["type"] == "input.append")
        .filter_map(|v| v["input"]["input_id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        two_units,
        vec!["unit_007".to_string(), "unit_008".to_string()]
    );

    // 全部 ACK 后剩余的也流出
    for idx in [7, 8] {
        stub.send_json(json!({
            "type": "response.unit.started", "input_id": format!("unit_{idx:03}"),
            "unit_index": idx,
        }));
        stub.send_json(json!({
            "type": "response.unit.committed", "unit_index": idx,
        }));
    }
    let rest = stub.drain_json(Duration::from_millis(400));
    let rest_units: Vec<String> = rest
        .iter()
        .filter(|v| v["type"] == "input.append")
        .filter_map(|v| v["input"]["input_id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        rest_units,
        vec!["unit_009".to_string(), "unit_010".to_string()]
    );

    stub.set_auto_close(true);
    sess.stop();
    drop(stub);
}

/// omni 模式：URL mode 参数替换 + 每 unit 恰好 1 帧 video_frames（黑帧/槽位）。
#[test]
fn omni_video_frame_slot() {
    let stub = Stub::spawn();
    // 端点自带 mode=video：客户端应替换成配置档位 omni
    let url = format!("ws://127.0.0.1:{}/e/test?mode=video&x=1", stub.port);
    let cfg = test_cfg(&url, 4, "omni");
    let collector = Arc::new(Collector::default());
    let sess = O5Session::new(&cfg, collector.clone(), None, None);
    sess.start();
    assert!(stub.wait_connected(Duration::from_secs(3)));
    assert_eq!(
        stub.path().as_deref(),
        Some("/e/test?x=1&mode=omni"),
        "mode 参数应被替换（追加在尾部的 Python urlencode 语义），其余透传"
    );
    drive_to_ready(&stub);
    stub.send_json(json!({"type": "session.created", "session_id": "s", "resume": {}}));
    assert!(wait_state(
        &sess,
        SessionState::Ready,
        Duration::from_secs(3)
    ));

    let next_append = |label: &str| -> Value {
        stub.wait_json(Duration::from_secs(3), |v| v["type"] == "input.append")
            .unwrap_or_else(|| panic!("未等到 unit（{label}）"))
    };

    // 无帧：黑帧占位
    sess.offer_unit(silence(), None, Instant::now());
    let f = next_append("black");
    assert_eq!(f["input"]["video_frames"], json!([BLACK_JPEG_B64]));

    // set_video_frame 槽位：后续 unit 带槽里的最新帧
    sess.set_video_frame("FRAME_FROM_SLOT".to_string());
    sess.offer_unit(silence(), None, Instant::now());
    let f = next_append("slot");
    assert_eq!(f["input"]["video_frames"], json!(["FRAME_FROM_SLOT"]));

    // offer_unit 直接带帧：本 unit 生效（槽不回写）
    sess.offer_unit(silence(), Some("FRAME_DIRECT".to_string()), Instant::now());
    let f = next_append("direct");
    assert_eq!(f["input"]["video_frames"], json!(["FRAME_DIRECT"]));

    // 槽位仍是旧值（per-unit 绑定，快照语义）
    sess.offer_unit(silence(), None, Instant::now());
    let f = next_append("slot-again");
    assert_eq!(f["input"]["video_frames"], json!(["FRAME_FROM_SLOT"]));

    stub.set_auto_close(true);
    sess.stop();
    drop(stub);
}

/// protocolVersion "auto"：/v1/info 探测失败（桩不是 HTTP 服务）→ 回退兜底 v4，
/// 会话照常建立、init 不带 v5 的 input_mode。
#[test]
fn protocol_auto_falls_back_when_probe_unreachable() {
    init_logger();
    let stub = Stub::spawn();
    let mut cfg = test_cfg(&stub.url(), 4, "audio");
    cfg.fg_protocol_auto = true; // JSON "auto" 解析结果
    let collector = Arc::new(Collector::default());
    let sess = O5Session::new(&cfg, collector.clone(), None, None);
    sess.start();
    assert!(stub.wait_connected(Duration::from_secs(3)));
    drive_to_ready(&stub);
    stub.send_json(json!({"type": "session.created", "session_id": "s", "resume": {}}));
    assert!(wait_state(
        &sess,
        SessionState::Ready,
        Duration::from_secs(5) // 首次探测多给 5s 预算
    ));
    {
        let init = stub
            .wait_json(Duration::from_secs(3), |v| v["type"] == "session.init")
            .expect("未收到 session.init");
        assert_eq!(init["payload"]["protocol_version"], "4");
        assert!(
            init["payload"].get("input_mode").is_none(),
            "v4 不应带 input_mode"
        );
    }
    sess.stop();
    drop(stub);
}

/// v3 老 dialect：audio 字段、超长 tool_result 的 delta/done 分片、stop 直断。
#[test]
fn v3_dialect_and_slices() {
    let stub = Stub::spawn();
    let cfg = test_cfg(&stub.url(), 3, "audio");
    let collector = Arc::new(Collector::default());
    let sess = O5Session::new(&cfg, collector.clone(), None, None);
    sess.start();
    assert!(stub.wait_connected(Duration::from_secs(3)));
    drive_to_ready(&stub);
    stub.send_json(json!({"type": "session.created", "session_id": "s", "resume": {}}));
    assert!(wait_state(
        &sess,
        SessionState::Ready,
        Duration::from_secs(3)
    ));
    {
        let init = stub
            .wait_json(Duration::from_secs(3), |v| v["type"] == "session.init")
            .expect("未收到 session.init");
        assert_eq!(init["payload"]["protocol_version"], "3");
    }

    // v3 input.append 用 audio 字段
    stub.set_auto_ack(true);
    sess.offer_unit(silence(), None, Instant::now());
    let f = stub
        .wait_json(Duration::from_secs(3), |v| v["type"] == "input.append")
        .expect("未收到 input.append");
    assert!(f["input"]["audio"].is_string(), "v3 应带 audio 字段");
    assert!(f["input"].get("audio_base64").is_none());
    assert!(f["input"].get("sample_rate").is_none());

    // 短结果：v3 content 数组形态
    sess.send_tool_result("tc-short", "短结果");
    let tr = stub
        .wait_json(Duration::from_secs(3), |v| v["type"] == "input.tool_result")
        .expect("未收到短 tool_result");
    assert_eq!(
        tr["content"],
        json!([{"kind": "text", "text": "短结果"}]),
        "v3 短结果应为 content 数组形态"
    );

    // 超长结果：delta/done 分片（1024B 字符边界切，拼回无损）
    let long_text: String = "很长的结果".repeat(800); // 12000B（15B x 800）> 2048
    sess.send_tool_result_raw("tc-long", &long_text);
    let mut slices: Vec<Value> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match stub.evt.recv_timeout(Duration::from_millis(500)) {
            Ok(StubEvt::Text(t)) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    let is_done = v["type"] == "input.tool_result.done";
                    let is_slice = v["type"]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("input.tool_result");
                    if is_slice {
                        slices.push(v);
                        if is_done {
                            break;
                        }
                    }
                }
            }
            _ => break,
        }
    }
    assert!(
        slices.iter().any(|v| v["type"] == "input.tool_result.done"),
        "分片序列应以 done 收尾: {slices:?}"
    );
    assert!(
        slices.len() >= 12,
        "12KB 应切出多个 delta: {}",
        slices.len()
    );
    let mut rebuilt = String::new();
    for s in &slices {
        if s["type"] == "input.tool_result.delta" {
            rebuilt.push_str(s["delta"].as_str().unwrap_or(""));
        }
    }
    assert_eq!(rebuilt, long_text, "分片拼回应无损");

    // v3 stop：不发 session.close，直接强断（快速到 closed）
    let t0 = Instant::now();
    sess.stop();
    assert!(wait_state(
        &sess,
        SessionState::Closed,
        Duration::from_secs(3)
    ));
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "v3 stop 应直断不等待 5s"
    );
    // 停止前桩不应见到 session.close
    let seen = stub.drain_json(Duration::from_millis(200));
    assert!(
        !seen.iter().any(|v| v["type"] == "session.close"),
        "v3 不应发送 session.close"
    );
    drop(stub);
}

/// v4 优雅关闭超时：服务端不回 session.closed -> 5s 强断 -> closed。
#[test]
fn graceful_close_timeout_forces_closed() {
    let stub = Stub::spawn();
    let cfg = test_cfg(&stub.url(), 4, "audio");
    let collector = Arc::new(Collector::default());
    let sess = O5Session::new(&cfg, collector.clone(), None, None);
    sess.start();
    assert!(stub.wait_connected(Duration::from_secs(3)));
    drive_to_ready(&stub);
    stub.send_json(json!({"type": "session.created", "session_id": "s", "resume": {}}));
    assert!(wait_state(
        &sess,
        SessionState::Ready,
        Duration::from_secs(3)
    ));

    let t0 = Instant::now();
    sess.stop();
    // 收到 session.close 但故意不回
    assert!(stub
        .wait_json(Duration::from_secs(3), |v| v["type"] == "session.close")
        .is_some());
    assert!(wait_state(
        &sess,
        SessionState::Closed,
        Duration::from_secs(8)
    ));
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_secs(4) && elapsed <= Duration::from_secs(7),
        "应等 5s 后强断，实际 {elapsed:?}"
    );
    {
        let closed = collector.closed.lock().unwrap();
        assert_eq!(closed.last().unwrap(), "连接结束");
    }
    drop(stub);
}

/// progress 超时：连接后无任何帧 -> failed（小超时验证）。
#[test]
fn progress_timeout_fails() {
    let stub = Stub::spawn();
    let mut cfg = test_cfg(&stub.url(), 4, "audio");
    cfg.fg_progress_timeout_s = 2.0;
    let collector = Arc::new(Collector::default());
    let sess = O5Session::new(&cfg, collector.clone(), None, None);
    sess.start();
    assert!(stub.wait_connected(Duration::from_secs(3)));
    // 桩不发任何帧
    let t0 = Instant::now();
    assert!(wait_state(
        &sess,
        SessionState::Failed,
        Duration::from_secs(6)
    ));
    assert!(
        t0.elapsed() >= Duration::from_secs(1),
        "不应早于 progress 超时失败"
    );
    let errors = collector.errors.lock().unwrap().clone();
    assert!(
        errors
            .iter()
            .any(|(code, msg)| code.is_empty() && msg.contains("会话建立超时")),
        "应报会话建立超时: {errors:?}"
    );
    drop(stub);
}

/// ack 超时：最老未确认 unit 超时 -> failed（小超时验证）。
#[test]
fn ack_timeout_fails() {
    let stub = Stub::spawn();
    let mut cfg = test_cfg(&stub.url(), 4, "audio");
    cfg.fg_ack_timeout_s = 1.5;
    let collector = Arc::new(Collector::default());
    let sess = O5Session::new(&cfg, collector.clone(), None, None);
    sess.start();
    assert!(stub.wait_connected(Duration::from_secs(3)));
    drive_to_ready(&stub);
    stub.send_json(json!({"type": "session.created", "session_id": "s", "resume": {}}));
    assert!(wait_state(
        &sess,
        SessionState::Ready,
        Duration::from_secs(3)
    ));

    // 桩不 ACK，投 1 个 unit 后应触发 ack 超时
    sess.offer_unit(silence(), None, Instant::now());
    assert!(stub
        .wait_json(Duration::from_secs(3), |v| v["type"] == "input.append")
        .is_some());
    assert!(wait_state(
        &sess,
        SessionState::Failed,
        Duration::from_secs(5)
    ));
    let errors = collector.errors.lock().unwrap().clone();
    assert!(
        errors
            .iter()
            .any(|(code, msg)| code.is_empty() && msg.contains("音频 unit 确认超时")),
        "应报音频确认超时: {errors:?}"
    );
    drop(stub);
}

/// 服务端 error 帧 -> failed + on_error(code, message)。
#[test]
fn server_error_fails() {
    let stub = Stub::spawn();
    let cfg = test_cfg(&stub.url(), 4, "audio");
    let collector = Arc::new(Collector::default());
    let sess = O5Session::new(&cfg, collector.clone(), None, None);
    sess.start();
    assert!(stub.wait_connected(Duration::from_secs(3)));
    drive_to_ready(&stub);
    stub.send_json(json!({"type": "session.created", "session_id": "s", "resume": {}}));
    assert!(wait_state(
        &sess,
        SessionState::Ready,
        Duration::from_secs(3)
    ));

    stub.send_json(json!({
        "type": "error", "error": {"code": "invalid_request", "message": "mode=omni 要求每 unit 1 帧"},
    }));
    assert!(wait_state(
        &sess,
        SessionState::Failed,
        Duration::from_secs(3)
    ));
    let errors = collector.errors.lock().unwrap().clone();
    assert!(
        errors
            .iter()
            .any(|(c, m)| c == "invalid_request" && m.contains("每 unit 1 帧")),
        "on_error 应带服务端 code/message: {errors:?}"
    );
    drop(stub);
}

/// api_key 走 Authorization Bearer 头；queue gate 关闭时连接后立即发 init。
#[test]
fn api_key_and_no_queue_gate() {
    let stub = Stub::spawn();
    let mut cfg = test_cfg(&stub.url(), 4, "audio");
    cfg.fg_api_key = "test-key-123".to_string();
    cfg.fg_queue_gate = false;
    let collector = Arc::new(Collector::default());
    let sess = O5Session::new(&cfg, collector.clone(), None, None);
    sess.start();
    assert!(stub.wait_connected(Duration::from_secs(3)));
    assert_eq!(stub.auth().as_deref(), Some("Bearer test-key-123"));
    // queue_gate=false：不等 queue_done 直接连发 init
    let init = stub
        .wait_json(Duration::from_secs(3), |v| v["type"] == "session.init")
        .expect("queue_gate=false 应立即发送 session.init");
    assert!(init["payload"].is_object());
    assert!(wait_state(
        &sess,
        SessionState::Initializing,
        Duration::from_secs(2)
    ));
    // drop -> stop()：Initializing 中途的优雅关闭路径（自动回包避免 5s 等待）
    stub.set_auto_close(true);
    drop(stub);
    drop(sess);
}

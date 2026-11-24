//! PC 单测：本地 TcpListener 最小 HTTP 桩（不引框架），覆盖 delegation.py 的
//! 全部行为分支——400 带 tools 自动降级重试、tool_calls 多轮循环、保留字段
//! 不可覆盖、超长截断、进度查询各分支文案（绝不出现 task_id=）、abort 丢弃、
//! 并发满文案、get_current_time 格式、超时预算文案、任务表容量。

use super::*;
use o5_tools::ToolExecutor;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 最小 HTTP 桩（Connection: close —— 一条连接一个请求）
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StubResponse {
    status: u16,
    reason: &'static str,
    body: String,
    delay_s: f64,
}

impl StubResponse {
    fn ok(body: Value) -> Self {
        StubResponse {
            status: 200,
            reason: "OK",
            body: body.to_string(),
            delay_s: 0.0,
        }
    }

    fn bad_request(body: &str) -> Self {
        StubResponse {
            status: 400,
            reason: "Bad Request",
            body: body.to_string(),
            delay_s: 0.0,
        }
    }

    fn delayed(mut self, delay_s: f64) -> Self {
        self.delay_s = delay_s;
        self
    }
}

struct Captured {
    path: String,
    head: String,
    body: String,
}

struct StubServer {
    port: u16,
    requests: Arc<Mutex<Vec<Captured>>>,
}

fn spawn_stub(script: Vec<StubResponse>) -> StubServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let port = listener.local_addr().expect("local addr").port();
    let requests: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    let sink = Arc::clone(&requests);
    let _listener = std::thread::Builder::new()
        .name("o5-test-stub".to_string())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                let script = Arc::clone(&script);
                let sink = Arc::clone(&sink);
                std::thread::Builder::new()
                    .name("o5-test-stub-conn".to_string())
                    .spawn(move || serve_conn(&mut stream, &script, &sink))
                    .ok();
            }
        })
        .expect("spawn stub listener");
    StubServer { port, requests }
}

fn serve_conn(
    stream: &mut TcpStream,
    script: &Mutex<VecDeque<StubResponse>>,
    sink: &Mutex<Vec<Captured>>,
) {
    let Some((path, head, body)) = read_request(stream) else {
        return;
    };
    sink.lock().unwrap().push(Captured { path, head, body });
    let Some(resp) = script.lock().unwrap().pop_front() else {
        write_response(stream, 500, "Internal Server Error", "{}");
        return;
    };
    if resp.delay_s > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(resp.delay_s));
    }
    write_response(stream, resp.status, resp.reason, &resp.body);
}

fn write_response(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// 读一个完整请求（头部 + Content-Length 的 body），返回 (path, 原始头部, body)。
fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut content_length = 0usize;
    for line in head.lines() {
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    while buf.len() < header_end + 4 + content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[(header_end + 4).min(buf.len())..]).into_owned();
    let path = head
        .lines()
        .next()
        .and_then(|req_line| req_line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    Some((path, head, body))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// OpenAI 兼容应答构造 + manager 组装
// ---------------------------------------------------------------------------

fn text_completion(content: &str) -> StubResponse {
    StubResponse::ok(json!({
        "choices": [ { "message": { "role": "assistant", "content": content } } ]
    }))
}

fn tool_calls_completion(tool_calls: Value, content: &str) -> StubResponse {
    StubResponse::ok(json!({
        "choices": [ { "message": {
            "role": "assistant", "content": content, "tool_calls": tool_calls
        } } ]
    }))
}

/// 无 key 的真实 ToolExecutor（工具面 schema 离线可用；未知工具名不联网）。
fn plain_tools() -> Option<Arc<ToolExecutor>> {
    Some(Arc::new(ToolExecutor::new("", "", "", "", "")))
}

fn build(
    enabled: bool,
    endpoint: &str,
    api_key: &str,
    tools: Option<Arc<ToolExecutor>>,
    max_concurrent: usize,
    max_result_chars: usize,
    request_params: Value,
) -> (DelegationManager, mpsc::Receiver<(String, String)>) {
    let (tx, rx) = mpsc::channel();
    let tx = Arc::new(Mutex::new(tx));
    let manager = DelegationManager::new(
        enabled,
        endpoint.to_string(),
        api_key.to_string(),
        "m".to_string(),
        30.0,
        max_concurrent,
        "SYS".to_string(),
        max_result_chars,
        request_params,
        tools,
        Box::new(move |id: &str, text: &str| {
            tx.lock()
                .unwrap()
                .send((id.to_string(), text.to_string()))
                .unwrap();
        }),
    );
    (manager, rx)
}

fn wait_result(rx: &mpsc::Receiver<(String, String)>) -> (String, String) {
    rx.recv_timeout(Duration::from_secs(10))
        .expect("后台结果应在 10s 内回填")
}

fn endpoint_of(stub: &StubServer) -> String {
    format!("http://127.0.0.1:{}", stub.port)
}

fn captured_json(stub: &StubServer) -> Vec<Value> {
    stub.requests
        .lock()
        .unwrap()
        .iter()
        .map(|c| serde_json::from_str(&c.body).expect("请求 body 应为 JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[test]
fn get_current_time_format_and_unsupported_tools() {
    let (mgr, rx) = build(true, "http://127.0.0.1:1", "k", None, 2, 1200, json!({}));

    // 格式 "%Y-%m-%d %H:%M:%S UTC+HH:MM"（本地时区偏移，含夏令时口径）
    let now = mgr
        .handle_tool_call("c1", "get_current_time", "{}")
        .expect("get_current_time 应立即回答");
    assert_now_str_shape(&now);

    // 未声明/不认识的工具也要回填，不能让调用悬空
    assert_eq!(
        mgr.handle_tool_call("c2", "show_card", "{}"),
        Some("Unsupported tool: show_card".to_string())
    );
    assert_eq!(
        mgr.handle_tool_call("c3", "", "{}"),
        Some("Unsupported tool: ".to_string())
    );

    // 立即回复不走 send_result（那只从后台任务线程调用）
    assert!(rx.try_recv().is_err());
}

fn assert_now_str_shape(s: &str) {
    let parts: Vec<&str> = s.splitn(3, ' ').collect();
    assert_eq!(
        parts.len(),
        3,
        "应形如 'YYYY-MM-DD HH:MM:SS UTC+HH:MM': {s}"
    );
    let (date, hm, off) = (parts[0], parts[1], parts[2]);
    assert_eq!(date.len(), 10, "{s}");
    let d: Vec<&str> = date.split('-').collect();
    assert_eq!(d.len(), 3, "{s}");
    assert!(d[0].len() == 4 && d[1].len() == 2 && d[2].len() == 2, "{s}");
    assert!(date.chars().all(|c| c.is_ascii_digit() || c == '-'), "{s}");
    assert_eq!(hm.len(), 8, "{s}");
    assert_eq!(hm.as_bytes()[2], b':');
    assert_eq!(hm.as_bytes()[5], b':');
    assert!(hm.chars().all(|c| c.is_ascii_digit() || c == ':'), "{s}");
    assert!(off.starts_with("UTC+") || off.starts_with("UTC-"), "{s}");
    let digits = &off[3..];
    assert_eq!(digits.len(), 6, "{s}");
    assert_eq!(digits.as_bytes()[3], b':');
    assert!(digits[1..3].chars().all(|c| c.is_ascii_digit()), "{s}");
    assert!(digits[4..6].chars().all(|c| c.is_ascii_digit()), "{s}");
}

#[test]
fn guards_disabled_and_concurrency_full() {
    // enabled=false：后台模型未配置
    let (mgr, _rx) = build(false, "http://127.0.0.1:1", "", None, 2, 1200, json!({}));
    assert_eq!(
        mgr.handle_tool_call("c", "delegate_task", r#"{"message": "查天气"}"#),
        Some(wrap_speak("后台模型未配置，此工具当前不可用"))
    );
    // endpoint 为空同样视为未配置
    let (mgr2, _rx2) = build(true, "", "", None, 2, 1200, json!({}));
    assert_eq!(
        mgr2.handle_tool_call("c", "delegate_task", r#"{"message": "查天气"}"#),
        Some(wrap_speak("后台模型未配置，此工具当前不可用"))
    );
    // 未启用时进度查询仍可用（无任务）
    assert_eq!(
        mgr.handle_tool_call("c", "delegate_task", "{}"),
        Some(wrap_speak("当前没有进行中的后台任务"))
    );
    assert_eq!(mgr.status_line(), "后台未配置");

    // 并发满
    let stub = spawn_stub(vec![text_completion("慢结果").delayed(1.5)]);
    let (mgr3, rx) = build(true, &endpoint_of(&stub), "k", None, 1, 1200, json!({}));
    assert!(mgr3
        .handle_tool_call("call-1", "delegate_task", r#"{"message":"任务一"}"#)
        .is_none());
    assert_eq!(mgr3.active_count(), 1);
    assert_eq!(mgr3.status_line(), "后台任务 x1");
    assert_eq!(
        mgr3.handle_tool_call("call-2", "delegate_task", r#"{"message":"任务二"}"#),
        Some(wrap_speak("后台并发已满（1），请稍后再试"))
    );
    let (id, text) = wait_result(&rx);
    assert_eq!(
        (id.as_str(), text.as_str()),
        ("call-1", wrap_speak("慢结果").as_str())
    );
    assert_eq!(mgr3.status_line(), "后台空闲");
}

#[test]
fn progress_query_branches() {
    let stub = spawn_stub(vec![text_completion("调研结论").delayed(1.2)]);
    let (mgr, rx) = build(true, &endpoint_of(&stub), "k", None, 2, 1200, json!({}));

    // 无任务
    assert_eq!(
        mgr.handle_tool_call("c0", "delegate_task", "{}"),
        Some(wrap_speak("当前没有进行中的后台任务"))
    );
    // 空白 message 同样按查询处理
    assert_eq!(
        mgr.handle_tool_call("c0b", "delegate_task", r#"{"message": "   "}"#),
        Some(wrap_speak("当前没有进行中的后台任务"))
    );
    // 参数不是 JSON 也按空参数（查询）处理
    assert_eq!(
        mgr.handle_tool_call("c0c", "delegate_task", "not-json"),
        Some(wrap_speak("当前没有进行中的后台任务"))
    );

    // 开任务（单引号 Python 字面量参数也要能解析）
    assert!(mgr
        .handle_tool_call("call-1", "delegate_task", "{'message': '做个调研'}")
        .is_none());

    // 运行中：模型编造 task_id 查询 -> 回退最近任务的真实状态
    let q = mgr
        .handle_tool_call(
            "call-2",
            "delegate_task",
            r#"{"task_id": "fc_call_000001"}"#,
        )
        .expect("查询应立即回复");
    assert!(q.starts_with(SPEAK_DIRECTIVE), "{q}");
    assert!(!q.contains("task-"), "回复绝不出现 task_id: {q}");
    let body = q.strip_prefix(SPEAK_DIRECTIVE).expect("包口播指令");
    let secs = body
        .strip_prefix("该任务仍在进行中，已运行 ")
        .and_then(|s| s.strip_suffix(" 秒"))
        .expect("运行中文案");
    assert!(
        secs.chars().all(|c| c.is_ascii_digit()),
        "秒数应为整数: {q}"
    );

    // 空 task_id -> 最近一个
    let q2 = mgr
        .handle_tool_call("call-3", "delegate_task", r#"{"task_id": ""}"#)
        .expect("查询应立即回复");
    assert!(q2.contains("该任务仍在进行中"), "{q2}");

    // 等完成
    let (id, text) = wait_result(&rx);
    assert_eq!(id, "call-1");
    assert_eq!(text, wrap_speak("调研结论"));

    // 已完成
    assert_eq!(
        mgr.handle_tool_call(
            "call-4",
            "delegate_task",
            r#"{"task_id": "fc_call_000002"}"#
        ),
        Some(wrap_speak("该任务已完成，结果刚才已经回报，请直接转述"))
    );
}

#[test]
fn abort_drops_result() {
    let stub = spawn_stub(vec![text_completion("被丢弃的结果").delayed(1.0)]);
    let (mgr, rx) = build(true, &endpoint_of(&stub), "k", None, 2, 1200, json!({}));
    assert!(mgr
        .handle_tool_call("call-x", "delegate_task", r#"{"message":"会被取消"}"#)
        .is_none());
    mgr.handle_tool_abort("call-x");
    assert_eq!(mgr.active_count(), 0, "已取消的任务不算活跃");

    // 已取消任务的结果绝不回填（丢弃）：桩 1s 后就绪，2.5s 内不应有任何回填
    assert!(rx.recv_timeout(Duration::from_millis(2500)).is_err());
    assert_eq!(
        mgr.handle_tool_call("c", "delegate_task", r#"{"task_id": "whatever"}"#),
        Some(wrap_speak("后台任务已取消。"))
    );
    assert_eq!(mgr.status_line(), "后台空闲");
}

#[test]
fn http_400_tools_degradation() {
    let stub = spawn_stub(vec![
        StubResponse::bad_request("{\"error\": \"tools not supported\"}"),
        text_completion("无工具结论"),
    ]);
    let (mgr, rx) = build(
        true,
        &endpoint_of(&stub),
        "k",
        plain_tools(),
        2,
        1200,
        json!({}),
    );
    assert!(mgr
        .handle_tool_call("c1", "delegate_task", r#"{"message":"查一下"}"#)
        .is_none());
    let (_id, text) = wait_result(&rx);
    assert_eq!(text, wrap_speak("无工具结论"));

    let reqs = captured_json(&stub);
    assert_eq!(reqs.len(), 2, "400 后应无工具重试一轮");
    assert!(reqs[0]["tools"].is_array(), "首轮应带 tools");
    assert!(reqs[1].get("tools").is_none(), "第二轮不应再带 tools");
    assert_eq!(reqs[1]["model"], "m");
    assert_eq!(reqs[1]["stream"], false);
    let paths: Vec<String> = stub
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.path.clone())
        .collect();
    assert_eq!(
        paths,
        vec![
            "/chat/completions".to_string(),
            "/chat/completions".to_string()
        ]
    );
}

#[test]
fn tool_calls_loop() {
    let stub = spawn_stub(vec![
        tool_calls_completion(
            json!([{ "id": "tc1", "type": "function",
                     "function": { "name": "echo_tool", "arguments": "{\"x\": 1}" } }]),
            "",
        ),
        text_completion("最终结论"),
    ]);
    let (mgr, rx) = build(
        true,
        &endpoint_of(&stub),
        "k",
        plain_tools(),
        2,
        1200,
        json!({}),
    );
    assert!(mgr
        .handle_tool_call("c1", "delegate_task", r#"{"message":"查天气"}"#)
        .is_none());
    let (_id, text) = wait_result(&rx);
    assert_eq!(text, wrap_speak("最终结论"));

    let reqs = captured_json(&stub);
    assert_eq!(reqs.len(), 2);
    let msgs = reqs[1]["messages"].as_array().expect("messages 应为数组");
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "SYS");
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(msgs[1]["content"], "查天气");
    assert_eq!(msgs[2]["role"], "assistant");
    assert_eq!(msgs[2]["content"], "", "content 为空时补 \"\"");
    assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "echo_tool");
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "tc1");
    // 未知工具不联网，直接回 "[参数错误]" 文案（等价 Python 的异常文案路径）
    assert_eq!(msgs[3]["content"], "[参数错误] 未知工具: echo_tool");
}

#[test]
fn reserved_fields_not_overridable() {
    let stub = spawn_stub(vec![text_completion("ok")]);
    let params = json!({
        "model": "EVIL", "stream": true, "messages": ["evil"], "tools": ["evil"],
        "tool_choice": "none", "temperature": 0.7, "max_tokens": 5,
    });
    let tools = plain_tools();
    let expect_tools = tools.as_ref().unwrap().background_definitions();
    let (mgr, rx) = build(true, &endpoint_of(&stub), "k", tools, 2, 1200, params);
    assert!(mgr
        .handle_tool_call("c1", "delegate_task", r#"{"message":"m"}"#)
        .is_none());
    let _ = wait_result(&rx);

    let reqs = captured_json(&stub);
    let b = &reqs[0];
    assert_eq!(b["model"], "m", "model 不可被 request_params 覆盖");
    assert_eq!(b["stream"], false, "stream 不可被覆盖");
    assert_eq!(
        b["messages"].as_array().unwrap().len(),
        2,
        "messages 不可被覆盖"
    );
    assert_eq!(b["tools"], expect_tools, "tools 只来自 executor 定义");
    assert!(b.get("tool_choice").is_none(), "tool_choice 永不下发");
    assert_eq!(b["temperature"], 0.7, "非保留字段正常透传");
    assert_eq!(b["max_tokens"], 5);
}

#[test]
fn result_truncation() {
    // 超长：按码点截断 + 后缀（多字节字符验证按字而非字节截断）
    let stub = spawn_stub(vec![text_completion("一二三四五六七八九十甲乙丙")]);
    let (mgr, rx) = build(true, &endpoint_of(&stub), "k", None, 2, 5, json!({}));
    assert!(mgr
        .handle_tool_call("c1", "delegate_task", r#"{"message":"m"}"#)
        .is_none());
    let (_id, text) = wait_result(&rx);
    assert_eq!(text, wrap_speak("一二三四五……（已截断）"));

    // 恰好等于上限：不截断
    let stub2 = spawn_stub(vec![text_completion("一二三四五")]);
    let (mgr2, rx2) = build(true, &endpoint_of(&stub2), "k", None, 2, 5, json!({}));
    assert!(mgr2
        .handle_tool_call("c1", "delegate_task", r#"{"message":"m"}"#)
        .is_none());
    let (_id, text2) = wait_result(&rx2);
    assert_eq!(text2, wrap_speak("一二三四五"));
}

#[test]
fn empty_and_multipart_content() {
    // choices 空
    let s1 = spawn_stub(vec![StubResponse::ok(json!({ "choices": [] }))]);
    let (m1, rx1) = build(true, &endpoint_of(&s1), "k", None, 2, 1200, json!({}));
    assert!(m1
        .handle_tool_call("c", "delegate_task", r#"{"message":"m"}"#)
        .is_none());
    assert_eq!(wait_result(&rx1).1, wrap_speak("后台模型没有返回结果"));

    // content 空白 -> strip 后为空
    let s2 = spawn_stub(vec![text_completion("  ")]);
    let (m2, rx2) = build(true, &endpoint_of(&s2), "k", None, 2, 1200, json!({}));
    assert!(m2
        .handle_tool_call("c", "delegate_task", r#"{"message":"m"}"#)
        .is_none());
    assert_eq!(wait_result(&rx2).1, wrap_speak("后台模型返回了空结果"));

    // 多段 content 列表：按 c.text 拼接
    let s3 = spawn_stub(vec![StubResponse::ok(json!({
        "choices": [ { "message": { "role": "assistant", "content": [
            { "type": "text", "text": "你好，" },
            { "type": "text", "text": "世界" }
        ] } } ]
    }))]);
    let (m3, rx3) = build(true, &endpoint_of(&s3), "k", None, 2, 1200, json!({}));
    assert!(m3
        .handle_tool_call("c", "delegate_task", r#"{"message":"m"}"#)
        .is_none());
    assert_eq!(wait_result(&rx3).1, wrap_speak("你好，世界"));
}

#[test]
fn tool_rounds_exhausted() {
    let resp = tool_calls_completion(
        json!([{ "id": "t", "type": "function",
                 "function": { "name": "echo_tool", "arguments": "{}" } }]),
        "",
    );
    let stub = spawn_stub(vec![resp; 5]);
    let (mgr, rx) = build(
        true,
        &endpoint_of(&stub),
        "k",
        plain_tools(),
        2,
        1200,
        json!({}),
    );
    assert!(mgr
        .handle_tool_call("c1", "delegate_task", r#"{"message":"m"}"#)
        .is_none());
    let (_id, text) = wait_result(&rx);
    assert_eq!(text, wrap_speak("后台工具调用轮数超限，未能给出结论"));
    assert_eq!(
        stub.requests.lock().unwrap().len(),
        5,
        "恰为 MAX_TOOL_ROUNDS"
    );
}

#[test]
fn url_forms_and_headers() {
    let stub = spawn_stub(vec![text_completion("a"), text_completion("b")]);
    // 基址（…/v1）自动补 /chat/completions
    let base = format!("http://127.0.0.1:{}/v1", stub.port);
    let (m1, rx1) = build(true, &base, "k", None, 2, 1200, json!({}));
    assert!(m1
        .handle_tool_call("c", "delegate_task", r#"{"message":"m"}"#)
        .is_none());
    assert_eq!(wait_result(&rx1).1, wrap_speak("a"));
    // 完整路径原样使用（同一桩继续出第二个应答）
    let full = format!("{base}/chat/completions");
    let (m2, rx2) = build(true, &full, "", None, 2, 1200, json!({}));
    assert!(m2
        .handle_tool_call("c", "delegate_task", r#"{"message":"m"}"#)
        .is_none());
    assert_eq!(wait_result(&rx2).1, wrap_speak("b"));

    let reqs = stub.requests.lock().unwrap();
    assert_eq!(reqs[0].path, "/v1/chat/completions");
    assert_eq!(reqs[1].path, "/v1/chat/completions");
    let h0 = reqs[0].head.to_ascii_lowercase();
    assert!(h0.contains("content-type: application/json"));
    assert!(h0.contains("authorization: bearer k"), "有 key 发 Bearer");
    let h1 = reqs[1].head.to_ascii_lowercase();
    assert!(!h1.contains("authorization:"), "空 key 不发");
}

#[test]
fn supervise_budget_and_failure_text() {
    // 预算耗尽：立即回超时文案（工作线程被放弃，不再等它）
    let out = supervise("task-x", 0.4, || {
        std::thread::sleep(Duration::from_secs(3));
        Ok("不该被采用".to_string())
    });
    assert!(out.starts_with("后台任务超时（>"), "{out}");
    assert!(out.ends_with("s）"), "{out}");

    // 异常 -> "后台任务失败: {类型}: {e}"
    let out2 = supervise("task-x", 5.0, || {
        Err(DelegationError::Timeout(
            "dial tcp: i/o timeout".to_string(),
        ))
    });
    assert_eq!(out2, "后台任务失败: Timeout: dial tcp: i/o timeout");

    // 成功路径
    let out3 = supervise("task-x", 5.0, || Ok("结论".to_string()));
    assert_eq!(out3, "结论");
}

#[test]
fn task_table_keeps_recent_20() {
    let mut table = TaskTable::default();
    for i in 0..25 {
        table.insert(TaskRecord {
            id: format!("task-{i}"),
            tool_call_id: format!("c{i}"),
            status: TaskStatus::Running,
            started: Instant::now(),
            cancelled: false,
            message: "m".to_string(),
            result: None,
        });
    }
    assert_eq!(table.order.len(), 20);
    assert_eq!(table.tasks.len(), 20);
    assert_eq!(table.active_count(), 20);
    assert!(table.tasks.contains_key("task-24"), "最新保留");
    assert!(!table.tasks.contains_key("task-4"), "最老淘汰");
    assert_eq!(table.find("task-24").unwrap().tool_call_id, "c24");
    assert_eq!(table.find("").unwrap().id, "task-24", "空 id -> 最近");
    assert!(table.find("task-999").is_none(), "编造 id 不在表内");
}

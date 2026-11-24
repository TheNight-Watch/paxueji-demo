//! 后台大脑：delegate_task 委派 -> OpenAI 兼容 /chat/completions（非流式）。
//!
//! 对齐 Glass_Omni_Cpm_Harness_Demo 的 DelegationManager + cpm-harness 的回填纪律
//! （由 Python 版 o5/delegation.py 忠实移植）：
//!
//! - 前台唯一工具 delegate_task({message?, task_id?})：带 message 发起任务，
//!   只带 task_id 查询进度（空 task_id = 最近一个任务）。
//! - 结果经 input.tool_result 回填，由前台小脑口头转述。
//! - tool_call.abort / 错误帧 -> 取消任务且绝不回填（无主 tool_result 会杀会话）。
//! - 回填文案必须"可直接口播"：绝不带 task_id= 等内部标记（训练的委派协议
//!   要求"不朗读内部 id 或回复标记"，带前缀会被当元数据直接沉默），并包上
//!   口播指令（cpm-harness wrapSpeakDirective，2026-09-03 真机排障定案：
//!   不包装时结论句会念、反问/报错文本会被当已知信息 no_action 沉默）。
//! - 后台保留联网搜索 / 地图：委派模型支持 function calling 时下发
//!   web_search / map_search（o5-tools 同一份实现），多轮工具循环。
//!
//! ## 线程模型（对照 Python 的 asyncio 版）
//!
//! - [`DelegationManager::handle_tool_call`] 在 session 读线程调用（**快路径**，
//!   只做参数解析和表记账，绝不做 HTTP）：返回 `Some(text)` = 立即回复文本
//!   （进度查询 / get_current_time / 未知工具），由调用方经其工具结果回填
//!   通道发出（等价 main.py 的 `_backfill`，含开口兜底 armSpeakNudge——
//!   nudge 在 app 层，不在本 crate）；返回 `None` = 已开后台任务，结果稍后
//!   从任务线程经 `send_result` 回调回填——**回调只从后台任务线程调用，
//!   app 侧要负责线程安全**。
//! - 每个任务一条监督线程（`o5-delegate-task`），内部再派一条工作线程
//!   （`o5-delegate-http`）跑阻塞 HTTP：监督线程 `recv_timeout(budget)` 等
//!   结果，预算耗尽立即回超时文案、工作线程被放弃——与 Python
//!   `asyncio.wait_for(loop.run_in_executor(...))` 的放弃语义一致（被放弃的
//!   线程继续跑完但结果作废）。
//!
//! ## 与 Python 版的有意偏差
//!
//! - [`handle_tool_call`](DelegationManager::handle_tool_call) 比契约摘要多了
//!   首参 `tool_call_id`：abort 要按 tool_call_id 匹配任务、任务线程回填也
//!   要用它，契约签名里省掉后这些语义无从实现（Python 版拿到的是完整
//!   call dict，本来就含 tool_call_id）。
//! - `send_result` 回调必填（main.py 的实际用法），不再有 Python 的
//!   `bind(session)` 延迟绑定兜底；立即回复改为返回值交给调用方发送。
//! - 任务线程启动失败时回填失败文案（不能让调用悬空杀会话）。
//! - [`o5_tools::ToolExecutor::execute`] 自身永不失败 / panic（异常已在内部
//!   格式化为 "[工具失败] {name}: ..." 文案），故无需 Python 的逐工具
//!   try/except；工作线程整体再套一层 catch_unwind 兜底（release 下
//!   panic=abort，此兜底只在 dev/test 生效）。
//! - 字符串截断按 Unicode 码点（Python 切片口径），`str(c)` 对极端
//!   非 JSON 类型（bool 等）的文本化可能有 "True"/"true" 级差异。

use chrono::Local;
use o5_tools::ToolExecutor;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

/// 回填口播指令（cpm-harness DEFAULT_BACKFILL_SPEAK_DIRECTIVE）：让任何后台产出
/// 都落在"该说话"的分布上——O5 拿到 tool_response 后由 spoken 生成自行决定
/// 开不开口，不包装时反问句/报错文本会被当成已知信息 no_action 沉默。
pub const SPEAK_DIRECTIVE: &str = "后台回复如下，请立即用一句话原意转述给用户，不要沉默：";

/// 后台工具循环轮数上限（4 轮工具调用 + 1 轮最终结论）。
pub const MAX_TOOL_ROUNDS: usize = 5;

/// 工具循环额外预算（秒）：每轮搜索/地图 HTTP 的时间，加在 timeoutSeconds 之上。
pub const TOOL_LOOP_BUDGET_S: f64 = 60.0;

/// 任务表容量：只保最近 N 条（含正在运行的），超出淘汰最老。
const TABLE_CAP: usize = 20;

/// 回填回调（契约类型别名）：`send_result(tool_call_id, text)`，只会从后台
/// 任务线程调用，app 侧负责线程安全。
type SendResult = Box<dyn Fn(&str, &str) + Send + Sync>;

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn wrap_speak(result: &str) -> String {
    format!("{SPEAK_DIRECTIVE}{result}")
}

/// 当前本地时间 + UTC 偏移（对齐 get_current_time 训练描述的口径）。
/// chrono 的 Local 自带夏令时判断（等价 Python 的 time.altzone/tm_isdst 分支）。
fn now_str() -> String {
    let now = Local::now();
    let off = now.offset().local_minus_utc(); // 秒，东经为正（= Python 的 -time.timezone）
    let sign = if off >= 0 { '+' } else { '-' };
    let secs = off.unsigned_abs();
    let (hh, mm) = (secs / 3600, (secs % 3600) / 60);
    format!(
        "{} UTC{sign}{hh:02}:{mm:02}",
        now.format("%Y-%m-%d %H:%M:%S")
    )
}

/// 按 Unicode 码点截断（Python `s[:n]` 口径）。
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python `len(s)` 口径（码点数，非字节数）。
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Python `str(args.get(k) or "")`：字符串原样，null/缺失为空，其余文本化。
fn coerce_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Value -> 纯文本（Python `str(c)`；字符串不加引号，其余 JSON 文本化）。
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 多段 content：`"".join(str(c.get("text", "")) if isinstance(c, dict) else str(c))`。
fn join_content(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|c| match c {
                Value::Object(m) => value_to_text(m.get("text").unwrap_or(&Value::Null)),
                other => value_to_text(other),
            })
            .collect(),
        other => value_to_text(other),
    }
}

///  assistant 回显消息里的 content：`msg.get("content") or ""`（null 补空串，
/// 列表原样保留）。
fn assistant_content(msg: &Value) -> Value {
    match msg.get("content") {
        Some(v) if !v.is_null() => v.clone(),
        _ => Value::String(String::new()),
    }
}

/// tool_calls.function.arguments：`fn.get("arguments") or "{}"`
/// （字符串透传，缺失/空为 "{}"；对象罕见，序列化回 JSON 文本）。
fn function_arguments(function: &Value) -> String {
    match function.get("arguments") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(v) if v.is_object() => v.to_string(),
        _ => "{}".to_string(),
    }
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "未知 panic".to_string()
    }
}

// ---------------------------------------------------------------------------
// 错误类型（类型名对齐 Python requests 异常，用于 "后台任务失败: {类型}: {e}"）
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum DelegationError {
    /// requests.exceptions.Timeout
    Timeout(String),
    /// requests.exceptions.ConnectionError
    Connection(String),
    /// requests.exceptions.HTTPError（raise_for_status）
    Http(String),
    /// json.JSONDecodeError
    JsonDecode(String),
    /// 兜底（含工作线程 panic）
    Runtime(String),
}

impl DelegationError {
    fn type_name(&self) -> &'static str {
        match self {
            DelegationError::Timeout(_) => "Timeout",
            DelegationError::Connection(_) => "ConnectionError",
            DelegationError::Http(_) => "HTTPError",
            DelegationError::JsonDecode(_) => "JSONDecodeError",
            DelegationError::Runtime(_) => "RuntimeError",
        }
    }
}

impl std::fmt::Display for DelegationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            DelegationError::Timeout(m)
            | DelegationError::Connection(m)
            | DelegationError::Http(m)
            | DelegationError::JsonDecode(m)
            | DelegationError::Runtime(m) => m,
        };
        f.write_str(msg)
    }
}

/// DNS/连接/超时等传输层错误 -> Timeout / ConnectionError（类型名与 requests 对齐）。
fn map_transport(t: ureq::Transport) -> DelegationError {
    let msg = t.to_string();
    if is_timeout(&t) {
        DelegationError::Timeout(msg)
    } else {
        DelegationError::Connection(msg)
    }
}

fn is_timeout(t: &ureq::Transport) -> bool {
    use std::error::Error as _;
    if let Some(src) = t.source() {
        if let Some(io_err) = src.downcast_ref::<std::io::Error>() {
            if matches!(
                io_err.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                return true;
            }
        }
    }
    let m = t.to_string().to_lowercase();
    m.contains("timed out") || m.contains("timeout")
}

// ---------------------------------------------------------------------------
// 任务表
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TaskStatus {
    Running,
    Done,
}

struct TaskRecord {
    id: String,
    tool_call_id: String,
    status: TaskStatus,
    started: Instant,
    cancelled: bool,
    /// 发起消息前 200 字（排查记账）。
    message: String,
    /// 完成结果前 200 字（排查记账；对前台只经 send_result 回填）。
    result: Option<String>,
}

/// task_id -> 状态 + 插入顺序（保最近 [`TABLE_CAP`] 条）。
#[derive(Default)]
struct TaskTable {
    tasks: HashMap<String, TaskRecord>,
    order: Vec<String>,
}

impl TaskTable {
    fn insert(&mut self, record: TaskRecord) {
        self.order.push(record.id.clone());
        self.tasks.insert(record.id.clone(), record);
        while self.order.len() > TABLE_CAP {
            let oldest = self.order.remove(0);
            self.tasks.remove(&oldest);
        }
    }

    /// 活跃数：running 且未取消（Python active_count 的口径）。
    fn active_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|t| t.status == TaskStatus::Running && !t.cancelled)
            .count()
    }

    /// task_id_arg 非空精确匹配（编造 id 对不上返回 None，回退逻辑在查询层）；
    /// 空则取最近一个仍存在的任务。
    fn find(&self, task_id_arg: &str) -> Option<&TaskRecord> {
        if !task_id_arg.is_empty() {
            return self.tasks.get(task_id_arg);
        }
        self.order.iter().rev().find_map(|tid| self.tasks.get(tid))
    }
}

// ---------------------------------------------------------------------------
// DelegationManager
// ---------------------------------------------------------------------------

struct DelegationInner {
    enabled: bool,
    endpoint: String,
    api_key: String,
    model: String,
    timeout_s: f64,
    max_concurrent: usize,
    system_prompt: String,
    max_result_chars: usize,
    request_params: Value,
    tools: Option<Arc<ToolExecutor>>,
    send_result: SendResult,
    table: Mutex<TaskTable>,
}

impl DelegationInner {
    /// 锁任务表（中毒也恢复数据继续：任务表只是记账，不值得挂掉整个后台）。
    fn lock_table(&self) -> MutexGuard<'_, TaskTable> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 任务线程主体：预算内等 HTTP 结果（或超时/异常文案），更新任务表并回填。
    fn run_task(self: Arc<Self>, task_id: String, tool_call_id: String, message: String) {
        // 工具循环要在 timeoutSeconds 之外再给搜索/地图 HTTP 留预算
        let mut budget = self.timeout_s + 5.0;
        if self.tools.is_some() {
            budget += TOOL_LOOP_BUDGET_S;
        }
        let worker = Arc::clone(&self);
        let content = supervise(&task_id, budget, move || worker.http_call(&message));
        self.finish_task(&task_id, &tool_call_id, &content);
    }

    /// 任务收尾：已取消/被淘汰的任务绝不回填；否则记 done + 结果截 200，
    /// 内容截 max_result_chars 后包口播指令回填。
    fn finish_task(&self, task_id: &str, tool_call_id: &str, content: &str) {
        {
            let mut table = self.lock_table();
            let task = match table.tasks.get_mut(task_id) {
                Some(t) if !t.cancelled => t,
                _ => {
                    log::info!("后台任务 {task_id} 已取消，结果丢弃（不回填）");
                    return;
                }
            };
            task.status = TaskStatus::Done;
            task.result = Some(truncate_chars(content, 200));
            log::debug!(
                "后台任务 {task_id} 记账: message={:?} result={:?}",
                truncate_chars(&task.message, 40),
                truncate_chars(task.result.as_deref().unwrap_or(""), 40)
            );
        }
        let content = if char_len(content) > self.max_result_chars {
            format!(
                "{}……（已截断）",
                truncate_chars(content, self.max_result_chars)
            )
        } else {
            content.to_string()
        };
        log::info!("后台任务 {task_id} 完成: {}", truncate_chars(&content, 120));
        // 回填不带 task_id 前缀（不触发"不朗读内部标记"的沉默），包口播指令
        (self.send_result)(tool_call_id, &wrap_speak(&content));
    }

    /// OpenAI 兼容 HTTP 调用（阻塞，跑在 supervise 的工作线程里）。
    fn http_call(&self, message: &str) -> Result<String, DelegationError> {
        // 兼容两种写法：基址（…/v1）自动补 /chat/completions，完整路径原样使用
        let trimmed = self.endpoint.trim_end_matches('/');
        let url = if trimmed.ends_with("/chat/completions") {
            trimmed.to_string()
        } else {
            format!("{trimmed}/chat/completions")
        };
        // 每个请求的超时 = timeoutSeconds（Python requests.post(timeout=...)）
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs_f64(self.timeout_s.max(0.001)))
            .build();
        let mut messages: Vec<Value> = vec![
            json!({ "role": "system", "content": self.system_prompt }),
            json!({ "role": "user", "content": message }),
        ];
        // 后台保留联网搜索 / 地图：模型支持 function calling 时多轮工具循环；
        // 不支持（400）则自动退回无工具单轮，行为与旧版一致。
        let tools_payload = self.tools.as_ref().map(|t| t.background_definitions());
        let mut use_tools = tools_payload.is_some();
        for _round in 0..MAX_TOOL_ROUNDS {
            let mut body = json!({
                "model": self.model,
                "stream": false,
                "messages": Value::Array(messages.clone()),
            });
            // 透传请求参数（眼镜版 requestParameters），但不允许覆盖保留字段
            if let Some(params) = self.request_params.as_object() {
                for (k, v) in params {
                    if !matches!(
                        k.as_str(),
                        "model" | "stream" | "messages" | "tools" | "tool_choice"
                    ) {
                        body[k.as_str()] = v.clone();
                    }
                }
            }
            if let Some(tp) = tools_payload.as_ref().filter(|_| use_tools) {
                body["tools"] = tp.clone();
            }
            let mut req = agent.post(&url).set("Content-Type", "application/json");
            if !self.api_key.is_empty() {
                req = req.set("Authorization", &format!("Bearer {}", self.api_key));
            }
            let resp = match req.send_string(&body.to_string()) {
                Ok(r) => r,
                // HTTP 400 且带 tools：大概率是不支持 function calling 的模型，
                // 去掉 tools 重试一轮（自动降级）
                Err(ureq::Error::Status(400, r)) if use_tools => {
                    let text = r.into_string().unwrap_or_default();
                    log::warn!(
                        "后台模型不接受 tools 参数（{}），退回无工具单轮",
                        truncate_chars(&text, 120)
                    );
                    use_tools = false;
                    continue;
                }
                Err(ureq::Error::Status(code, r)) => {
                    let status_text = r.status_text().to_string();
                    return Err(DelegationError::Http(format!(
                        "{code} {status_text} for url: {url}"
                    )));
                }
                Err(ureq::Error::Transport(t)) => return Err(map_transport(t)),
            };
            let text = resp
                .into_string()
                .map_err(|e| DelegationError::Connection(format!("{e}")))?;
            let data: Value = serde_json::from_str(&text)
                .map_err(|e| DelegationError::JsonDecode(format!("{e}")))?;
            let choices = data.get("choices").and_then(Value::as_array);
            let Some(first) = choices.and_then(|c| c.first()) else {
                return Ok("后台模型没有返回结果".to_string());
            };
            let msg = first.get("message").cloned().unwrap_or_else(|| json!({}));
            let tool_calls: Vec<Value> = msg
                .get("tool_calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if use_tools && !tool_calls.is_empty() {
                messages.push(json!({
                    "role": "assistant",
                    "content": assistant_content(&msg),
                    "tool_calls": Value::Array(tool_calls.clone()),
                }));
                for tc in &tool_calls {
                    let function = tc.get("function").cloned().unwrap_or_else(|| json!({}));
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let arguments = function_arguments(&function);
                    // o5-tools 的 execute 内部已把异常格式化为 "[工具失败]" 文案，
                    // 永不返回 Err / panic（Python 的逐工具 try/except 无需移植）
                    let result = match &self.tools {
                        Some(executor) => executor.execute(&name, &arguments),
                        None => format!("[工具失败] {name}: RuntimeError: 后台工具执行器未绑定"),
                    };
                    log::info!("后台工具 {name}: {}", truncate_chars(&result, 120));
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tc.get("id").and_then(Value::as_str).unwrap_or_default(),
                        "content": result,
                    }));
                }
                continue;
            }
            let content = msg.get("content").cloned().unwrap_or(Value::Null);
            let text = join_content(&content);
            let trimmed = text.trim();
            return Ok(if trimmed.is_empty() {
                "后台模型返回了空结果".to_string()
            } else {
                trimmed.to_string()
            });
        }
        Ok("后台工具调用轮数超限，未能给出结论".to_string())
    }
}

/// 后台大脑：delegate_task 的执行器（o5/delegation.py 忠实移植）。
///
/// 用法（o5-app 侧，等价 main.py 的 on_tool_call）：
///
/// ```ignore
/// if let Some(reply) = delegation.handle_tool_call(&call.id, &call.name, &call.arguments) {
///     self.backfill(&call.id, &reply); // send_tool_result + 开口兜底
/// }
/// // None = 后台任务已开，结果稍后经 send_result 回调回填
/// ```
#[derive(Clone)]
pub struct DelegationManager {
    inner: Arc<DelegationInner>,
}

impl DelegationManager {
    /// 构造。`enabled` 为 false **或 endpoint 为空**时视为未配置
    /// （Python：`self.enabled = enabled and bool(endpoint)`）。
    ///
    /// `send_result(tool_call_id, text)` 只会从后台任务线程调用——app 侧负责
    /// 线程安全（main.py 的 `_backfill`：send_tool_result + armSpeakNudge）。
    #[allow(clippy::too_many_arguments)] // 契约签名（CONTRACTS.md o5-delegation）
    pub fn new(
        enabled: bool,
        endpoint: String,
        api_key: String,
        model: String,
        timeout_s: f64,
        max_concurrent: usize,
        system_prompt: String,
        max_result_chars: usize,
        request_params: serde_json::Value,
        tools: Option<std::sync::Arc<o5_tools::ToolExecutor>>,
        send_result: SendResult,
    ) -> Self {
        DelegationManager {
            inner: Arc::new(DelegationInner {
                enabled: enabled && !endpoint.is_empty(),
                endpoint,
                api_key,
                model,
                timeout_s,
                max_concurrent,
                system_prompt,
                max_result_chars,
                request_params,
                tools,
                send_result,
                table: Mutex::new(TaskTable::default()),
            }),
        }
    }

    /// 活跃后台任务数（running 且未取消）。
    pub fn active_count(&self) -> usize {
        self.inner.lock_table().active_count()
    }

    /// UI 状态行（Python status_line）："后台未配置" / "后台任务 xN" / "后台空闲"。
    pub fn status_line(&self) -> String {
        if !self.inner.enabled {
            return "后台未配置".to_string();
        }
        let n = self.active_count();
        if n > 0 {
            format!("后台任务 x{n}")
        } else {
            "后台空闲".to_string()
        }
    }

    /// 工具入口（在 session 读线程调用，**快路径**，只做解析与表记账）。
    ///
    /// 返回 `Some(text)` = 立即回复文本，由调用方经其工具结果回填通道发出；
    /// `None` = 已开后台任务（不立即回复），结果稍后从任务线程经 `send_result`
    /// 回调回填。
    ///
    /// - `delegate_task`：参数用 [`o5_tools::parse_arguments`] 解析（兼容单引号
    ///   Python 字面量）。带 `message` = 开任务（守卫：未配置 / 并发满时立即
    ///   回文案）；不带 `message` = 进度查询——模型拿自造的 task_id 来查时
    ///   回退最近一个任务的真实状态，让它重新对齐 id。查询回复统一包
    ///   [`SPEAK_DIRECTIVE`]，且**绝不出现 task_id=**（读 id 会触发元数据沉默）。
    /// - `get_current_time`：立即回 `_now_str()`（"%Y-%m-%d %H:%M:%S UTC+HH:MM"）。
    /// - 未声明/不认识的工具：`Some("Unsupported tool: {name}")`——不能让调用
    ///   悬空（无主 tool_result 会杀会话）。
    pub fn handle_tool_call(
        &self,
        tool_call_id: &str,
        name: &str,
        arguments: &str,
    ) -> Option<String> {
        if name == "delegate_task" {
            return self.delegate(tool_call_id, arguments);
        }
        if name == "get_current_time" {
            return Some(now_str());
        }
        Some(format!("Unsupported tool: {name}"))
    }

    /// tool_call.abort：按 `tool_call_id` 匹配任务置 cancelled。
    /// 已取消任务完成时**结果绝不回填（直接丢弃）**——前台已放弃这次调用，
    /// 再回填会成为无主 tool_result 杀会话。
    pub fn handle_tool_abort(&self, tool_call_id: &str) {
        let mut table = self.inner.lock_table();
        for t in table.tasks.values_mut() {
            if t.tool_call_id == tool_call_id {
                t.cancelled = true;
                log::info!("后台任务已取消（{tool_call_id}）");
            }
        }
    }

    /// delegate_task 分发（Python `_delegate`）。
    fn delegate(&self, tool_call_id: &str, arguments: &str) -> Option<String> {
        let args = o5_tools::parse_arguments(arguments);
        let message = coerce_text(args.get("message")).trim().to_string();
        let task_id_arg = coerce_text(args.get("task_id")).trim().to_string();

        if message.is_empty() {
            // 查询进度。模型有时拿自造的 task_id 来查（实测出现 fc_call_000001），
            // 对不上但本地确有任务时回最近一个的真实状态，让它重新对齐 id。
            // 回复不带 task_id 前缀（训练纪律：不朗读内部 id），统一包口播指令。
            let reply = {
                let table = self.inner.lock_table();
                let task = table
                    .find(&task_id_arg)
                    .or_else(|| table.order.last().and_then(|tid| table.tasks.get(tid)));
                match task {
                    None => "当前没有进行中的后台任务".to_string(),
                    Some(t) if t.cancelled => "后台任务已取消。".to_string(),
                    Some(t) if t.status == TaskStatus::Running => {
                        let elapsed = t.started.elapsed().as_secs();
                        format!("该任务仍在进行中，已运行 {elapsed} 秒")
                    }
                    Some(_) => "该任务已完成，结果刚才已经回报，请直接转述".to_string(),
                }
            };
            return Some(wrap_speak(&reply));
        }

        if !self.inner.enabled {
            return Some(wrap_speak("后台模型未配置，此工具当前不可用"));
        }

        let task_id = {
            let mut table = self.inner.lock_table();
            if table.active_count() >= self.inner.max_concurrent {
                return Some(wrap_speak(&format!(
                    "后台并发已满（{}），请稍后再试",
                    self.inner.max_concurrent
                )));
            }
            let task_id = format!("task-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
            table.insert(TaskRecord {
                id: task_id.clone(),
                tool_call_id: tool_call_id.to_string(),
                status: TaskStatus::Running,
                started: Instant::now(),
                cancelled: false,
                message: truncate_chars(&message, 200),
                result: None,
            });
            log::info!("委派后台任务 {task_id}: {}", truncate_chars(&message, 80));
            task_id
        };

        // spawn 后台线程跑 _run_task（Python asyncio.ensure_future）
        let inner = Arc::clone(&self.inner);
        let fail_task_id = task_id.clone();
        let tcid = tool_call_id.to_string();
        let spawned = thread::Builder::new()
            .name("o5-delegate-task".to_string())
            .spawn(move || inner.run_task(task_id, tcid, message));
        if let Err(e) = spawned {
            // 线程都起不来时也不能让调用悬空杀会话：回填失败文案
            log::error!("后台任务线程启动失败: {e}");
            let content = format!("后台任务失败: RuntimeError: 无法启动后台任务线程: {e}");
            self.inner
                .finish_task(&fail_task_id, tool_call_id, &content);
        }
        None
    }
}

// ---------------------------------------------------------------------------
// 预算监督（Python asyncio.wait_for + run_in_executor 的放弃语义）
// ---------------------------------------------------------------------------

/// 在 `budget_s` 内等 `work` 的结果：超时立即返回超时文案（工作线程被放弃，
/// 与 Python wait_for 放弃 executor 线程一致）；异常映射为
/// "后台任务失败: {类型}: {e}"。
fn supervise<F>(task_id: &str, budget_s: f64, work: F) -> String
where
    F: FnOnce() -> Result<String, DelegationError> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("o5-delegate-http".to_string())
        .spawn(move || {
            // 工作线程永不阻塞监督：panic 也经 catch_unwind 转成 Err 文本
            let outcome = catch_unwind(AssertUnwindSafe(work));
            let msg = match outcome {
                Ok(r) => r,
                Err(p) => Err(DelegationError::Runtime(format!(
                    "PanicError: {}",
                    panic_text(p.as_ref())
                ))),
            };
            let _ = tx.send(msg);
        });
    if let Err(e) = spawned {
        return format!("后台任务失败: RuntimeError: 无法启动后台调用线程: {e}");
    }
    match rx.recv_timeout(Duration::from_secs_f64(budget_s.max(0.001))) {
        Ok(Ok(content)) => content,
        Ok(Err(e)) => {
            log::error!("后台任务 {task_id} 异常: {}: {}", e.type_name(), e);
            format!("后台任务失败: {}: {}", e.type_name(), e)
        }
        Err(RecvTimeoutError::Timeout) => format!("后台任务超时（>{:.0}s）", budget_s),
        Err(RecvTimeoutError::Disconnected) => {
            log::error!("后台任务 {task_id} 调用线程异常退出");
            "后台任务失败: RuntimeError: 后台调用线程异常退出".to_string()
        }
    }
}

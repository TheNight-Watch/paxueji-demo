//! 服务端 -> 客户端（帧解析辅助 + [`ServerFrame`] 归一化）。
//!
//! 字段抽取与 Python `session.py::_handle_frame` + `protocol.py` 的解析辅助一致：
//! `spoken.delta` 的文本优先 steps 批（kind=="text"）兼容顶层 text；音频走
//! [`crate::decode_audio`]；tool_call 参数从 `call.arguments`（缺省退 `args_raw`）取。
//!
//! 与 Python 的有意偏差（均为不可达正常 wire 数据的边界）：
//! - 非字符串的字段值（如数字 reason）Python 会 `str()`，这里取空串；
//! - `session.queued` 携带的 position 在契约 `Queued` 变体里没有位置（丢弃）；
//! - `queue_update` 的 position / `unit_index` 缺失时取 0（契约为裸 i64）；
//! - `think.end` 即使带文本也归一为 `ThinkEnd`（契约变体无 text 字段）；
//! - 音频解码失败的 spoken.delta：Python 在 spoken_audio 抛异常整帧丢弃，
//!   这里文本照常交付、`audio` 为 `None`。

use serde_json::{Map, Value};

use crate::codec::decode_audio;
use crate::DEFAULT_OUTPUT_SAMPLE_RATE;

/// `response.tool_call.done` 的归一化结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 服务端帧归一化（`parse_server_frame` 的产物）。
#[derive(Debug, Clone, PartialEq)]
pub enum ServerFrame {
    Queued,
    QueueUpdate { position: i64 },
    QueueDone,
    Created {
        session_id: String,
        resume_sha256: Option<String>,
    },
    UnitStarted {
        input_id: Option<String>,
        unit_index: i64,
    },
    UnitCommitted {
        input_id: Option<String>,
        unit_index: i64,
    },
    /// 音频 = (pcm16 字节, sample_rate)
    SpokenDelta {
        text: String,
        audio: Option<(Vec<u8>, u32)>,
    },
    SpokenEnd { reason: String },
    ThinkBegin,
    ThinkDelta { text: String },
    ThinkEnd,
    ToolCallBegin { id: String },
    /// id 为 None 表示帧缺 tool_call_id（session 侧应丢弃该增量）
    ToolCallDelta {
        id: Option<String>,
        arguments: String,
    },
    /// call 为 None 表示错误帧（session 侧忽略不执行）
    ToolCallDone { call: Option<ParsedToolCall> },
    ToolCallAbort { tool_call_id: String },
    NonSpokenEnd,
    Closed {
        reason: String,
        diagnostic: Option<String>,
    },
    Error { code: String, message: String },
    Unknown { type_: String },
}

/// 帧类型字符串（缺失/非字符串时为 ""）。
pub fn frame_type(frame: &Value) -> String {
    frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// `response.spoken.delta` 的文本：优先 steps 批，兼容顶层 text。
///
/// steps 是列表时（哪怕没有 kind=="text" 的项）只拼 steps，不回退顶层 text。
pub fn spoken_text(frame: &Value) -> String {
    if let Some(steps) = frame.get("steps").and_then(Value::as_array) {
        let mut parts = String::new();
        for s in steps {
            if s.get("kind").and_then(Value::as_str) == Some("text") {
                parts.push_str(s.get("text").and_then(Value::as_str).unwrap_or(""));
            }
        }
        return parts;
    }
    frame
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 帧内 `sample_rate` 兜底：字段缺失、falsy（如 0）或非法时用默认值。
/// 对应 Python `frame.get("sample_rate") or DEFAULT` + `int()` 容错。
fn frame_rate(frame: &Map<String, Value>) -> u32 {
    let v = match frame.get("sample_rate") {
        Some(v) if json_truthy(v) => v,
        _ => return DEFAULT_OUTPUT_SAMPLE_RATE,
    };
    match coerce_int(v) {
        Some(r) if (1..=u32::MAX as i64).contains(&r) => r as u32,
        _ => DEFAULT_OUTPUT_SAMPLE_RATE,
    }
}

/// `response.spoken.delta` 的音频 -> `(pcm16, sample_rate)`；无音频返回 `None`。
pub fn spoken_audio(frame: &Value) -> Option<(Vec<u8>, u32)> {
    let b64 = frame.get("audio").and_then(Value::as_str)?;
    if b64.is_empty() {
        return None;
    }
    decode_audio(b64, frame_rate(frame.as_object()?))
}

/// `response.tool_call.done` -> [`ParsedToolCall`]；错误帧返回 `None`。
///
/// 参数优先 `call.arguments`（字符串），缺失/为 null 时退顶层 `args_raw`；
/// 非字符串的 arguments 在 Python 里经 `str()` 变单引号 repr，这里用 JSON 序列化。
pub fn parse_tool_call(frame: &Value) -> Option<ParsedToolCall> {
    let has_call = matches!(frame.get("call"), Some(Value::Object(_)));
    if frame.get("error").is_some_and(json_truthy) && !has_call {
        return None;
    }
    let call = frame.get("call").and_then(Value::as_object);
    let tool_call_id = frame
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if tool_call_id.is_empty() {
        return None;
    }
    // str(call.get("name") or frame.get("name") or "")
    let name = call
        .and_then(|c| c.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            frame
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
        .to_string();
    let arguments = match call.and_then(|c| c.get("arguments")) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => frame
            .get("args_raw")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    };
    Some(ParsedToolCall {
        id: tool_call_id.to_string(),
        name,
        arguments,
    })
}

/// 解析一条服务端文本帧。非法 JSON / 非 JSON 对象返回 `None`
/// （对应 Python 侧 json.loads 异常被吞、帧丢弃）；其余按 type 归一为
/// [`ServerFrame::Unknown`]（含 type 缺失，`type_` 为 ""）。
pub fn parse_server_frame(text: &str) -> Option<ServerFrame> {
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            log::debug!("o5-protocol: 无法解析的帧（{e}）: {:.120}", text);
            return None;
        }
    };
    let frame = value.as_object()?;
    let f = &value;
    Some(match frame_type(f).as_str() {
        "session.queued" => ServerFrame::Queued,
        "session.queue_update" => ServerFrame::QueueUpdate {
            position: field_int(frame, "position").unwrap_or(0),
        },
        "session.queue_done" => ServerFrame::QueueDone,
        "session.created" => ServerFrame::Created {
            session_id: field_str(frame, "session_id"),
            resume_sha256: frame
                .get("resume")
                .and_then(|r| r.get("tts_prompt_audio_sha256"))
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        "response.unit.started" => ServerFrame::UnitStarted {
            input_id: field_opt_str(frame, "input_id"),
            unit_index: field_int(frame, "unit_index").unwrap_or(0),
        },
        "response.unit.committed" => ServerFrame::UnitCommitted {
            input_id: field_opt_str(frame, "input_id"),
            unit_index: field_int(frame, "unit_index").unwrap_or(0),
        },
        "response.spoken.delta" => ServerFrame::SpokenDelta {
            text: spoken_text(f),
            audio: spoken_audio(f),
        },
        "response.spoken.end" => ServerFrame::SpokenEnd {
            reason: field_str(frame, "reason"),
        },
        "response.think.begin" => ServerFrame::ThinkBegin,
        "response.think.delta" => ServerFrame::ThinkDelta {
            text: spoken_text(f),
        },
        "response.think.end" => ServerFrame::ThinkEnd,
        "response.tool_call.begin" => ServerFrame::ToolCallBegin {
            id: field_str(frame, "tool_call_id"),
        },
        "response.tool_call.delta" => ServerFrame::ToolCallDelta {
            id: field_opt_str(frame, "tool_call_id"),
            arguments: spoken_text(f),
        },
        "response.tool_call.done" => {
            let call = parse_tool_call(f);
            if call.is_none() {
                let raw = f.to_string();
                log::warn!("tool_call.done 错误帧原文: {}", raw.chars().take(600).collect::<String>());
            }
            ServerFrame::ToolCallDone { call }
        }
        "response.tool_call.abort" => ServerFrame::ToolCallAbort {
            tool_call_id: field_str(frame, "tool_call_id"),
        },
        "response.non_spoken.end" => ServerFrame::NonSpokenEnd,
        "session.closed" => ServerFrame::Closed {
            reason: field_str(frame, "reason"),
            diagnostic: frame
                .get("diagnostic")
                .and_then(|d| d.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        "error" => {
            let err = frame.get("error").and_then(Value::as_object);
            ServerFrame::Error {
                code: err
                    .and_then(|e| e.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                message: err
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            }
        }
        other => ServerFrame::Unknown {
            type_: other.to_string(),
        },
    })
}

/// Python `str(frame.get(k) or "")` 的字符串字段版本（非字符串取 ""）。
fn field_str(frame: &Map<String, Value>, key: &str) -> String {
    frame
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 同 [`field_str`]，但空串归一为 None（对应 Python 真值判断 `if input_id:`）。
fn field_opt_str(frame: &Map<String, Value>, key: &str) -> Option<String> {
    match frame.get(key).and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    }
}

/// 整数字段：Number（含浮点截断）与数字字符串都接受（对应 Python `int()`）。
fn coerce_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn field_int(frame: &Map<String, Value>, key: &str) -> Option<i64> {
    frame.get(key).and_then(coerce_int)
}

/// JSON 值的 Python 真值语义（error / sample_rate 兜底判断用）。
fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

//! 客户端 -> 服务端消息构造（返回完整 JSON 文本帧）。
//!
//! 输出与 Python `json.dumps(msg, ensure_ascii=False, separators=(",", ":"))`
//! 逐字节同构：字段顺序照 Python dict 插入序，非 ASCII 不转义，紧凑分隔符。
//! 动态字符串一律经 `serde_json::to_string` 转义（与 Python json 的转义规则一致）。

use serde_json::Value;

use crate::codec::pcm16_to_float32_b64;
use crate::{
    BLACK_JPEG_B64, INPUT_SAMPLE_RATE, TOOL_RESULT_DELTA_CHUNK_BYTES, UNIT_BYTES,
};

/// JSON 字符串字面量（含引号与转义；serde_json 与 Python json 的转义规则一致，
/// 非 ASCII 保留原字符）。
fn jstr(s: &str) -> String {
    serde_json::to_string(s).expect("serde_json 字符串转义不会失败")
}

/// 任意 JSON 值序列化（透传基底 / tools 定义等）。
fn jval(v: &Value) -> String {
    serde_json::to_string(v).expect("serde_json::Value 序列化不会失败")
}

/// 按插入序拼 JSON 对象；键是 ASCII 字面量（无需转义），值是已序列化的 JSON 文本。
fn join_object<I, K>(fields: I) -> String
where
    I: IntoIterator<Item = (K, String)>,
    K: AsRef<str>,
{
    let mut out = String::from("{");
    let mut first = true;
    for (k, v) in fields {
        if !first {
            out.push(',');
        }
        first = false;
        out.push('"');
        out.push_str(k.as_ref());
        out.push_str("\":");
        out.push_str(&v);
    }
    out.push('}');
    out
}

/// 构造 session.init（信封 `{type, payload}`）。
///
/// `init_payload` 作为透传基底（可带 checkpoint_profile_id / unit_policy 等），
/// 必填字段随后覆盖（与 Python dict 语义一致：基底已有的键原位更新、没有的
/// 按赋值顺序追加到末尾）；`system.segments` 由 initPayload 的既有段落 + 本函数拼接。
/// `shell_prompt` 为空则不加 shell 段（非空时末尾保证换行——训练属性）；
/// `ref_audio_b64` 是音色参考 WAV 段；`tts_audio_b64` 进 `payload.tts_prompt_audio`
/// （为 None 时保留基底里既有的 tts_prompt_audio，不主动写）。
///
/// 与 Python 的唯一顺序偏差：透传基底内部嵌套对象的键序——serde_json::Value
/// 用 BTreeMap（字典序）而 Python dict 保留插入序；键序对 JSON 语义无影响。
/// Python `build_session_init` 同参签名字段（8 参对齐源实现）。
#[allow(clippy::too_many_arguments)]
pub fn build_session_init(
    protocol_version: u8,
    task_prompt: &str,
    shell_prompt: &str,
    ref_audio_b64: Option<&str>,
    tts_audio_b64: Option<&str>,
    tools: Option<&Value>,
    init_payload: Option<&Value>,
    input_mode: Option<&str>,
) -> String {
    let empty = serde_json::Map::new();
    let base = init_payload.and_then(Value::as_object).unwrap_or(&empty);

    // payload = dict(init_payload or {})：基底键按原序占位
    let mut fields: Vec<(String, String)> =
        base.iter().map(|(k, v)| (k.clone(), jval(v))).collect();

    // config：既有 settings 原样保留，decoding_policy 缺失才补（setdefault）
    let config_json = {
        let base_config = base.get("config").and_then(Value::as_object);
        let mut settings: Vec<(String, String)> = base_config
            .iter()
            .flat_map(|c| c.iter())
            .map(|(k, v)| (k.clone(), jval(v)))
            .collect();
        if !base_config.is_some_and(|c| c.contains_key("decoding_policy")) {
            settings.push((
                "decoding_policy".to_string(),
                jstr("constrained_decoding"),
            ));
        }
        join_object(settings)
    };

    // system：既有其他键保留；segments = 基底段落 + shell 段 + 音色段 + task 段
    let system_json = {
        let base_system = base.get("system").and_then(Value::as_object);
        let mut system: Vec<(String, String)> = base_system
            .iter()
            .flat_map(|s| s.iter())
            .filter(|(k, _)| k.as_str() != "segments" && k.as_str() != "tools")
            .map(|(k, v)| (k.clone(), jval(v)))
            .collect();
        let mut segments: Vec<String> = Vec::new();
        if let Some(existing) = base_system
            .and_then(|sys| sys.get("segments"))
            .and_then(Value::as_array)
        {
            for seg in existing {
                segments.push(jval(seg));
            }
        }
        if !shell_prompt.is_empty() {
            // 训练属性：shell 段末尾必须带换行
            let text = if shell_prompt.ends_with('\n') {
                shell_prompt.to_string()
            } else {
                format!("{shell_prompt}\n")
            };
            segments.push(join_object([
                ("kind", jstr("text")),
                ("text", jstr(&text)),
            ]));
        }
        if let Some(ref_audio) = ref_audio_b64 {
            segments.push(join_object([
                ("kind", jstr("audio")),
                (
                    "audio",
                    join_object([
                        ("source", jstr("base64")),
                        ("format", jstr("wav")),
                        ("data", jstr(ref_audio)),
                    ]),
                ),
            ]));
        }
        segments.push(join_object([
            ("kind", jstr("text")),
            ("text", jstr(task_prompt)),
        ]));
        system.push(("segments".to_string(), format!("[{}]", segments.join(","))));
        // tools or []
        let tools_json = match tools {
            None | Some(Value::Null) => "[]".to_string(),
            Some(v) => jval(v),
        };
        system.push(("tools".to_string(), tools_json));
        join_object(system)
    };

    // Python 依次赋值：protocol_version/（v5 时）input_mode/mode/fc_duplex/
    // generate_audio/config/system，条件性赋值 tts_prompt_audio —— 基底已有的键原位更新
    let mut init_fields: Vec<(&str, String)> = vec![
        ("protocol_version", jstr(&protocol_version.to_string())),
    ];
    if protocol_version == 5 {
        // input_mode 只有 v5 用（audio/omni，声明本场媒体契约）；v3/v4 忽略
        init_fields.push(("input_mode", jstr(input_mode.unwrap_or("audio"))));
    }
    init_fields.extend([
        ("mode", jstr("full_duplex")),
        ("fc_duplex", "true".to_string()),
        ("generate_audio", "true".to_string()),
        ("config", config_json),
        ("system", system_json),
    ]);
    for (k, v) in init_fields {
        if let Some(slot) = fields.iter_mut().find(|(fk, _)| fk.as_str() == k) {
            slot.1 = v;
        } else {
            fields.push((k.to_string(), v));
        }
    }
    if let Some(tts_audio) = tts_audio_b64 {
        let v = join_object([
            ("source", jstr("base64")),
            ("format", jstr("wav")),
            ("data", jstr(tts_audio)),
        ]);
        if let Some(slot) = fields.iter_mut().find(|(fk, _)| fk == "tts_prompt_audio") {
            slot.1 = v;
        } else {
            fields.push(("tts_prompt_audio".to_string(), v));
        }
    }

    format!(
        "{{\"type\":{},\"payload\":{}}}",
        jstr("session.init"),
        join_object(fields)
    )
}

/// 构造 input.append（1 秒音频 unit）。
///
/// - `pcm16` 必须恰好 [`UNIT_BYTES`] 字节（否则 panic，同 Python 的 ValueError）；
/// - `version == 3` 用 `audio` 字段，v4+ 用 `audio_base64` + `sample_rate: 16000`；
/// - `force_listen` 为 true 时携带 `force_listen: true`（打断抢话）；
/// - `video_jpeg_b64`：`None` 表示 audio 模式（禁带 video_frames，v5 强校验）；
///   `Some("")` 表示 omni/video 模式但暂无帧——用 [`BLACK_JPEG_B64`] 占位
///   （对应 Python `with_video=True, video_b64=None`）；`Some(f)` 携带恰好 1 帧。
pub fn build_input_append(
    input_id: &str,
    pcm16: &[u8],
    video_jpeg_b64: Option<&str>,
    force_listen: bool,
    version: u8,
) -> String {
    assert!(
        pcm16.len() == UNIT_BYTES,
        "unit 必须是恰好 1 秒 PCM16（{} 字节），当前 {} 字节",
        UNIT_BYTES,
        pcm16.len()
    );
    let audio = pcm16_to_float32_b64(pcm16);
    let mut entry: Vec<(&str, String)> = vec![("input_id", jstr(input_id))];
    if version == 3 {
        entry.push(("audio", jstr(&audio)));
    } else {
        entry.push(("audio_base64", jstr(&audio)));
        entry.push(("sample_rate", INPUT_SAMPLE_RATE.to_string()));
    }
    if force_listen {
        entry.push(("force_listen", "true".to_string()));
    }
    if let Some(frame) = video_jpeg_b64 {
        // 视频槽不许空：最后一帧，否则黑帧占位（video_b64 or BLACK_JPEG_B64）
        let frame = if frame.is_empty() { BLACK_JPEG_B64 } else { frame };
        entry.push(("video_frames", format!("[{}]", jstr(frame))));
    }
    format!(
        "{{\"type\":{},\"input\":{}}}",
        jstr("input.append"),
        join_object(entry)
    )
}

/// 构造 input.tool_result。v4+：原子回填（content 字符串）；v3：content 数组形态。
///
/// v3 且 UTF-8 字节数超过 [`crate::TOOL_RESULT_MAX_BYTES`] 时由调用方改走
/// [`build_tool_result_slices`]（与 session.py 的分流一致）。
pub fn build_tool_result(tool_call_id: &str, content: &str, version: u8) -> String {
    let content_json = if version == 3 {
        format!(
            "[{}]",
            join_object([("kind", jstr("text")), ("text", jstr(content))])
        )
    } else {
        jstr(content)
    };
    format!(
        "{{\"type\":{},\"tool_call_id\":{},\"content\":{}}}",
        jstr("input.tool_result"),
        jstr(tool_call_id),
        content_json
    )
}

/// v3 老 dialect 的超限回填分片（delta）。
pub fn build_tool_result_delta(tool_call_id: &str, content: &str) -> String {
    format!(
        "{{\"type\":{},\"tool_call_id\":{},\"delta\":{}}}",
        jstr("input.tool_result.delta"),
        jstr(tool_call_id),
        jstr(content)
    )
}

/// v3 老 dialect 的超限回填分片（done，收尾）。
pub fn build_tool_result_done(tool_call_id: &str) -> String {
    format!(
        "{{\"type\":{},\"tool_call_id\":{}}}",
        jstr("input.tool_result.done"),
        jstr(tool_call_id)
    )
}

/// v3 老 dialect 的超限回填：按字符边界切片 delta + done。
///
/// 按字节硬切会把多字节字符劈成两半，拼回去就坏了，所以按字符累积到字节上限
/// （[`TOOL_RESULT_DELTA_CHUNK_BYTES`]）再切。返回完整 JSON 帧序列（末帧恒为 done）。
pub fn build_tool_result_slices(tool_call_id: &str, text: &str) -> Vec<String> {
    let mut events: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut size = 0usize;
    for ch in text.chars() {
        let w = ch.len_utf8();
        if size + w > TOOL_RESULT_DELTA_CHUNK_BYTES && !buf.is_empty() {
            events.push(build_tool_result_delta(tool_call_id, &buf));
            buf.clear();
            size = 0;
        }
        buf.push(ch);
        size += w;
    }
    if !buf.is_empty() {
        events.push(build_tool_result_delta(tool_call_id, &buf));
    }
    events.push(build_tool_result_done(tool_call_id));
    events
}

/// 构造 session.close。
pub fn build_session_close() -> String {
    "{\"type\":\"session.close\"}".to_string()
}

/// v4+ 文字输入通道；v4 typed 来源必须用包装文案（真机 T1-T5 实验定型），
/// v5 发裸文本（对齐 cpm-harness SemanticV5Codec）。
///
/// event_id 为 `"typed-" + 12 个十六进制随机字符`（对应 `uuid.uuid4().hex[:12]`）。
pub fn build_standalone_event(text: &str, protocol_version: u8) -> String {
    let event_id = format!("typed-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let content = if protocol_version == 5 {
        text.to_string()
    } else {
        format!("【用户文字输入】「{text}」。请当作用户在跟你说话，立刻口头回应。")
    };
    format!(
        "{{\"type\":{},\"event_id\":{},\"content\":{}}}",
        jstr("input.standalone_event"),
        jstr(&event_id),
        jstr(&content)
    )
}

//! `tools/test_session_pc.py::test_protocol_units`（174-239 行）的 Rust 移植单测电池，
//! 外加任务书要求的扩展断言（force_listen、非法长度 panic、standalone_event、
//! session.close、ServerFrame 全变体解析）。
//!
//! 与 Python 电池的已知差异：随机 PCM 用自实现的 LCG（Python 侧是
//! `np.random.default_rng(42)`），断言语义不变——往返 ≤1 LSB 对任意数据成立。

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use o5_protocol::*;
use serde_json::{json, Value};

/// 与 golden 生成脚本一致的 LCG：x = x*A + C (mod 2^64)，取高 16 位。
fn lcg_pcm(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    let mut out = Vec::with_capacity(n * 2);
    for _ in 0..n {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let v = (x >> 48) as u16 as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn parse(msg: &str) -> Value {
    serde_json::from_str(msg).expect("构造出的必须是合法 JSON")
}

// ---------------------------------------------------------------------------
// f32 编解码往返
// ---------------------------------------------------------------------------

/// Python: 随机 PCM -> b64 -> 解回，限幅 + 四舍五入会差 1 LSB，只验证量级正确。
/// （任务书：随机 PCM 多轮）
#[test]
fn float32_roundtrip_within_1_lsb() {
    for &(n, seed) in &[
        (16000usize, 42u64),
        (16000, 777),
        (16000, 20261105),
        (320, 9),
        (8, 3),
        (1, 5),
    ] {
        let pcm = lcg_pcm(n, seed);
        let b64 = pcm16_to_float32_b64(&pcm);
        let back = float32_b64_to_pcm16(&b64).expect("合法 b64 必须可解码");
        let mut max_err: i32 = 0;
        for (a, b) in pcm.chunks_exact(2).zip(back.chunks_exact(2)) {
            let x = i16::from_le_bytes([a[0], a[1]]) as i32;
            let y = i16::from_le_bytes([b[0], b[1]]) as i32;
            max_err = max_err.max((x - y).abs());
        }
        assert!(
            max_err <= 1,
            "seed={seed} n={n} 往返最大误差 {max_err} LSB（应 ≤1）"
        );
    }
}

/// 解码：nan→0、±inf→±1 后限幅、越界值 clip、rint 四舍五入到偶数。
#[test]
fn float32_decode_special_values() {
    let mut raw = Vec::new();
    for v in [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        1.5,
        -1.5,
        0.5,
        -0.5,
        1.0,
        -1.0,
        0.25,
        0.75,
    ] {
        raw.extend_from_slice(&v.to_le_bytes());
    }
    let b64 = STANDARD.encode(&raw);
    let pcm = float32_b64_to_pcm16(&b64).expect("解码失败");
    let s16 = |i: usize| i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]);
    assert_eq!(s16(0), 0); // nan -> 0
    assert_eq!(s16(1), 32767); // +inf -> 1.0 -> 32767
    assert_eq!(s16(2), -32767); // -inf -> -1.0 -> -32767
    assert_eq!(s16(3), 32767); // 1.5 clip -> 1.0
    assert_eq!(s16(4), -32767); // -1.5 clip -> -1.0
    assert_eq!(s16(5), 16384); // 0.5*32767 = 16383.5 -> rint(half-even) -> 16384
    assert_eq!(s16(6), -16384); // -16383.5 -> -16384
    assert_eq!(s16(7), 32767);
    assert_eq!(s16(8), -32767);
    assert_eq!(s16(9), 8192); // 0.25*32767 = 8191.75 -> 8192
    assert_eq!(s16(10), 24575); // 0.75*32767 = 24575.25 -> 24575
    // 非法输入
    assert!(float32_b64_to_pcm16("!!!!").is_none());
    assert!(float32_b64_to_pcm16("QUJD").is_none()); // 3 字节，非 4 的倍数
    assert!(decode_audio("", 24000).is_none()); // Python: `if not b64: return None`
    let (pcm, rate) = decode_audio(&b64, 24000).unwrap();
    assert_eq!(rate, 24000);
    assert_eq!(pcm, float32_b64_to_pcm16(&b64).unwrap());
}

// ---------------------------------------------------------------------------
// input.append
// ---------------------------------------------------------------------------

/// Python ok1/ok1b：v4 用 audio_base64+sample_rate；v3 用 audio；不带 video_frames。
#[test]
fn input_append_v4_v3_fields() {
    let pcm = lcg_pcm(16000, 42);
    let msg = parse(&build_input_append("unit_001", &pcm, None, false, 4));
    let entry = &msg["input"];
    assert_eq!(msg["type"], "input.append");
    assert!(entry.get("audio_base64").is_some());
    assert_eq!(entry["sample_rate"], 16000);
    assert_eq!(entry["input_id"], "unit_001");
    assert!(entry.get("video_frames").is_none());

    let msg3 = parse(&build_input_append("unit_001", &pcm, None, false, 3));
    let entry3 = &msg3["input"];
    assert!(entry3.get("audio").is_some());
    assert!(entry3.get("audio_base64").is_none());
    assert!(entry3.get("sample_rate").is_none());
    // 两个版本的音频载荷相同（float32 b64）
    assert_eq!(
        entry["audio_base64"].as_str().unwrap(),
        entry3["audio"].as_str().unwrap()
    );
    assert_eq!(
        entry["audio_base64"].as_str().unwrap(),
        &pcm16_to_float32_b64(&pcm)
    );
}

/// Python ok2：v5 with_video=True、无帧 -> BLACK_JPEG_B64 占位恰好 1 帧；
/// audio 模式（None）不得带 video_frames（v5 强校验）。
#[test]
fn input_append_v5_black_frame_placeholder() {
    let pcm = lcg_pcm(16000, 42);
    let msgv = parse(&build_input_append("unit_001", &pcm, Some(""), false, 5));
    let frames = msgv["input"]["video_frames"]
        .as_array()
        .expect("video 模式必须带 video_frames");
    assert_eq!(frames.len(), 1, "恰好 1 帧");
    assert_eq!(frames[0].as_str().unwrap(), BLACK_JPEG_B64);
    let decoded = STANDARD.decode(frames[0].as_str().unwrap()).unwrap();
    assert!(decoded.len() > 100);

    // audio 模式不得带 video_frames
    let msga = parse(&build_input_append("unit_001", &pcm, None, false, 5));
    assert!(msga["input"].get("video_frames").is_none());

    // 有真帧时透传恰好 1 帧
    let msgf = parse(&build_input_append("unit_001", &pcm, Some("aGVsbG8="), false, 5));
    let frames = msgf["input"]["video_frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0], "aGVsbG8=");
}

/// force_listen 标志只在 true 时携带。
#[test]
fn input_append_force_listen() {
    let pcm = lcg_pcm(16000, 42);
    let m = parse(&build_input_append("u", &pcm, None, true, 4));
    assert_eq!(m["input"]["force_listen"], true);
    let m = parse(&build_input_append("u", &pcm, None, false, 4));
    assert!(m["input"].get("force_listen").is_none());
}

/// 非 32000 字节的 unit 必须 panic（Python 的 ValueError）。
#[test]
#[should_panic(expected = "unit 必须是恰好 1 秒 PCM16")]
fn input_append_rejects_wrong_length() {
    let _ = build_input_append("u", &vec![0u8; 3200], None, false, 4);
}

// ---------------------------------------------------------------------------
// session.init
// ---------------------------------------------------------------------------

/// Python ok3：payload 结构（protocol_version/fc_duplex/decoding_policy/
/// tts_prompt_audio.data/system.segments 末段 text）。
#[test]
fn session_init_payload_structure() {
    let tools = json!([]);
    let init = parse(&build_session_init(
        4,
        "T",
        "",
        None,
        Some("QUJD"),
        Some(&tools),
        None,
        None,
    ));
    assert_eq!(init["type"], "session.init");
    let p = &init["payload"];
    assert_eq!(p["protocol_version"], "4");
    // v4 不带 input_mode（v5 专属）
    assert!(p.get("input_mode").is_none());
    assert_eq!(p["mode"], "full_duplex");
    assert_eq!(p["fc_duplex"], true);
    assert_eq!(p["generate_audio"], true);
    assert_eq!(p["config"]["decoding_policy"], "constrained_decoding");
    assert_eq!(p["tts_prompt_audio"]["data"], "QUJD");
    assert_eq!(p["tts_prompt_audio"]["source"], "base64");
    assert_eq!(p["tts_prompt_audio"]["format"], "wav");
    assert_eq!(
        p["system"]["segments"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["text"],
        "T"
    );
    assert_eq!(p["system"]["tools"], json!([]));
}

/// 完整形态：shell 段换行补齐、音色 wav 段、既有段落/透传基底/config setdefault。
#[test]
fn session_init_segments_and_passthrough() {
    let tools = json!([{"type": "function", "function": {"name": "web_search"}}]);
    let base = json!({
        "checkpoint_profile_id": "cp-1",
        "unit_policy": {"max_units": 3},
        "config": {"other": true},
        "system": {"segments": [{"kind": "text", "text": "既有"}], "foo": 1}
    });
    let init = parse(&build_session_init(
        5,
        "TASK",
        "shell>",
        Some("REFWAV"),
        Some("TTS"),
        Some(&tools),
        Some(&base),
        Some("omni"),
    ));
    let p = &init["payload"];
    // v5 媒体契约声明（audio/omni）；v3/v4 不带
    assert_eq!(p["input_mode"], "omni");
    assert_eq!(p["checkpoint_profile_id"], "cp-1");
    assert_eq!(p["unit_policy"]["max_units"], 3);
    // 既有 config 键保留；decoding_policy 缺失才补
    assert_eq!(p["config"]["other"], true);
    assert_eq!(p["config"]["decoding_policy"], "constrained_decoding");
    // system 其他键保留
    assert_eq!(p["system"]["foo"], 1);
    let segs = p["system"]["segments"].as_array().unwrap();
    assert_eq!(segs.len(), 4); // 既有 1 + shell + ref_audio + task
    assert_eq!(segs[0]["text"], "既有");
    // shell 段末尾必须带换行（训练属性）
    assert_eq!(segs[1], json!({"kind": "text", "text": "shell>\n"}));
    assert_eq!(
        segs[2],
        json!({"kind": "audio",
               "audio": {"source": "base64", "format": "wav", "data": "REFWAV"}})
    );
    assert_eq!(segs[3], json!({"kind": "text", "text": "TASK"}));
    assert_eq!(p["system"]["tools"], tools);
    assert_eq!(p["tts_prompt_audio"]["data"], "TTS");

    // shell 已带换行则不再补
    let init2 = parse(&build_session_init(
        4,
        "T",
        "shell\n",
        None,
        None,
        None,
        None,
        None,
    ));
    let segs2 = init2["payload"]["system"]["segments"].as_array().unwrap();
    assert_eq!(segs2.len(), 2);
    assert_eq!(segs2[0]["text"], "shell\n");
    // 无 tts_audio_b64 时不得带 tts_prompt_audio
    assert!(init2["payload"].get("tts_prompt_audio").is_none());
    assert_eq!(init2["payload"]["system"]["tools"], json!([]));
}

/// 基底 config 已有 decoding_policy 时必须保留原值（Python setdefault 语义）。
#[test]
fn session_init_existing_decoding_policy_kept() {
    let base = json!({"config": {"decoding_policy": "beam_search", "aaa": 1}});
    let init = parse(&build_session_init(4, "T", "", None, None, None, Some(&base), None));
    assert_eq!(
        init["payload"]["config"]["decoding_policy"],
        "beam_search"
    );
    assert_eq!(init["payload"]["config"]["aaa"], 1);
}

/// 基底带 tts_prompt_audio 而本次未提供 tts_audio_b64 时，基底值原样保留。
#[test]
fn session_init_base_tts_prompt_audio_survives() {
    let base =
        json!({"tts_prompt_audio": {"source": "base64", "format": "wav", "data": "OLD"}});
    let init = parse(&build_session_init(4, "T", "", None, None, None, Some(&base), None));
    assert_eq!(init["payload"]["tts_prompt_audio"]["data"], "OLD");
    // 提供了 tts_audio_b64 则覆盖基底
    let init2 = parse(&build_session_init(
        4,
        "T",
        "",
        None,
        Some("NEW"),
        None,
        Some(&base),
        None,
    ));
    assert_eq!(init2["payload"]["tts_prompt_audio"]["data"], "NEW");
}

// ---------------------------------------------------------------------------
// tool_result
// ---------------------------------------------------------------------------

/// Python ok4：v4 原子字符串 content；v3 content 为 [{kind:"text",text:...}] 列表。
#[test]
fn tool_result_v4_string_v3_list() {
    let tr4 = parse(&build_tool_result("tc-1", "结果", 4));
    assert_eq!(tr4["type"], "input.tool_result");
    assert_eq!(tr4["tool_call_id"], "tc-1");
    assert!(tr4["content"].is_string());
    assert_eq!(tr4["content"], "结果");

    let tr3 = parse(&build_tool_result("tc-1", "结果", 3));
    assert_eq!(tr3["type"], "input.tool_result");
    assert!(tr3["content"].is_array());
    assert_eq!(tr3["content"], json!([{"kind": "text", "text": "结果"}]));
}

/// Python ok5：v3 超长分片——末帧 done，其余 delta；字节上限内不劈多字节字符。
#[test]
fn tool_result_slices_v3() {
    let text = "很长的结果".repeat(100); // 15B * 100 = 1500 字节
    let slices = build_tool_result_slices("tc-1", &text);
    assert!(slices.len() >= 3, "1500 字节至少切成 2 个 delta + done");
    let mut rejoined = String::new();
    for (i, s) in slices.iter().enumerate() {
        let v = parse(s);
        assert_eq!(v["tool_call_id"], "tc-1");
        if i + 1 == slices.len() {
            assert_eq!(v["type"], "input.tool_result.done");
            assert!(v.get("delta").is_none());
        } else {
            assert_eq!(v["type"], "input.tool_result.delta");
            let d = v["delta"].as_str().unwrap();
            // 字符边界切：每个 delta 的 UTF-8 字节数不超过上限（且都是合法字符序列）
            assert!(d.len() <= TOOL_RESULT_DELTA_CHUNK_BYTES);
            rejoined.push_str(d);
        }
    }
    assert_eq!(rejoined, text, "delta 拼回去必须等于原文");

    // 单 delta/done 原子帧形态
    let d = parse(&build_tool_result_delta("t", "分片"));
    assert_eq!(d["type"], "input.tool_result.delta");
    assert_eq!(d["tool_call_id"], "t");
    assert_eq!(d["delta"], "分片");
    let done = parse(&build_tool_result_done("t"));
    assert_eq!(done["type"], "input.tool_result.done");
    assert_eq!(done["tool_call_id"], "t");

    // 空文本：只有 done
    assert_eq!(build_tool_result_slices("t", ""), vec![build_tool_result_done("t")]);
    // 1024 整字节内不切
    let short = "a".repeat(1024);
    assert_eq!(build_tool_result_slices("t", &short).len(), 2);
}

// ---------------------------------------------------------------------------
// 服务端解析辅助
// ---------------------------------------------------------------------------

/// Python ok6（前半）：spoken_text 抽取（steps 批 / 顶层 text）。
#[test]
fn spoken_text_extraction() {
    let st = spoken_text(&json!({
        "steps": [{"kind": "text", "text": "a"}, {"kind": "text", "text": "b"}]
    }));
    assert_eq!(st, "ab");
    let st2 = spoken_text(&json!({"text": "c"}));
    assert_eq!(st2, "c");
    // steps 里非 text 的 kind 跳过；steps 是列表时不回退顶层 text
    let st3 = spoken_text(&json!({
        "steps": [{"kind": "audio", "text": "x"}, {"kind": "text", "text": "z"}],
        "text": "top"
    }));
    assert_eq!(st3, "z");
    let st4 = spoken_text(&json!({"steps": [], "text": "top"}));
    assert_eq!(st4, "");
}

/// Python ok6（后半）：parse_tool_call——错误帧 -> None；call 参数抽取。
#[test]
fn parse_tool_call_frames() {
    // 错误帧（有 error 无 call）-> None
    assert!(parse_tool_call(&json!({"tool_call_id": "t", "error": {"code": "x"}})).is_none());
    // 无 tool_call_id -> None
    assert!(parse_tool_call(&json!({"call": {"name": "n"}})).is_none());
    // call.arguments
    let call2 = parse_tool_call(&json!({
        "tool_call_id": "t2", "call": {"name": "n", "arguments": "{}"}
    }))
    .unwrap();
    assert_eq!(call2.id, "t2");
    assert_eq!(call2.name, "n");
    assert_eq!(call2.arguments, "{}");
    // arguments 缺失退 args_raw
    let call3 = parse_tool_call(&json!({
        "tool_call_id": "t3", "args_raw": "{\"a\": 1}"
    }))
    .unwrap();
    assert_eq!(call3.name, "");
    assert_eq!(call3.arguments, "{\"a\": 1}");
    // error 但带 call：不算错误帧（有 call 就执行）
    let call4 = parse_tool_call(&json!({
        "tool_call_id": "t4", "error": {"code": "x"},
        "call": {"name": "m", "arguments": "[]"}
    }))
    .unwrap();
    assert_eq!(call4.name, "m");
}

/// Python ok7：rms_db 边界（全零/满幅/空）。
#[test]
fn rms_db_bounds() {
    assert!(rms_db(&vec![0u8; 3200]) <= -89.9, "全零应视为静音 -90");
    assert_eq!(rms_db(&[]), -90.0);
    let pcm = lcg_pcm(16000, 42);
    assert!(rms_db(&pcm) > -20.0, "随机满量程 PCM 应 > -20 dB");
    // 满幅（恒 32767）：0 dBFS 附近
    let full = i16::MAX.to_le_bytes().repeat(800);
    let v = rms_db(&full);
    assert!((-0.01..=0.0).contains(&v), "满幅应在 0dBFS 附近，实际 {v}");
    let neg = i16::MIN.to_le_bytes().repeat(800);
    let v = rms_db(&neg);
    assert!((-0.01..=0.0).contains(&v), "负满幅应在 0dBFS 附近，实际 {v}");
}

// ---------------------------------------------------------------------------
// 其他客户端帧 + ServerFrame 全变体
// ---------------------------------------------------------------------------

#[test]
fn session_close_and_standalone_event() {
    assert_eq!(build_session_close(), "{\"type\":\"session.close\"}");

    let ev = parse(&build_standalone_event("你好", 4));
    assert_eq!(ev["type"], "input.standalone_event");
    assert_eq!(
        ev["content"],
        "【用户文字输入】「你好」。请当作用户在跟你说话，立刻口头回应。"
    );
    let id = ev["event_id"].as_str().unwrap();
    assert!(id.starts_with("typed-"), "event_id 前缀：{id}");
    assert_eq!(id.len(), 18, "typed-(6) + 12 hex");
    assert!(
        id[6..].chars().all(|c| c.is_ascii_hexdigit()),
        "12 位小写十六进制"
    );
    assert_ne!(build_standalone_event("x", 4), build_standalone_event("x", 4));
    // v5：裸文本（对齐 cpm-harness SemanticV5Codec）
    let ev5 = parse(&build_standalone_event("你好", 5));
    assert_eq!(ev5["content"], "你好");
}

#[test]
fn parse_server_frame_all_variants() {
    use ServerFrame::*;

    assert_eq!(parse_server_frame(r#"{"type":"session.queued"}"#), Some(Queued));
    assert_eq!(
        parse_server_frame(r#"{"type":"session.queue_update","position":3}"#),
        Some(QueueUpdate { position: 3 })
    );
    assert_eq!(parse_server_frame(r#"{"type":"session.queue_done"}"#), Some(QueueDone));
    assert_eq!(
        parse_server_frame(
            r#"{"type":"session.created","session_id":"s1","resume":{"tts_prompt_audio_sha256":"abc"}}"#
        ),
        Some(Created {
            session_id: "s1".into(),
            resume_sha256: Some("abc".into())
        })
    );
    assert_eq!(
        parse_server_frame(r#"{"type":"session.created","session_id":"s2"}"#),
        Some(Created {
            session_id: "s2".into(),
            resume_sha256: None
        })
    );
    assert_eq!(
        parse_server_frame(r#"{"type":"response.unit.started","input_id":"u1","unit_index":7}"#),
        Some(UnitStarted {
            input_id: Some("u1".into()),
            unit_index: 7
        })
    );
    // v4 committed 不带 input_id
    assert_eq!(
        parse_server_frame(r#"{"type":"response.unit.committed","unit_index":8}"#),
        Some(UnitCommitted {
            input_id: None,
            unit_index: 8
        })
    );

    // spoken.delta：steps 文本 + 音频解码（sample_rate 帧内优先）
    let audio_b64 = STANDARD.encode(0.5f32.to_le_bytes());
    let frame = format!(
        r#"{{"type":"response.spoken.delta","steps":[{{"kind":"text","text":"你"}}],"audio":"{audio_b64}","sample_rate":48000}}"#
    );
    assert_eq!(
        parse_server_frame(&frame),
        Some(SpokenDelta {
            text: "你".into(),
            audio: Some((16384i16.to_le_bytes().to_vec(), 48000))
        })
    );
    // 顶层 text + 默认采样率 + 无音频
    assert_eq!(
        parse_server_frame(r#"{"type":"response.spoken.delta","text":"hi"}"#),
        Some(SpokenDelta {
            text: "hi".into(),
            audio: None
        })
    );
    // 空 audio 字符串 -> None
    assert_eq!(
        parse_server_frame(r#"{"type":"response.spoken.delta","text":"h","audio":""}"#),
        Some(SpokenDelta {
            text: "h".into(),
            audio: None
        })
    );

    assert_eq!(
        parse_server_frame(r#"{"type":"response.spoken.end","reason":"turn_eos"}"#),
        Some(SpokenEnd {
            reason: "turn_eos".into()
        })
    );
    assert_eq!(
        parse_server_frame(r#"{"type":"response.think.begin"}"#),
        Some(ThinkBegin)
    );
    assert_eq!(
        parse_server_frame(r#"{"type":"response.think.delta","text":"思考"}"#),
        Some(ThinkDelta {
            text: "思考".into()
        })
    );
    assert_eq!(
        parse_server_frame(r#"{"type":"response.think.end"}"#),
        Some(ThinkEnd)
    );
    assert_eq!(
        parse_server_frame(r#"{"type":"response.tool_call.begin","tool_call_id":"tc9"}"#),
        Some(ToolCallBegin {
            id: "tc9".into()
        })
    );
    // tool_call.delta 的参数走 spoken_text 抽取
    assert_eq!(
        parse_server_frame(
            r#"{"type":"response.tool_call.delta","tool_call_id":"tc9","text":"{\"q\":1}"}"#
        ),
        Some(ToolCallDelta {
            id: Some("tc9".into()),
            arguments: "{\"q\":1}".into()
        })
    );
    assert_eq!(
        parse_server_frame(r#"{"type":"response.tool_call.delta","text":"x"}"#),
        Some(ToolCallDelta {
            id: None,
            arguments: "x".into()
        })
    );
    assert_eq!(
        parse_server_frame(
            r#"{"type":"response.tool_call.done","tool_call_id":"t","error":{"code":"x"}}"#
        ),
        Some(ToolCallDone { call: None })
    );
    assert_eq!(
        parse_server_frame(
            r#"{"type":"response.tool_call.done","tool_call_id":"t2","call":{"name":"n","arguments":"{}"}}"#
        ),
        Some(ToolCallDone {
            call: Some(ParsedToolCall {
                id: "t2".into(),
                name: "n".into(),
                arguments: "{}".into()
            })
        })
    );
    assert_eq!(
        parse_server_frame(r#"{"type":"response.tool_call.abort","tool_call_id":"tc9"}"#),
        Some(ToolCallAbort {
            tool_call_id: "tc9".into()
        })
    );
    assert_eq!(
        parse_server_frame(r#"{"type":"response.non_spoken.end"}"#),
        Some(NonSpokenEnd)
    );
    assert_eq!(
        parse_server_frame(
            r#"{"type":"session.closed","reason":"server_restart","diagnostic":{"message":"pool drained"}}"#
        ),
        Some(Closed {
            reason: "server_restart".into(),
            diagnostic: Some("pool drained".into())
        })
    );
    assert_eq!(
        parse_server_frame(r#"{"type":"session.closed"}"#),
        Some(Closed {
            reason: "".into(),
            diagnostic: None
        })
    );
    assert_eq!(
        parse_server_frame(r#"{"type":"error","error":{"code":"E1","message":"boom"}}"#),
        Some(Error {
            code: "E1".into(),
            message: "boom".into()
        })
    );
    // 未知帧 / type 缺失 / 非法 JSON / 非对象
    assert_eq!(
        parse_server_frame(r#"{"type":"session.something_new"}"#),
        Some(Unknown {
            type_: "session.something_new".into()
        })
    );
    assert_eq!(
        parse_server_frame(r#"{"foo":1}"#),
        Some(Unknown { type_: "".into() })
    );
    assert_eq!(parse_server_frame("not json"), None);
    assert_eq!(parse_server_frame("42"), None);
    assert_eq!(parse_server_frame("null"), None);
}

/// frame_type / spoken_audio 辅助函数直接可用。
#[test]
fn helper_functions() {
    let f = json!({"type": "response.spoken.delta"});
    assert_eq!(frame_type(&f), "response.spoken.delta");
    assert_eq!(frame_type(&json!({})), "");
    assert_eq!(frame_type(&json!({"type": 5})), "");

    let audio_b64 = STANDARD.encode((-0.5f32).to_le_bytes());
    let f = json!({"audio": audio_b64});
    let (pcm, rate) = spoken_audio(&f).unwrap();
    assert_eq!(pcm, (-16384i16).to_le_bytes().to_vec());
    assert_eq!(rate, DEFAULT_OUTPUT_SAMPLE_RATE); // 帧内无 sample_rate -> 24k
    assert!(spoken_audio(&json!({})).is_none());
    assert!(spoken_audio(&json!({"audio": ""})).is_none());
    // 帧内 sample_rate 优先
    let f = json!({"audio": audio_b64, "sample_rate": 16000});
    assert_eq!(spoken_audio(&f).unwrap().1, 16000);
    // 非法 sample_rate 回默认
    let f = json!({"audio": audio_b64, "sample_rate": "bad"});
    assert_eq!(spoken_audio(&f).unwrap().1, DEFAULT_OUTPUT_SAMPLE_RATE);
}

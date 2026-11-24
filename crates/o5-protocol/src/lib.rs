//! O5 Realtime API 协议消息构造与解析（wire 形态 v3/v4/v5）。
//!
//! 依据：
//! - cpm-harness docs/research/o5-v4/01-spec.md（v4 实测规格）
//! - cpm-harness packages/protocol/src/o5-events.ts + wire-dialect.ts
//! - Glass_Omni_Cpm_Harness_Demo O5Protocol.java（v4 字段名）
//! - 1020 MaxiCAM2 测试报告（merge1006-128k-300step 端点跑通记录）
//!
//! 要点：
//! - 上行音频 = 16kHz 单声道 float32 LE，base64，恰好 16000 采样（1 秒一个 unit）。
//! - 下行 TTS 音频 = 24kHz 单声道 float32 LE，base64（帧内 sample_rate 字段优先）。
//! - v4/v5 音频字段名 audio_base64 + sample_rate；v3 用 audio。
//! - omni/video 模式下每个 unit 的同一条 input.append 里带恰好 1 帧 JPEG（video_frames[0]）；
//!   没有帧时用黑帧占位。audio 模式禁止带 video_frames（v5 会强校验）。
//! - v4+ 的 tool_result 一律原子回填（content 字符串）；v3 超 2048B 才用 delta/done 切片。
//!
//! 本 crate 从 Python `o5/protocol.py` 忠实移植：PCM 编解码 / 重采样 / RMS 电平与
//! numpy 参考实现逐位一致（float32 编解码、线性插值、numpy pairwise 求和），
//! `tests/golden/protocol_golden.json` 由参考实现生成做位级回归。
//! 不引入任何网络/TLS 依赖（那是 o5-session 的事）。

pub mod black_jpeg;
mod client;
mod codec;
mod server;

pub use black_jpeg::BLACK_JPEG_B64;
pub use client::{
    build_input_append, build_session_close, build_session_init, build_standalone_event,
    build_tool_result, build_tool_result_delta, build_tool_result_done, build_tool_result_slices,
};
pub use codec::{
    decode_audio, float32_b64_to_pcm16, pcm16_to_float32_b64, resample_pcm16, rms_db,
};
pub use server::{
    frame_type, parse_server_frame, parse_tool_call, spoken_audio, spoken_text, ParsedToolCall,
    ServerFrame,
};

/// 上行音频采样率（16kHz 单声道）。
pub const INPUT_SAMPLE_RATE: u32 = 16000;
/// 每个 unit 恰好 16000 采样（1 秒）。
pub const UNIT_SAMPLES: usize = 16000;
/// PCM16 单声道一个 unit 的字节数。
pub const UNIT_BYTES: usize = UNIT_SAMPLES * 2;
/// 下行 TTS 默认采样率（帧内 sample_rate 字段优先）。
pub const DEFAULT_OUTPUT_SAMPLE_RATE: u32 = 24000;
/// v3 老 dialect 超限回填的字节上限（超过则改走 delta/done 分片）。
pub const TOOL_RESULT_MAX_BYTES: usize = 2048;
/// v3 delta 分片的字节上限（按字符边界切，不劈开多字节字符）。
pub const TOOL_RESULT_DELTA_CHUNK_BYTES: usize = 1024;

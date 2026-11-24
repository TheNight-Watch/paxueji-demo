//! 与 Python `o5/protocol.py`（numpy 2.5.4 参考实现）的位级回归。
//!
//! `tests/golden/protocol_golden.json` 由参考实现生成：PCM -> b64、特殊浮点解码、
//! rms_db 的 f32 位型、resample 的输出字节，全部逐位比对；BLACK_JPEG_B64 逐字符比对。

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use o5_protocol::*;
use serde_json::Value;

fn golden() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/protocol_golden.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("golden 文件存在"))
        .expect("golden 文件是合法 JSON")
}

fn unhex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .chunks_exact(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

/// 与生成脚本一致的 LCG。
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

/// b64 编解码：与 numpy 参考实现逐字节一致。
#[test]
fn golden_pcm_b64_roundtrip() {
    let g = golden();
    let pcm = lcg_pcm(16000, 42);
    assert_eq!(unhex(g["pcm_hex"].as_str().unwrap()), pcm, "LCG 必须与生成端一致");
    let b64 = pcm16_to_float32_b64(&pcm);
    assert_eq!(b64, g["b64"].as_str().unwrap(), "上行 float32 b64 逐字节一致");
    let back = float32_b64_to_pcm16(&b64).unwrap();
    assert_eq!(
        back,
        unhex(g["roundtrip_hex"].as_str().unwrap()),
        "下行解码逐字节一致"
    );
}

/// 特殊浮点值（nan/inf/clip 边界）的解码黄金值。
#[test]
fn golden_special_values() {
    let g = golden();
    let b64 = g["special_b64"].as_str().unwrap();
    let pcm = float32_b64_to_pcm16(b64).expect("必须可解码");
    assert_eq!(
        pcm,
        unhex(g["special_pcm_hex"].as_str().unwrap()),
        "nan_to_num + clip + rint 逐字节一致"
    );
}

/// rms_db 的 f32 位型（覆盖 pairwise 求和的各分支：n<8、8..=128、>128、递归切半）。
#[test]
fn golden_rms_db_bits() {
    let g = golden();
    for (name, case) in g["rms"].as_object().unwrap() {
        let buf = unhex(case["hex"].as_str().unwrap());
        let bits = rms_db(&buf).to_bits();
        let want = case["f32_bits"].as_u64().unwrap() as u32;
        assert_eq!(bits, want, "rms_db({name}) 的 f32 位型与 numpy 参考不一致");
    }
}

/// resample：linspace + interp + clip + rint 全链路逐字节一致。
#[test]
fn golden_resample_bytes() {
    let g = golden();
    for case in g["resample"].as_array().unwrap() {
        let input = unhex(case["in_hex"].as_str().unwrap());
        let from = case["from"].as_u64().unwrap() as u32;
        let to = case["to"].as_u64().unwrap() as u32;
        let out = resample_pcm16(&input, from, to);
        assert_eq!(
            out,
            unhex(case["out_hex"].as_str().unwrap()),
            "resample {from}->{to}（{} 采样）与 numpy 参考不一致",
            input.len() / 2
        );
    }
}

/// BLACK_JPEG_B64 逐字符一致（防手抄漂移）。
#[test]
fn golden_black_jpeg() {
    let g = golden();
    assert_eq!(BLACK_JPEG_B64, g["black_jpeg"].as_str().unwrap());
    assert_eq!(BLACK_JPEG_B64.len(), 5020);
    let decoded = STANDARD.decode(BLACK_JPEG_B64).unwrap();
    assert!(decoded.len() > 100);
    assert_eq!(&decoded[..2], &[0xFF, 0xD8], "JPEG 魔数 SOI");
}

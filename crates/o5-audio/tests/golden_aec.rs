//! 黄金向量测试：Rust 移植 vs Python 参考实现（`o5/aec.py` / `o5/subband_aec.py`）。
//!
//! fixtures 由 `tests/gen_aec_fixtures.py` 生成：7 个 24s 合成场景（echo_only /
//! doubletalk / quiet / drift × {nlms, sub}），25ms 块**内容对齐**节奏（与官方
//! tools/test_aec_pc.py run_stream 相同：先 `push_far(far_block)` 再
//! `cancel(near_block)`），输出拼接后与 `*.out.s16` 逐段比对。
//!
//! 容差档位（依实测定档，见各场景注释）：
//! - 收敛段（10s 后）：逐 0.5s 段 `diff_rms <= 1% * ref_rms` 或 `max|diff| <= 3 LSB`；
//! - 瞬态段（10s 前，含锁延迟/预热/伪峰重锁的相位差）：放宽到 24 LSB / 2% RMS。
//!
//! 数值不完全逐位一致是预期内的：参考实现跑在 numpy（BLAS 累加序、pocketfft），
//! 移植用朴素 f32/f64 序贯累加 + rustfft，误差在 1e-7 相对量级；自适应滤波是
//! 收缩迭代（线搜索保证残差不增），微差不会发散。

use std::fs;
use std::path::PathBuf;

use o5_audio::{AecImpl, NlmsAec, NlmsAecParams, SubbandParams, SubbandSuppressor};
use serde_json::Value;

const SR: usize = 16000;
const BLOCK_MS: usize = 25;
/// 比对分段长度（样本）：0.5s。
const SEG: usize = 8000;
/// 收敛段起点（秒）。
const CONV_START_S: f64 = 10.0;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/aec")
}

fn load_s16(scenario: &str, kind: &str) -> Vec<i16> {
    let p = fixture_dir().join(format!("{scenario}.{kind}.s16"));
    let raw = fs::read(&p).unwrap_or_else(|e| panic!("读 {} 失败：{}", p.display(), e));
    let mut out = Vec::with_capacity(raw.len() / 2);
    for c in raw.chunks_exact(2) {
        out.push(i16::from_le_bytes([c[0], c[1]]));
    }
    out
}

fn load_meta(scenario: &str) -> Value {
    let p = fixture_dir().join(format!("{scenario}.meta.json"));
    serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap()
}

/// 生成器同款节奏回放：先 push_far 后 cancel，输出拼接。
fn replay(aec: &mut AecImpl, near: &[i16], far: &[i16], bs: usize) -> Vec<i16> {
    let n_blocks = near.len().min(far.len()) / bs;
    let mut out = Vec::with_capacity(near.len());
    for i in 0..n_blocks {
        aec.push_far(&far[i * bs..(i + 1) * bs]);
        out.extend_from_slice(&aec.cancel(&near[i * bs..(i + 1) * bs]));
    }
    out
}

#[derive(Debug, Clone, Copy)]
struct SegStat {
    t0: f64,
    ref_rms: f64,
    diff_rms: f64,
    max_abs: f64,
}

/// 逐段比对统计（RMS 用 f64，与生成器 meta 的统计口径一致）。
fn segment_stats(got: &[i16], want: &[i16]) -> Vec<SegStat> {
    assert_eq!(got.len(), want.len(), "输出长度不一致");
    let mut out = Vec::new();
    for (i, (g, w)) in got.chunks(SEG).zip(want.chunks(SEG)).enumerate() {
        let mut s2 = 0f64;
        let mut d2 = 0f64;
        let mut mx = 0f64;
        for (&a, &b) in g.iter().zip(w) {
            let da = a as f64;
            let db = b as f64;
            s2 += db * db;
            let d = da - db;
            d2 += d * d;
            mx = mx.max(d.abs());
        }
        out.push(SegStat {
            t0: (i * SEG) as f64 / SR as f64,
            ref_rms: (s2 / g.len() as f64).sqrt(),
            diff_rms: (d2 / g.len() as f64).sqrt(),
            max_abs: mx,
        });
    }
    out
}

struct Tolerance {
    /// 收敛段：diff_rms / ref_rms 上限。
    conv_rms_rel: f64,
    /// 收敛段：max|diff| 上限（LSB），与 rms_rel 满足其一即可。
    conv_max_lsb: f64,
    /// 瞬态段：diff_rms / ref_rms 上限。
    trans_rms_rel: f64,
    /// 瞬态段：max|diff| 上限（LSB），与 rms_rel 满足其一即可。
    trans_max_lsb: f64,
    /// 延迟锁定容差（ms，对 meta.stats.delay_ms）。
    delay_tol_ms: f64,
}

fn check_scenario(scenario: &str, aec: &mut AecImpl, tol: &Tolerance) {
    let near = load_s16(scenario, "near");
    let far = load_s16(scenario, "far");
    let want = load_s16(scenario, "out");
    let bs = SR * BLOCK_MS / 1000;
    let got = replay(aec, &near, &far, bs);
    assert_eq!(got.len(), want.len(), "{scenario}：输出长度（速率守恒）");
    let stats = segment_stats(&got, &want);
    let mut failures = Vec::new();
    for s in &stats {
        let (rel, max_ok, region, rel_lim, max_lim) = if s.t0 >= CONV_START_S {
            (
                s.diff_rms / s.ref_rms.max(1e-12),
                s.max_abs <= tol.conv_max_lsb,
                "conv",
                tol.conv_rms_rel,
                tol.conv_max_lsb,
            )
        } else {
            (
                s.diff_rms / s.ref_rms.max(1e-12),
                s.max_abs <= tol.trans_max_lsb,
                "trans",
                tol.trans_rms_rel,
                tol.trans_max_lsb,
            )
        };
        let ok = rel <= rel_lim || max_ok;
        if !ok {
            failures.push(format!(
                "{scenario} {region} t={:5.1}s rel={:6.3}% max={:6.1} (ref_rms={:8.2}, 限 rel<={} max<={})",
                s.t0,
                rel * 100.0,
                s.max_abs,
                s.ref_rms,
                rel_lim,
                max_lim
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{scenario}：{} 段超容差（conv: rms<={} 或 max<={}；trans: rms<={} 或 max<={}）：\n{}",
        failures.len(),
        tol.conv_rms_rel,
        tol.conv_max_lsb,
        tol.trans_rms_rel,
        tol.trans_max_lsb,
        failures.join("\n")
    );

    // meta 一致性：锁定/收敛/延迟（±tolerance）。
    let meta = load_meta(scenario);
    let mstats = &meta["stats"];
    let rust = aec.stats();
    assert_eq!(
        rust["locked"].as_bool().unwrap_or(false),
        mstats["locked"].as_bool().unwrap_or(false),
        "{scenario}：locked 不一致（rust={} py={}）",
        rust["locked"], mstats["locked"]
    );
    if mstats["locked"].as_bool().unwrap_or(false) {
        let py = mstats["delay_ms"].as_f64().unwrap_or(0.0);
        let rs = rust["delay_ms"].as_f64().unwrap_or(0.0);
        assert!(
            (rs - py).abs() <= tol.delay_tol_ms,
            "{scenario}：delay_ms 偏差过大 rust={} py={}",
            rs,
            py
        );
    }
    // 整体抑制水平：tail_erle（生成器口径：near 后半段 RMS² / 输出后半段 RMS²，dB）。
    let half = got.len() / 2;
    let er = |x: &[i16]| {
        (x.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / x.len() as f64).sqrt()
    };
    let tail_erle =
        10.0 * ((er(&near[half..]).powi(2) + 1e-12) / (er(&got[half..]).powi(2) + 1e-12)).log10();
    let py_tail = meta["tail_erle_db"].as_f64().unwrap_or(0.0);
    assert!(
        (tail_erle - py_tail).abs() <= 3.0,
        "{scenario}：tail_erle_db 偏差过大 rust={:.2} py={:.2}",
        tail_erle,
        py_tail
    );
    eprintln!(
        "{scenario}: OK (rust tail_erle={:.2}dB py={:.2}dB delay={:?})",
        tail_erle,
        py_tail,
        rust["delay_ms"].as_f64()
    );
}

fn nlms() -> AecImpl {
    AecImpl::Nlms(NlmsAec::new(NlmsAecParams {
        sample_rate: 16000,
        max_delay_ms: 1000.0,
        ..Default::default()
    }))
}

fn sub() -> AecImpl {
    AecImpl::Subband(SubbandSuppressor::new(SubbandParams {
        sample_rate: 16000,
        max_delay_ms: 1000.0,
        ..Default::default()
    }))
}

// ---- NLMS 场景 -------------------------------------------------------------

/// 纯回声（连续语音 far，echo 0.55×tanh 软削波）。
///
/// 实测（x86_64 PC，rustfft 6.4）：全 24s max|diff| = 1 LSB、收敛段最差
/// rel-RMS 0.09%（延迟估计与 Python 逐位一致，530.00007615182ms）。
/// 容差留裕量：收敛段 3 LSB / 1%，瞬态段 24 LSB / 2%。
#[test]
fn nlms_echo_only() {
    check_scenario(
        "nlms_echo_only",
        &mut nlms(),
        &Tolerance {
            conv_rms_rel: 0.01,
            conv_max_lsb: 3.0,
            trans_rms_rel: 0.02,
            trans_max_lsb: 24.0,
            delay_tol_ms: 10.0,
        },
    );
}

/// 双端说话（3~8s、14~18s 两窗近端语音）。
///
/// 实测：全 24s max|diff| = 1 LSB、收敛段最差 rel-RMS 0.09%（DTD 冻结
/// 判据在本数据上未发生翻转）。容差留裕量到 24 LSB / 5%，防 DTD 在
/// 阈值附近的偶发翻转造成成段冻结差。
#[test]
fn nlms_doubletalk() {
    check_scenario(
        "nlms_doubletalk",
        &mut nlms(),
                &Tolerance {
            conv_rms_rel: 0.01,
            conv_max_lsb: 3.0,
            trans_rms_rel: 0.02,
            trans_max_lsb: 24.0,
            delay_tol_ms: 10.0,
        },
    );
}

/// 近端几乎安静（echo×0.05，-26dBFS 量级）。
///
/// 实测：全 24s max|diff| = 1 LSB、收敛段最差 rel-RMS 0.16%（信号小，
/// 相对误差放大）。容差 24 LSB / 5%。
#[test]
fn nlms_quiet() {
    check_scenario(
        "nlms_quiet",
        &mut nlms(),
                &Tolerance {
            conv_rms_rel: 0.01,
            conv_max_lsb: 3.0,
            trans_rms_rel: 0.02,
            trans_max_lsb: 24.0,
            delay_tol_ms: 10.0,
        },
    );
}

/// 漂移场景（far 时钟 +0.1%/s，多径 taps，12~14s 近端语音）。
///
/// 实测：全 24s max|diff| = 1 LSB、收敛段最差 rel-RMS 0.001%——重估节拍与
/// Python 完全一致（delay_ms 552.2414781735802 与 meta 逐位相同），漂移
/// 补偿路径对位。容差 24 LSB / 5% 留裕量（若重估节拍差一拍，权重整数
/// 平移的相位差会到这个量级）。
#[test]
fn nlms_drift() {
    check_scenario(
        "nlms_drift",
        &mut nlms(),
                &Tolerance {
            conv_rms_rel: 0.01,
            conv_max_lsb: 3.0,
            trans_rms_rel: 0.02,
            trans_max_lsb: 24.0,
            delay_tol_ms: 25.0,
        },
    );
}

// ---- 子带场景 --------------------------------------------------------------

/// 纯回声（句子 far 1.5s 开 / 0.7s 关）。
///
/// 实测：全 24s **逐位一致**（max|diff| = 0）；输出长度 = 输入 - 496
/// （STFT 群延迟 + 尾帧），速率守恒精确成立。容差 3 LSB / 1% 防平台
/// 浮点差。
#[test]
fn sub_echo_only() {
    check_scenario(
        "sub_echo_only",
        &mut sub(),
                &Tolerance {
            conv_rms_rel: 0.01,
            conv_max_lsb: 3.0,
            trans_rms_rel: 0.02,
            trans_max_lsb: 24.0,
            delay_tol_ms: 25.0,
        },
    );
}

/// 双端说话（全程近端语音 + 句子 far）。
///
/// 实测：全 24s 逐位一致（max|diff| = 0）。容差 3 LSB / 1% 防平台浮点差。
#[test]
fn sub_doubletalk() {
    check_scenario(
        "sub_doubletalk",
        &mut sub(),
                &Tolerance {
            conv_rms_rel: 0.01,
            conv_max_lsb: 3.0,
            trans_rms_rel: 0.02,
            trans_max_lsb: 24.0,
            delay_tol_ms: 25.0,
        },
    );
}

/// 近端几乎安静（echo×0.05）。
///
/// 实测：全 24s 逐位一致（max|diff| = 0）。容差 3 LSB / 1% 防平台浮点差。
#[test]
fn sub_quiet() {
    check_scenario(
        "sub_quiet",
        &mut sub(),
                &Tolerance {
            conv_rms_rel: 0.01,
            conv_max_lsb: 3.0,
            trans_rms_rel: 0.02,
            trans_max_lsb: 24.0,
            delay_tol_ms: 25.0,
        },
    );
}

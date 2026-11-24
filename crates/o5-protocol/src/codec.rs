//! PCM 编解码 / 重采样 / RMS 电平。
//!
//! 数值语义与 Python `o5/protocol.py`（numpy 参考实现）逐位一致：
//! - `pcm16_to_float32_b64`：int16 / 32768.0（f32，除以 2 的幂，精确）。
//! - `float32_b64_to_pcm16`：nan_to_num（nan→0、±inf→±1）→ clip ±1 → rint(x*32767.0)
//!   （`rint` 是四舍五入到偶数，对应 `np.rint`）。
//! - `resample_pcm16`：`np.linspace` + `np.interp`（float64 插值）复刻。
//! - `rms_db`：float32 平方与 numpy pairwise 求和算法复刻（8 累加器、128 块、
//!   二分递归），末段 20*log10 在 float64 里算。
//!
//! 与 Python 的偏差（都不可达正常 wire 数据）：
//! - Python `base64.b64decode` 默认 `validate=False` 会忽略非法字符；这里用
//!   base64 crate 的严格 STANDARD 引擎，非法 base64 直接返回 `None`。
//! - 奇数字节的 PCM 输入：numpy `frombuffer` 会抛 ValueError，这里同样 panic。

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

fn pcm16_len_check(pcm16: &[u8]) {
    if !pcm16.len().is_multiple_of(2) {
        panic!(
            "pcm16 长度必须是 2 的倍数（PCM16 LE），当前 {} 字节",
            pcm16.len()
        );
    }
}

/// PCM16 LE bytes -> float32(/32768) LE -> base64。
pub fn pcm16_to_float32_b64(pcm16: &[u8]) -> String {
    pcm16_len_check(pcm16);
    let mut f32s = Vec::with_capacity(pcm16.len() * 2);
    for b in pcm16.chunks_exact(2) {
        let s = i16::from_le_bytes([b[0], b[1]]);
        // int16 / 32768.0（f32 除以 2 的幂，精确无损）
        f32s.extend_from_slice(&((s as f32) / 32768.0f32).to_le_bytes());
    }
    STANDARD.encode(f32s)
}

/// base64 float32 LE -> 限幅 [-1,1] -> PCM16 LE（四舍五入到偶数）。
///
/// 与 Python `float32_b64_to_pcm16` 一致；解析失败（非法 base64 或字节数不是 4
/// 的倍数）返回 `None`（Python 抛 ValueError，由上层吞掉）。
pub fn float32_b64_to_pcm16(encoded: &str) -> Option<Vec<u8>> {
    let raw = match STANDARD.decode(encoded) {
        Ok(raw) => raw,
        Err(e) => {
            log::debug!("o5-protocol: audio 帧不是合法 base64: {e}");
            return None;
        }
    };
    if !raw.len().is_multiple_of(4) {
        log::debug!("o5-protocol: audio 帧长度不是 4 的倍数（{} 字节）", raw.len());
        return None;
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    for b in raw.chunks_exact(4) {
        let x = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        // np.nan_to_num(x, nan=0.0, posinf=1.0, neginf=-1.0)
        let x = if x.is_nan() {
            0.0
        } else if x == f32::INFINITY {
            1.0
        } else if x == f32::NEG_INFINITY {
            -1.0
        } else {
            x
        };
        let x = x.clamp(-1.0, 1.0);
        // np.rint(x * 32767.0)（f32 乘法；rint = 四舍五入到偶数）
        let r = (x * 32767.0f32).round_ties_even();
        out.extend_from_slice(&(r as i16).to_le_bytes());
    }
    Some(out)
}

/// `response.spoken.delta` 的音频 -> `(pcm16 字节, sample_rate)`；无音频返回 `None`。
///
/// 对应 Python `spoken_audio` 的主体：空串视为无音频；`default_rate` 是帧内
/// `sample_rate` 缺失/非法时的兜底（帧字段抽取在 [`crate::parse_server_frame`] 里）。
pub fn decode_audio(b64: &str, default_rate: u32) -> Option<(Vec<u8>, u32)> {
    if b64.is_empty() {
        return None;
    }
    let pcm = float32_b64_to_pcm16(b64)?;
    Some((pcm, default_rate))
}

/// 线性插值重采样 PCM16（用于播放端 24k->48k 之类的兜底）。
///
/// 复刻 `resample_pcm16`：`np.linspace(0, len-1, n_out)` 采样点 + `np.interp`
/// 线性插值（float64 运算），再 clip 到 [-32768, 32767] 并 rint。输出长度是
/// `int(len(src) * rate_to / rate_from)` 个采样。
pub fn resample_pcm16(pcm16: &[u8], rate_from: u32, rate_to: u32) -> Vec<u8> {
    if rate_from == rate_to || pcm16.is_empty() {
        return pcm16.to_vec();
    }
    pcm16_len_check(pcm16);
    // np.frombuffer(...).astype(np.float32)：i16 -> f32 -> f64（两步都精确）
    let src: Vec<f64> = pcm16
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 as f64)
        .collect();
    let n = src.len();
    // n_out = int(len(src) * rate_to / rate_from)
    let n_out = ((n as f64) * (rate_to as f64) / (rate_from as f64)) as usize;
    if n_out == 0 {
        return Vec::new();
    }
    // idx = np.linspace(0.0, len(src) - 1, n_out)：
    // step = delta/div，idx[j] = j*step；仅 num>1 时把末点精确对齐 stop（endpoint），
    // num==1 时只有 [start]（即 0.0）
    let stop = (n - 1) as f64;
    let step = if n_out > 1 { stop / (n_out - 1) as f64 } else { 0.0 };
    let mut out = Vec::with_capacity(n_out * 2);
    for j in 0..n_out {
        let x = if n_out > 1 && j == n_out - 1 {
            stop
        } else {
            j as f64 * step
        };
        let v = interp_linear(x, &src);
        // np.rint(np.clip(v, -32768, 32767))
        let v = v.clamp(-32768.0, 32767.0).round_ties_even();
        out.extend_from_slice(&(v as i16).to_le_bytes());
    }
    out
}

/// `np.interp(x, xp=arange(n), fp)`：xp 是 0..n-1 的严格递增整数格点，fp 为
/// 任意浮点（本 crate 里都是整数格值的 f64）。采样点恒在 [0, n-1] 内。
fn interp_linear(x: f64, fp: &[f64]) -> f64 {
    let n = fp.len();
    if x <= 0.0 {
        return fp[0];
    }
    if x >= (n - 1) as f64 {
        return fp[n - 1];
    }
    // x 落在格点 j 与 j+1 之间（j = floor(x)，x 恰为整数格点时差值为 0，
    // 与 numpy binary_search 两种取界在整数 fp 下结果一致）
    let j = (x as usize).min(n - 2);
    let slope = (fp[j + 1] - fp[j]) / ((j + 1) as f64 - j as f64);
    slope * (x - j as f64) + fp[j]
}

/// numpy pairwise 求和的块大小（loops.c.src 的 PW_BLOCKSIZE）。
const PW_BLOCKSIZE: usize = 128;

/// 复刻 numpy `@TYPE@_pairwise_sum`（loops_utils.h，float32 路径）：
/// - n < 8：从 -0.0 起顺序累加（保留负零语义）；
/// - n <= 128：8 路累加器吞掉 8 的倍数部分，先做树形合并
///   `((r0+r1)+(r2+r3)) + ((r4+r5)+(r6+r7))`，再把尾部余数顺序加到结果上；
/// - n > 128：对半切（切点对齐 8）后递归相加。
///
/// 这是 `np.mean`/`np.add.reduce` 对连续 float32 数组的求和顺序，保证与
/// `rms_db` 的参考值逐位一致。
fn pairwise_sum(a: &[f32]) -> f32 {
    pairwise_sum_inner(a)
}

fn pairwise_sum_inner(a: &[f32]) -> f32 {
    let n = a.len();
    if n < 8 {
        let mut res: f32 = -0.0;
        for &v in a {
            res += v;
        }
        return res;
    }
    if n <= PW_BLOCKSIZE {
        let mut r = [0.0f32; 8];
        r.copy_from_slice(&a[..8]);
        let mut i = 8;
        let aligned_end = n - (n % 8);
        while i < aligned_end {
            for k in 0..8 {
                r[k] += a[i + k];
            }
            i += 8;
        }
        // 先树形合并，再顺序吞尾部余数（与 numpy 一致）
        let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        while i < n {
            res += a[i];
            i += 1;
        }
        return res;
    }
    // 对半切，切点对齐 8
    let mut n2 = n / 2;
    n2 -= n2 % 8;
    pairwise_sum_inner(&a[..n2]) + pairwise_sum_inner(&a[n2..])
}

/// 分片 RMS 电平（dBFS，-90 表示静音）。用于 UI 电平条与回声排查日志。
///
/// 与 Python `rms_db` 一致：float32 平方、numpy pairwise 均值、float32 开方，
/// 最后 `20*log10` 在 float64 中计算（返回值按契约收窄为 f32）。
pub fn rms_db(pcm16: &[u8]) -> f32 {
    if pcm16.is_empty() {
        return -90.0;
    }
    pcm16_len_check(pcm16);
    let squares: Vec<f32> = pcm16
        .chunks_exact(2)
        .map(|b| {
            let s = i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0f32;
            s * s
        })
        .collect();
    // np.mean：pairwise 求和后除以个数（f32 除法，n 可被 f32 精确表示）
    let mean = pairwise_sum(&squares) / squares.len() as f32;
    let rms = mean.sqrt(); // np.sqrt，IEEE 精确舍入
    if rms <= 0.0 {
        return -90.0;
    }
    let db = 20.0f64 * (rms as f64).log10();
    if db < -90.0 {
        -90.0
    } else {
        db as f32
    }
}

//! 时域 NLMS 回声消除（`o5/aec.py` 的忠实移植）。
//!
//! 问题背景：扬声器外放 TTS（24k，重采样后经 aplay 输出），麦克风 16k 单声道
//! 同期采集，扬声器声音被麦克风录回上行给语音服务器，服务器把 TTS 内容当
//! 用户语音（自言自语循环）。本模块在麦克风上行前用自适应滤波把回声从近端
//! 信号里减掉。
//!
//! 算法结构（时域 NLMS + 粗延迟预对齐 + 连续漂移补偿）：
//! 1. far-end 参考环形缓冲：`push_far` 持续写入扬声器写出流（默认 >= 1.5s 容量）。
//! 2. 延迟预对齐：far 与 near 之间存在未知固定延迟（声程 + aplay 缓冲，约
//!    50~300ms）。用 FFT 归一化互相关在 0~`max_delay_ms`（默认 1000ms）窗口
//!    内估计 far->near 延迟（抛物线内插细化到子样本），平时锁定不重算（互相关
//!    只在初始化和周期性重估时做，平时零开销）。估计出的"延迟"天然包含 aplay
//!    缓冲深度（写出提前量），数学上自洽。
//! 3. 时钟漂移补偿：mic 与播放属不同时钟域（~0.1% 相对漂移，延迟每秒漂
//!    ~1ms）。模块用周期性延迟重估（锁定后 ~3s 一次，之后每
//!    `delay_recheck_s`=10s 一次）估计漂移率（积分式修正），并在读取 far
//!    对齐窗时按漂移率做连续的分数样本平移（线性插值，块内也按
//!    (1-drift) 间距读取，而不是只在块边界平移）。这样滤波器看到的回声路径
//!    是（近似）静态的，两次重估之间的残余由 taps 窗口内的自适应跟踪，重估
//!    到新延迟后按整数样本平移滤波器权重（混响尾小于 taps 时平移不损失已
//!    收敛结果）。对齐窗锚点在估计延迟上回退 taps/4（默认 16ms）：互相关峰
//!    对多径色散远端有十几~几十样本的固有偏置，回退后直达径稳定落在 taps
//!    窗内（约 3/4 处），混响尾（<3/4 taps）也仍在窗内。
//! 4. NLMS 自适应滤波：子块（默认 4ms）聚合归一化 LMS 更新。为了达到单样本
//!    NLMS 的收敛速度，等效步长放大约 `step_gain` 倍，并配精确回溯线搜索保证
//!    每个子块残差能量单调不增（对任意信号稳定，不会发散）。
//! 5. 双端说话检测（DTD，简化版）：far 静音时不滤波不更新直接透传；深度收敛
//!    后（平滑 ERLE >= 10dB）双判据同时满足才冻结系数（并保持
//!    `dt_hangover_ms` 覆盖音节谷值），防止发散、防止啃话。收敛中途绝不冻结。
//! 6. 安全降级（任何异常状况都透传近端、绝不放大）：far 历史不足（欠载）、
//!    far 相对近端停推（欠载检测）、延迟无法锁定、延迟超出搜索窗、数值异常等。
//!
//! 使用约定（与 Python 版一致）：
//! - 时钟同启同停：麦克风停跑期间 far 也要冻结（由 audio_io 门控），否则空闲
//!   期 far 独自推进，两端位置差会累积进延迟估计。每次开麦前应 `reset()`
//!   重新同起点。
//! - far 推送节奏不要求平滑（合块 0.3~0.6s 一推没问题）：延迟估计与滤波窗口
//!   都锚定在近端位置（far 领先近端时按近端算），写入提前量不会进延迟估计；
//!   推送落后（真欠载）时按 far 实际位置读，读不到的段透传。
//! - 输入输出均为 16k mono PCM16。
//!
//! 数值口径（照 numpy 逐处对齐）：far/near 环形缓冲与滤波器系数 float32；
//! 互相关、窗能量前缀和、能量比值统计 float64；子块内积（`np.correlate` /
//! `@`）float32。

use std::fs::File;
use std::io::Write;
use std::path::Path;

use serde_json::json;

use crate::fft::{Rfft, RfftCache};

/// `bytes(s16le) -> float32[-1, 1]`：i16 / 32768（精确）。
fn decode_pcm(pcm: &[i16]) -> Vec<f32> {
    pcm.iter().map(|&s| s as f32 * (1.0f32 / 32768.0)).collect()
}

/// `float[-1,1] -> i16`：`np.round(clip(x)*32768)` 再夹 [-32768, 32767]。
///
/// `np.round` 是四舍六入五取偶（banker's rounding），全零/透传路径可精确往返。
fn encode_pcm(x: &[f32]) -> Vec<i16> {
    x.iter()
        .map(|&v| {
            let y = (v.clamp(-1.0, 1.0) * 32768.0f32).round_ties_even();
            y.clamp(-32768.0, 32767.0) as i16
        })
        .collect()
}

fn next_pow2(n: usize) -> usize {
    let mut p = 1usize;
    while p < n {
        p <<= 1;
    }
    p
}

/// Python `int(round(x))`（四舍六入五取偶后取整）。
fn round_usize(x: f64) -> usize {
    x.round_ties_even().max(0.0) as usize
}

/// Python `int(x)`（向零截断）。
fn trunc_usize(x: f64) -> usize {
    x.max(0.0) as usize
}

/// float32 点积（`np.dot` float32 语义：乘加都在 f32 域）。
fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = 0f32;
    for (&x, &y) in a.iter().zip(b) {
        acc += x * y;
    }
    acc
}

/// `np.correlate(a, v, "valid")`（实数、len(a) >= len(v)）：滑动窗点积。
///
/// `out[j] = sum_k a[j+k] * v[k]`，输出长度 `len(a) - len(v) + 1`。
fn correlate_valid_into(a: &[f32], v: &[f32], out: &mut [f32]) {
    debug_assert!(a.len() >= v.len());
    debug_assert_eq!(out.len(), a.len() - v.len() + 1);
    let t = v.len();
    for (j, o) in out.iter_mut().enumerate() {
        let row = &a[j..j + t];
        let mut acc = 0f32;
        for (&x, &y) in row.iter().zip(v) {
            acc += x * y;
        }
        *o = acc;
    }
}

/// 构造参数（与 Python `EchoCanceler.__init__` 一一对应，默认值相同）。
#[derive(Debug, Clone)]
pub struct NlmsAecParams {
    /// 采样率（far/near 一致），默认 16000。
    pub sample_rate: u32,
    /// NLMS 滤波器长度，默认 1024（64ms@16k：对齐残差 + 混响尾）。
    pub taps: usize,
    /// NLMS 步长，默认 0.5（内部有逐子块线搜索保底，不会发散）。
    pub mu: f64,
    /// 期望的 `cancel` 数据块长度（毫秒），用于内部缓冲余量，默认 100。
    pub adapt_block_ms: f64,
    /// 延迟搜索窗上限（默认 1000ms，配置 audio.echoMaxDelayMs）。
    /// 含 aplay 管道积压 + 设备内部缓冲 + 声学路径，合块播放下轻松超过 500ms。
    pub max_delay_ms: f64,
    /// 延迟估计用的互相关长度（默认 500ms）。
    pub corr_window_ms: f64,
    /// 延迟周期性重估间隔（默认 10s；锁定后第一次提前到 ~3s）。
    pub delay_recheck_s: f64,
    /// NLMS 子块更新粒度（默认 4ms；越小收敛越快、开销越大）。
    pub sub_block_ms: f64,
    /// 子块聚合更新的等效步长放大倍数（默认 32，带自适应回退 + 线搜索）。
    pub step_gain: f64,
    /// far 静音门限（RMS，默认 1e-3 = -60dBFS）。
    pub far_gate_rms: f64,
    /// 双端说话判定（能量判据）：近端能量超过回声预测能量的倍数（默认 1.5）。
    pub dt_near_ratio: f64,
    /// 双端说话判定（残差判据）：残差/近端能量比超过它（默认 0.35）。
    pub dt_residual_ratio: f64,
    /// 双端说话判据触发后的冻结保持时长（默认 300ms），覆盖语音音节包络的
    /// 谷值段（判据在谷值处会暂时不满足）。
    pub dt_hangover_ms: f64,
    /// 延迟估计的置信度门限（归一化互相关峰值），默认 0.25。
    pub min_correlation: f64,
    /// 锁定延迟后无条件自适应的时长（默认 500ms）。
    pub warmup_ms: f64,
    /// far 停推多久判定欠载（默认 900ms，需大于最大合块 0.6s + 余量）。
    pub staleness_ms: f64,
    /// 调试转储目录（audio.echoDebugDump）：far/near 两路 16k PCM 全量落盘。
    pub dump_dir: Option<std::path::PathBuf>,
}

impl Default for NlmsAecParams {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            taps: 1024,
            mu: 0.5,
            adapt_block_ms: 100.0,
            max_delay_ms: 1000.0,
            corr_window_ms: 500.0,
            delay_recheck_s: 10.0,
            sub_block_ms: 4.0,
            step_gain: 32.0,
            far_gate_rms: 1.0e-3,
            dt_near_ratio: 1.5,
            dt_residual_ratio: 0.35,
            dt_hangover_ms: 300.0,
            min_correlation: 0.25,
            warmup_ms: 500.0,
            staleness_ms: 900.0,
            dump_dir: None,
        }
    }
}

/// 时域 NLMS 回声消除器（纯数值计算，无平台依赖；`o5/aec.py` 1:1 移植）。
pub struct NlmsAec {
    // ---- 构造时定死的参数 ----
    sample_rate: u32,
    taps: usize,
    mu: f64,
    maxlag: usize,
    xlen: usize,
    min_corr_len: usize,
    sub: usize,
    gain_max: f64,
    gate2: f64,
    dt_near: f64,
    dt_res: f64,
    dt_hold_sub: usize,
    /// 对齐窗锚点回退量：滤波器窗覆盖 [est-anchor, est-anchor+taps]，直达径
    /// 落在窗内约 3/4 处，给互相关估计的多径偏置（可达十几~几十样本）与
    /// 重估抖动留出两侧余量，同时混响尾（<3/4 taps）仍在窗内。
    anchor_back: usize,
    min_corr: f64,
    warm_total: usize,
    recheck: usize,
    /// 锁定后首次重估尽量早（默认 1.5s）：首锁如果撞上伪峰，早点纠偏。
    first_recheck: usize,
    try_period: usize,
    stale_margin: usize,
    q_trigger: usize,
    erle_alpha: f64,
    py_alpha: f64,
    far_ring: Vec<f32>,
    near_ring: Vec<f32>,
    /// 单次 cancel 内分段处理的远端窗口上限（保证窗口 + taps 始终在环形缓冲内）。
    piece: usize,
    // ---- 运行状态 ----
    far_n: usize,
    near_n: usize,
    w: Vec<f32>,
    locked: bool,
    delay_f: f64,
    drift: f64,
    rate_ready: bool,
    t_snap: usize,
    warm: usize,
    far_updates: usize,
    erle_pow: Option<f64>,
    py_pow: Option<f64>,
    kappa: f64,
    dt_hold: usize,
    short_rechecks: i32,
    next_try: usize,
    next_recheck: usize,
    far_last_n: usize,
    far_adv_at: usize,
    was_stale: bool,
    warm_checked: bool,
    xdiag: String,
    next_fail_log: usize,
    // ---- FFT 计划缓存（互相关用；大小随历史长度增长后稳定） ----
    fft: RfftCache,
    // ---- 调试转储 ----
    dump_far: Option<File>,
    dump_near: Option<File>,
}

impl NlmsAec {
    /// 按参数构造（非法参数 panic，对应 Python 的 `ValueError`）。
    pub fn new(p: NlmsAecParams) -> Self {
        let sr = p.sample_rate as usize;
        if p.sample_rate < 8000 {
            panic!("sample_rate 需 >= 8000");
        }
        if !(0.0 < p.mu && p.mu <= 2.0) {
            panic!("mu 需在 (0, 2] 区间");
        }
        let taps = p.taps.max(8);
        let block_hint = round_usize(sr as f64 * p.adapt_block_ms / 1000.0).max(1);
        let maxlag = round_usize(sr as f64 * p.max_delay_ms / 1000.0).max(16);
        let xlen = round_usize(sr as f64 * p.corr_window_ms / 1000.0).max(256);
        // 互相关最小数据量：回声覆盖不足时的短窗相关全是噪声伪峰（AI 刚
        // 开口的前 ~0.3s），宁可晚一点锁也不能锁错——锁错后滤波在错误
        // 参考上输出（ERLE 为负），纠正要等重估/巡检。
        let min_corr_len = xlen.min(trunc_usize(0.3 * sr as f64).max(1024));
        let sub = round_usize(sr as f64 * p.sub_block_ms / 1000.0).max(32);
        let gain_max = p.step_gain.max(1.0);
        let gate2 = p.far_gate_rms * p.far_gate_rms;
        let dt_hold_sub = (round_usize(p.dt_hangover_ms * sr as f64 / 1000.0 / sub as f64))
            .max(1);
        let anchor_back = taps / 4;
        let warm_total = (round_usize(p.warmup_ms * sr as f64 / 1000.0 / sub as f64)).max(4);
        let recheck = round_usize(p.delay_recheck_s * sr as f64).max(trunc_usize(0.5 * sr as f64));
        let first_recheck = trunc_usize(1.5 * sr as f64)
            .min(trunc_usize(0.3 * recheck as f64));
        let try_period = trunc_usize(0.08 * sr as f64).max(1);
        let stale_margin = round_usize(sr as f64 * p.staleness_ms / 1000.0);
        let q_trigger = warm_total + trunc_usize(5.0 * sr as f64 / sub as f64);

        let far_len = ((1.5 * sr as f64).max(
            (maxlag + xlen + block_hint + taps) as f64 + 0.1 * sr as f64,
        )) as usize;
        let near_len = ((xlen + 2 * block_hint) as f64 + 0.25 * sr as f64) as usize;
        let piece = block_hint.max(far_len.saturating_sub(
            maxlag + taps + trunc_usize(0.05 * sr as f64),
        ));

        // 调试转储（audio.echoDebugDump）：far/near 两路 16k PCM 全量落盘，
        // 供离线分析延迟估计为什么锁不上（波形相关 vs 包络相关）。
        let (dump_far, dump_near) = match &p.dump_dir {
            Some(dir) => open_dump(dir),
            None => (None, None),
        };

        let mut aec = Self {
            sample_rate: p.sample_rate,
            taps,
            mu: p.mu,
            maxlag,
            xlen,
            min_corr_len,
            sub,
            gain_max,
            gate2,
            dt_near: p.dt_near_ratio,
            dt_res: p.dt_residual_ratio,
            dt_hold_sub,
            anchor_back,
            min_corr: p.min_correlation,
            warm_total,
            recheck,
            first_recheck,
            try_period,
            stale_margin,
            q_trigger,
            erle_alpha: 0.9,
            py_alpha: 0.9,
            far_ring: vec![0.0; far_len],
            near_ring: vec![0.0; near_len],
            piece,
            far_n: 0,
            near_n: 0,
            w: vec![0.0; taps],
            locked: false,
            delay_f: 0.0,
            drift: 0.0,
            rate_ready: false,
            t_snap: 0,
            warm: 0,
            far_updates: 0,
            erle_pow: None,
            py_pow: None,
            kappa: 0.0,
            dt_hold: 0,
            short_rechecks: 0,
            next_try: 0,
            next_recheck: 0,
            far_last_n: 0,
            far_adv_at: 0,
            was_stale: false,
            warm_checked: false,
            xdiag: String::new(),
            next_fail_log: 0,
            fft: RfftCache::new(),
            dump_far,
            dump_near,
        };
        aec.reset();
        aec
    }

    /// 扬声器侧参考信号（16k mono PCM16）。播放线程每写出一块就调用。
    ///
    /// 静音期间也要推全零块（保持 far 时钟推进）；断流会被判定为 far 欠载，
    /// 触发透传并在恢复后重新对齐。
    pub fn push_far(&mut self, far16k: &[i16]) {
        let a = decode_pcm(far16k);
        if a.is_empty() {
            return;
        }
        if let Some(f) = &mut self.dump_far {
            let _ = write_pcm16(f, far16k);
        }
        let r = self.far_ring.len();
        if a.len() >= r {
            self.far_ring.copy_from_slice(&a[a.len() - r..]);
        } else {
            let p = self.far_n % r;
            let k = r - p;
            if a.len() <= k {
                self.far_ring[p..p + a.len()].copy_from_slice(&a);
            } else {
                self.far_ring[p..].copy_from_slice(&a[..k]);
                self.far_ring[..a.len() - k].copy_from_slice(&a[k..]);
            }
        }
        self.far_n += a.len();
    }

    /// 麦克风信号 -> 消除回声后的残余（同长度）。
    ///
    /// 任意块长均可（推荐 100ms/1600 样本量级）。未锁定延迟 / far 静音或欠载
    /// 时返回的残余与输入逐样本一致（透传，绝不衰减近端语音）。
    pub fn cancel(&mut self, near: &[i16]) -> Vec<i16> {
        let d = decode_pcm(near);
        if d.is_empty() {
            return Vec::new();
        }
        if let Some(f) = &mut self.dump_near {
            let _ = write_pcm16(f, near);
        }
        let n = d.len();
        self.near_append(&d); // 近端历史入环（延迟估计要用），near_n 前进

        // -- far 欠载检测：far 停推超过 stale_margin（按近端时间计） --
        // 旧版用 gap 相对缓降基准的跌落幅度判定，会被合块写入的阶梯状
        // far 推进（扬声器 0.3~0.6s 写一块，far 同步跳一格）误触发：
        // 每个块间隙都判欠载 -> 透传 + 重新对齐，AEC 永远收敛不起来。
        // 改为只看"far 最后一次推进距今多久"，对推进节奏不敏感。
        if self.far_n != self.far_last_n {
            self.far_last_n = self.far_n;
            self.far_adv_at = self.near_n;
        }
        if self.near_n - self.far_adv_at > self.stale_margin {
            self.was_stale = true; // far 欠载：透传，冻结一切状态
            return encode_pcm(&d);
        }
        if self.was_stale {
            self.was_stale = false;
            self.realign(); // far 恢复：丢弃旧对齐，重新估计
        }

        self.maybe_update_delay();

        if !self.locked {
            return encode_pcm(&d); // 未锁定：透传（等 far/near 攒够历史）
        }

        self.delay_f += self.drift * n as f64; // 漂移补偿：对齐点连续前移

        let mut out = vec![0f32; n];
        // far 锚定在"近端位置"而不是 far 末梢：合块写入让 far_n 领先近端
        // （写入提前量随块大小锯齿摆动），锚 far_n 的话滤波窗口会跟着块
        // 节奏抖动、权重永远收敛不了。far 落后（真欠载）时仍用 far_n，
        // 读不到的段照旧透传。
        let f_abs = self.far_n.min(self.near_n);
        let mut pos = 0;
        while pos < n && self.locked {
            let m = self.piece.min(n - pos);
            if !self.cancel_piece(&d, &mut out, pos, m, n, f_abs) {
                // far 历史不足：本段透传
                out[pos..pos + m].copy_from_slice(&d[pos..pos + m]);
            }
            pos += m;
        }
        if pos < n {
            // 处理中途被解除锁定：剩余透传
            out[pos..].copy_from_slice(&d[pos..]);
        }

        if self.w.iter().any(|v| !v.is_finite()) {
            // 数值异常兜底
            self.w.fill(0.0);
            self.erle_pow = None;
            self.warm = self.warm_total;
            self.far_updates = 0;
        }

        // 首锁验证：预热刚完成时 ERLE 仍 < 3dB，说明首锁大概率撞上了
        // 伪峰（AI 刚开口、回声覆盖不足时短窗相关的固有风险）——立即
        // 重锁。此时回声覆盖已满，二次锁定必然对准真峰。只查一次。
        if self.locked && !self.warm_checked && self.far_updates >= self.warm_total {
            self.warm_checked = true;
            if self.erle_pow.is_none_or(|e| e < 2.0) {
                self.realign();
            }
        }
        // 收敛质量巡检：far 一直有能量却长期收敛不了 -> 延迟可能错锁，重新对齐
        if self.locked
            && self.far_updates >= self.q_trigger
            && self.erle_pow.is_none_or(|e| e < 1.0)
        {
            self.realign();
        }

        encode_pcm(&out)
    }

    /// 诊断信息：`{"erle_db", "delay_ms", "adapted", "locked", "far_s", "near_s"}`。
    ///
    /// - erle_db：远端活跃且回声占优时段内近端/残余能量比的平滑值（dB），无数据为 0。
    /// - delay_ms：当前锁定的 far->near 延迟（含 aplay 缓冲提前量，含漂移补偿），
    ///   未锁定为 0。
    /// - adapted：已锁定、完成预热且平滑 ERLE >= 6dB。
    /// - locked：延迟是否已锁定（排查用：locked=false 说明互相关没找到峰）。
    /// - far_s / near_s：累计喂入的远端/近端秒数（far_s 停长说明参考断了）。
    pub fn stats(&self) -> serde_json::Value {
        let sr = self.sample_rate as f64;
        let erle = match self.erle_pow {
            Some(e) if e > 0.0 => 10.0 * e.log10(),
            _ => 0.0,
        };
        let delay_ms = if self.locked {
            self.delay_f * 1000.0 / sr
        } else {
            0.0
        };
        let adapted = self.locked
            && self.far_updates >= self.warm_total
            && self.erle_pow.is_some_and(|e| e >= 4.0);
        json!({
            "erle_db": erle,
            "delay_ms": delay_ms,
            "adapted": adapted,
            "locked": self.locked,
            "far_s": self.far_n as f64 / sr,
            "near_s": self.near_n as f64 / sr,
        })
    }

    /// 时钟计数 (far 累计样本, near 累计样本)——far 时钟伺服估计速率差用。
    pub fn clocks(&self) -> (u64, u64) {
        (self.far_n as u64, self.near_n as u64)
    }

    /// 清空全部状态（缓冲、系数、延迟锁定、漂移估计、统计）。
    pub fn reset(&mut self) {
        self.far_ring.fill(0.0);
        self.near_ring.fill(0.0);
        self.far_n = 0;
        self.near_n = 0;
        self.w.fill(0.0);
        self.locked = false;
        self.delay_f = 0.0;
        self.drift = 0.0;
        self.rate_ready = false;
        self.t_snap = 0;
        self.warm = self.warm_total;
        self.far_updates = 0;
        self.erle_pow = None;
        self.py_pow = None;
        self.kappa = self.gain_max.min(8.0);
        self.dt_hold = 0;
        self.short_rechecks = 2;
        self.next_try = 0;
        self.next_recheck = 0;
        self.far_last_n = 0;
        self.far_adv_at = 0;
        self.was_stale = false;
        self.warm_checked = false;
        self.xdiag = "未尝试".to_string();
        self.next_fail_log = 0;
    }

    // ---------------- 内部：缓冲读写 ----------------

    fn near_append(&mut self, a: &[f32]) {
        let r = self.near_ring.len();
        if a.len() >= r {
            self.near_ring.copy_from_slice(&a[a.len() - r..]);
        } else {
            let p = self.near_n % r;
            let k = r - p;
            if a.len() <= k {
                self.near_ring[p..p + a.len()].copy_from_slice(a);
            } else {
                self.near_ring[p..].copy_from_slice(&a[..k]);
                self.near_ring[..a.len() - k].copy_from_slice(&a[k..]);
            }
        }
        self.near_n += a.len();
    }

    /// 读绝对区间 `[start, start+length)` 的 far 数据；越界/已覆盖返回 None。
    fn far_read(&self, start: i64, length: usize) -> Option<Vec<f32>> {
        if length == 0 || start < 0 {
            return None;
        }
        let r = self.far_ring.len() as i64;
        let far_n = self.far_n as i64;
        if start < far_n - r || start + length as i64 > far_n {
            return None;
        }
        let p = start % r;
        let p = p as usize;
        if p + length <= r as usize {
            Some(self.far_ring[p..p + length].to_vec())
        } else {
            let mut out = Vec::with_capacity(length);
            let k = r as usize - p;
            out.extend_from_slice(&self.far_ring[p..]);
            out.extend_from_slice(&self.far_ring[..length - k]);
            Some(out)
        }
    }

    /// 从分数位置 `start_f` 起按相邻样本间距 `slope` 读 `length` 个 far 样本。
    ///
    /// 线性插值。slope = 1 - drift：时钟漂移补偿——远端时钟快时（drift>0），
    /// 回声路径在 far 上的推进速度是 (1-drift) 样本/近端样本，块内也要按该
    /// 间距插值读取（而不是只在块边界平移一次），滤波器才能看到静态路径。
    fn far_read_frac(&self, start_f: f64, length: usize, slope: f64) -> Option<Vec<f32>> {
        if (slope - 1.0).abs() <= 1.0e-9 {
            let base = start_f.floor();
            let frac = start_f - base;
            let raw = self.far_read(base as i64, length + 1)?;
            if frac < 1.0e-6 {
                return Some(raw[..length].to_vec());
            }
            // np.interp(j + frac, arange(length+1), raw)：
            // raw[j] + (raw[j+1]-raw[j]) * frac
            let mut out = Vec::with_capacity(length);
            for j in 0..length {
                let a = raw[j] as f64;
                let b = raw[j + 1] as f64;
                out.push((a + (b - a) * frac) as f32);
            }
            Some(out)
        } else {
            let pos_last = start_f + slope * (length - 1) as f64;
            let lo = start_f.floor();
            let hi = pos_last.floor() + 1.0;
            let raw = self.far_read(lo as i64, (hi - lo) as usize + 1)?;
            let mut out = Vec::with_capacity(length);
            for j in 0..length {
                let x = start_f + slope * j as f64;
                // x ∈ [lo, hi]（格点整数），np.interp 线性内插
                let i = x.floor() as i64 - lo as i64;
                let a = raw[i as usize] as f64;
                let b = raw[i as usize + 1] as f64;
                out.push((a + (b - a) * (x - x.floor())) as f32);
            }
            Some(out)
        }
    }

    fn near_tail(&self, length: usize, end: usize) -> Option<Vec<f32>> {
        if length == 0 || end < length || end > self.near_n {
            return None;
        }
        let r = self.near_ring.len();
        if length > r {
            return None;
        }
        let p = (end - length) % r;
        if p + length <= r {
            Some(self.near_ring[p..p + length].to_vec())
        } else {
            let mut out = Vec::with_capacity(length);
            let k = r - p;
            out.extend_from_slice(&self.near_ring[p..]);
            out.extend_from_slice(&self.near_ring[..length - k]);
            Some(out)
        }
    }

    // ---------------- 内部：延迟估计与维护 ----------------

    fn maybe_update_delay(&mut self) {
        let near_end = self.near_n;
        if !self.locked {
            if near_end < self.next_try {
                return;
            }
            self.next_try = near_end + self.try_period;
            let est = self.xcorr_delay(near_end);
            match est {
                Some((delay, conf)) if conf >= self.min_corr => {
                    self.delay_f = delay.max(4.0);
                    self.locked = true;
                    log::info!(
                        "[aec] 延迟锁定 {:.0}ms（conf={:.2}）",
                        self.delay_f / self.sample_rate as f64 * 1000.0,
                        conf
                    );
                }
                _ => {
                    if near_end >= self.next_fail_log {
                        // 每 ~2s 输出一次未锁原因（现场排查卡点）
                        self.next_fail_log = near_end + 2 * self.sample_rate as usize;
                        log::info!("[aec] 锁定尝试未通过：{}", self.xdiag);
                        self.drift = 0.0;
                        self.rate_ready = false;
                        self.t_snap = near_end;
                        self.warm = self.warm_total;
                        self.far_updates = 0;
                        self.erle_pow = None;
                        self.py_pow = None;
                        self.next_recheck = near_end + self.first_recheck;
                        self.short_rechecks = 2;
                    }
                }
            }
            return;
        }
        if near_end >= self.next_recheck {
            // 锁定后前两次重估用短间隔（默认 1.5s）：漂移率是积分修正的，
            // 早点收敛到稳态精度；之后回到常规间隔（10s）。
            if self.short_rechecks > 0 {
                self.short_rechecks -= 1;
                self.next_recheck = near_end + self.first_recheck;
            } else {
                self.next_recheck = near_end + self.recheck;
            }
            let est = self.xcorr_delay(near_end);
            let (est_f, _conf) = match est {
                Some((d, c)) if c >= self.min_corr => (d.max(4.0), c),
                _ => return,
            };
            // 已收敛的锁拒绝大幅跳变：重估撞上"AI 刚停说、回声覆盖萎缩"
            // 或音节间隙的伪峰时（部分覆盖窗的归一化伪峰能险胜），别把
            // 好锁拉坏。真延迟剧变（播放通道重建等）时 ERLE 会先崩，
            // 由收敛质量巡检 -> 重对齐兜底，不受此保护影响。
            if self.erle_pow.is_some_and(|e| e >= 4.0)
                && (est_f - self.delay_f).abs() > 0.3 * self.maxlag as f64
            {
                return;
            }
            // -- 漂移率估计：本次互相关延迟相对当前对齐点的偏差/跨度 --
            let span = near_end - self.t_snap;
            if span >= self.try_period {
                let rate = (est_f - self.delay_f) / span as f64;
                let rate = rate.clamp(-0.01, 0.01);
                if !self.rate_ready {
                    self.drift = rate;
                    self.rate_ready = true;
                } else {
                    // rate 是"当前漂移估计还差多少"（误差信号），要积分修正
                    // 而不是 EMA 靠向它（EMA 的不动点是真值的一半）。
                    self.drift = (self.drift + 0.5 * rate).clamp(-0.01, 0.01);
                }
            }
            // -- 对齐点跳到新估计，权重按整数平移保持输出连续 --
            let delta = est_f - self.delay_f;
            let shift = delta.round_ties_even() as i64;
            if shift.unsigned_abs() >= self.taps as u64 {
                // 漂移/变化超出滤波器窗口：重置系数重新收敛
                self.w.fill(0.0);
                self.warm = self.warm_total;
                self.far_updates = 0;
                self.erle_pow = None;
            } else if shift != 0 {
                self.shift_weights(shift as i32);
            }
            self.delay_f = est_f;
            self.t_snap = near_end;
        }
    }

    /// FFT 归一化互相关估计 far->near 延迟（子样本精度）。
    ///
    /// 返回 `Some((delay_samples, conf))`。近端取最近 L 个样本，与 far 尾部
    /// 窗口 `[far_n-W, far_n)`（W = L+maxlag）做逐延迟能量归一化的互相关，
    /// 直接径主导时峰即延迟；抛物线内插把峰细化到子样本。只在初始化/周期
    /// 重估时调用。
    fn xcorr_delay(&mut self, near_end: usize) -> Option<(f64, f64)> {
        let sr = self.sample_rate as usize;
        let l = self.xlen.min(near_end);
        let maxlag = self.maxlag;
        // 窗口锚定在近端位置（far 领先时取 near_end），与滤波同一个锚：
        // 若锚 far 末梢，合块写入的提前量会被直接算进延迟估计（每次重估
        // 落在不同的摆动相位上，延迟估计来回跳）。
        let tip = self.far_n.min(near_end);
        if l < self.min_corr_len || tip < l + maxlag {
            self.xdiag = format!(
                "历史不足 near={:.1}s far={:.1}s",
                near_end as f64 / sr as f64,
                self.far_n as f64 / sr as f64
            );
            return None;
        }
        let wlen = l + maxlag;
        let far = match self.far_read(tip as i64 - wlen as i64, wlen) {
            Some(v) => v,
            None => {
                self.xdiag = "环形缓冲读取失败".into();
                return None;
            }
        };
        let near = match self.near_tail(l, near_end) {
            Some(v) => v,
            None => {
                self.xdiag = "环形缓冲读取失败".into();
                return None;
            }
        };
        let x: Vec<f64> = near.iter().map(|&v| v as f64).collect();
        let y: Vec<f64> = far.iter().map(|&v| v as f64).collect();
        let ex = x.iter().map(|&v| v * v).sum::<f64>();
        if ex <= 1.0e-10 {
            self.xdiag = "近端全静音".into();
            return None;
        }
        let ey = y.iter().map(|&v| v * v).sum::<f64>();
        if ey <= 1.0e-10 {
            self.xdiag = "far 全静音".into(); // far 全静音：没有回声可对齐，省掉这轮 FFT
            return None;
        }
        // 近端活跃门：末端 0.15s 必须有能量（回声是"现在进行时"，AI 停了
        // 以后不该再触发锁定）；活跃占比 >= 35%（占比太低时部分重叠窗的
        // 归一化伪峰能险胜真峰）。首锁万一仍撞伪峰，由 cancel() 里的
        // 首锁验证在预热结束时兜底重锁。
        let act_tail = act_bools(&x, self.gate2);
        let tail_n = trunc_usize(0.15 * sr as f64);
        let s0 = act_tail.len().saturating_sub(tail_n);
        if !act_tail[s0..].iter().any(|&b| b) {
            self.xdiag = "近端末尾 0.15s 静音（AI 停说后不该再锁）".into();
            return None;
        }
        let act_sum = act_tail.iter().filter(|&&b| b).count();
        if (act_sum as f64) < 0.35 * act_tail.len() as f64 {
            self.xdiag =
                format!("近端活跃占比低 {:.0}%", act_sum as f64 / act_tail.len() as f64 * 100.0);
            return None;
        }
        // y² 前缀和：每个延迟上 far 的局部能量，以及 far 尾部能量门
        let mut cy = Vec::with_capacity(wlen + 1);
        cy.push(0.0);
        let mut run = 0.0;
        for &v in &y {
            run += v * v;
            cy.push(run);
        }
        // 回声源必然是近期的 far：靠 tip 一侧没有能量（AI 已停一会儿）就
        // 不做估计——否则用户语音与窗内更早的 far 语音做包络级偶然相关，
        // 会假锁定一个毫无意义的延迟，再被周期重估扩散污染。
        let tail = trunc_usize(0.5 * sr as f64);
        if wlen > tail && cy[wlen] - cy[wlen - tail] <= self.gate2 * tail as f64 {
            self.xdiag = "far 尾 0.5s 静音（AI 未在说话）".into();
            return None;
        }
        // r[p] = sum_k x[k] * y[k+p]（相关峰 p = maxlag - delay）
        let nfft = next_pow2(wlen + 1);
        let mut r = vec![0.0; maxlag + 1];
        {
            let plan: &Rfft = self.fft.get(nfft);
            plan.xcorr(&x, &y, &mut r);
        }
        // 归一化互相关 + 找峰（argmax 首个最大）
        let mut nc = vec![0.0f64; maxlag + 1];
        let mut p = 0usize;
        let mut best = f64::NEG_INFINITY;
        for idx in 0..=maxlag {
            let efar = cy[idx + l] - cy[idx];
            let den = (ex * efar).max(1.0e-30).sqrt();
            let v = r[idx] / den;
            nc[idx] = v;
            if v > best {
                best = v;
                p = idx;
            }
        }
        let conf = nc[p];
        self.xdiag = format!(
            "conf={:.2} 延迟={:.0}ms",
            conf,
            (maxlag - p) as f64 / sr as f64 * 1000.0
        );
        let mut pf = p as f64;
        if p >= 1 && p < maxlag {
            // 抛物线子样本细化
            let (aa, bb, cc) = (nc[p - 1], nc[p], nc[p + 1]);
            let den2 = aa - 2.0 * bb + cc;
            if den2.abs() > 1.0e-12 {
                pf = p as f64 + 0.5 * ((aa - cc) / den2).clamp(-1.0, 1.0);
            }
        }
        Some((maxlag as f64 - pf, conf))
    }

    /// 延迟修正 delta 个样本时同步平移滤波器权重，保持输出连续。
    ///
    /// 新窗口起点比旧窗口早 delta（delta>0 时）：`w_new[i] = w_old[i - delta]`。
    /// 混响尾短于 taps 时丢掉的尾部系数接近 0，平移近似无损。
    fn shift_weights(&mut self, delta: i32) {
        let t = self.taps;
        if delta > 0 {
            let d = delta as usize;
            self.w.copy_within(..t - d, d);
            self.w[..d].fill(0.0);
        } else {
            let d = (-delta) as usize;
            self.w.copy_within(d.., 0);
            self.w[t - d..].fill(0.0);
        }
    }

    /// 丢弃当前对齐与收敛状态，回到透传并等待重新锁定的干净路径。
    fn realign(&mut self) {
        self.locked = false;
        self.delay_f = 0.0;
        self.drift = 0.0;
        self.rate_ready = false;
        self.t_snap = 0;
        self.warm = self.warm_total;
        self.far_updates = 0;
        self.erle_pow = None;
        self.py_pow = None;
        self.dt_hold = 0;
        self.next_try = 0;
        self.warm_checked = false;
    }

    // ---------------- 内部：NLMS 滤波与更新 ----------------

    /// 处理本块内 `[pos, pos+m)` 的近端样本。返回 false 表示 far 历史不足
    /// （本段已透传）。
    fn cancel_piece(&mut self, d: &[f32], out: &mut [f32], pos: usize, m: usize, n: usize, f_abs: usize) -> bool {
        let taps = self.taps;
        let drift = self.drift;
        // 本段第 j 行对应的 far 滑窗（长 taps）整体覆盖 seg（长 m + taps - 1）。
        // 近端绝对序号 q 的滑窗末端 = F - (N_end-1-q) - (delay_f - anchor_back)。
        // delay_f 是块末值：块内更早的样本延迟更小（drift>0 时），读取位置按
        // drift*(n-1-pos-j) 向新端修正 + 相邻行间距 (1-drift)，即块内连续分数
        // 漂移补偿（消除块边界的锯齿状相位失配）。
        let seg_start = f_abs as f64 - (self.delay_f - self.anchor_back as f64)
            - (n - 1 - pos) as f64
            - (taps - 1) as f64
            + drift * (n - 1 - pos) as f64;
        let seg = match self.far_read_frac(seg_start, m + taps - 1, 1.0 - drift) {
            Some(v) => v,
            None => return false,
        };
        // 窗能量前缀和（f64）
        let mut cs = Vec::with_capacity(m + taps);
        cs.push(0.0);
        let mut run = 0.0;
        for &v in &seg {
            run += v as f64 * v as f64;
            cs.push(run);
        }
        let mut y = vec![0f32; self.sub];
        let mut er = vec![0f32; self.sub];
        let mut g = vec![0f32; self.taps];
        let mut r = vec![0f32; self.sub];
        let mut s = 0usize;
        while s < m {
            let e1 = m.min(s + self.sub);
            let mm = e1 - s;
            let ssub = &seg[s..e1 - 1 + taps]; // 长 M + taps - 1
            let ds = &d[pos + s..pos + e1];
            // Ps = Σ_j ||x_j||²
            let mut ps = 0.0;
            for j in s..e1 {
                ps += cs[j + taps] - cs[j];
            }
            if ps <= self.gate2 * mm as f64 * taps as f64 {
                // far 静音：透传
                out[pos + s..pos + e1].copy_from_slice(ds);
                s = e1;
                continue;
            }
            y.resize(mm, 0.0);
            correlate_valid_into(ssub, &self.w, &mut y); // 回声估计 y = X @ w
            er.resize(mm, 0.0);
            for j in 0..mm {
                er[j] = ds[j] - y[j]; // 残差
            }
            let pd = dot_f32(ds, ds) as f64;
            if pd <= self.gate2 * mm as f64 {
                // 近端静音：透传
                out[pos + s..pos + e1].copy_from_slice(ds);
                s = e1;
                continue;
            }
            let py_ = dot_f32(&y, &y) as f64;
            let py = self.py_pow;
            self.py_pow = Some(match py {
                None => py_,
                Some(v) => self.py_alpha * v + (1.0 - self.py_alpha) * py_,
            });
            let pe = dot_f32(&er, &er) as f64;
            self.far_updates += 1;
            // DTD（简化版，双判据 + hangover，仅在深度收敛后武装）：
            // a) 能量判据：近端能量明显超过回声预测能量（近端以语音为主）；
            // b) 残差判据：残差占近端能量比例高（消不掉的"多出来的能量"）。
            // 只有平滑 ERLE >= 10dB（回声模型已可靠）判据才有区分度：收敛中途的
            // 滤波器 py 偏小、Pe 偏大，两判据都会误触发（把"没学好"误读成
            // "双端说话"而卡死）。漂移/路径渐变是相位失配（能量不变），同样
            // 不会在 10dB 收敛下触发。判据触发后保持 dt_hangover_ms 的冻结
            // （音节包络谷值处判据会暂时不满足，不代表双端说话结束了）。
            let converged = self.erle_pow.is_some_and(|e| e >= 10.0);
            if self.warm == 0
                && converged
                && py.is_some()
                && pd > self.dt_near * py.unwrap()
                && pe > self.dt_res * pd
            {
                self.dt_hold = self.dt_hold_sub;
            }
            let freeze = self.dt_hold > 0;
            if freeze {
                self.dt_hold -= 1;
            }
            if !freeze {
                g.resize(taps, 0.0);
                correlate_valid_into(ssub, &er, &mut g); // g = X^T @ e（长 taps）
                // 子块聚合步长放大 kappa 倍（逼近单样本 NLMS 的收敛速度），
                // 回溯线搜索保证该子块残差能量不增：不稳定信号自动收缩步长。
                let mut alpha = self.mu * self.kappa / (ps * 1.001 + 1.0e-15);
                r.resize(mm, 0.0);
                correlate_valid_into(ssub, &g, &mut r); // r = X @ g
                let mut halves = 0u32;
                for _ in 0..8 {
                    // diff = er - alpha*r（f32 域），能量与 Pe 比较
                    let mut e2 = 0f32;
                    for j in 0..mm {
                        let dv = er[j] - alpha as f32 * r[j];
                        e2 += dv * dv;
                    }
                    if e2 as f64 <= pe * 0.999 || alpha < 1.0e-18 {
                        break;
                    }
                    alpha *= 0.5;
                    halves += 1;
                }
                if halves == 0 {
                    // 本步干净：缓慢提升等效步长
                    self.kappa = (self.kappa * 1.05).min(self.gain_max);
                } else {
                    // 需要收缩：快速下调，避免持续恶化
                    self.kappa = (self.kappa * 0.5).max(1.0);
                }
                if halves < 8 {
                    let af = alpha as f32;
                    for (wk, gk) in self.w.iter_mut().zip(g.iter()) {
                        *wk += af * *gk;
                    }
                }
                if self.warm > 0 {
                    self.warm -= 1;
                }
                let mut ratio = if pe > 1.0e-15 { pd / pe } else { 1.0e4 };
                if ratio > 1.0e4 {
                    ratio = 1.0e4;
                }
                let ep = self.erle_pow;
                self.erle_pow = Some(match ep {
                    None => ratio,
                    Some(v) => self.erle_alpha * v + (1.0 - self.erle_alpha) * ratio,
                });
            }
            out[pos + s..pos + e1].copy_from_slice(&er[..mm]);
            s = e1;
        }
        true
    }
}

/// `x*x > gate2` 的布尔数组（近端活跃判定）。
fn act_bools(x: &[f64], gate2: f64) -> Vec<bool> {
    x.iter().map(|&v| v * v > gate2).collect()
}

/// 打开调试转储（far.pcm / near.pcm）。失败仅告警（不影响主流程）。
fn open_dump(dir: &Path) -> (Option<File>, Option<File>) {
    let mk = |name: &str| -> Option<File> {
        match std::fs::create_dir_all(dir) {
            Ok(()) => match File::create(dir.join(name)) {
                Ok(f) => Some(f),
                Err(e) => {
                    log::warn!("AEC 调试转储 {} 创建失败：{}", name, e);
                    None
                }
            },
            Err(e) => {
                log::warn!("AEC 调试转储目录 {} 创建失败：{}", dir.display(), e);
                None
            }
        }
    };
    let far = mk("far.pcm");
    let near = mk("near.pcm");
    if far.is_some() && near.is_some() {
        log::info!("AEC 调试转储: {}", dir.display());
    }
    (far, near)
}

fn write_pcm16(f: &mut File, pcm: &[i16]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(pcm.len() * 2);
    for &s in pcm {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    f.write_all(&buf)
}

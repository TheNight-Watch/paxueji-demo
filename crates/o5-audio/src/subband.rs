//! 子带包络回声抑制：真机链路上线性 AEC 的替代方案（`o5/subband_aec.py` 忠实移植）。
//!
//! 真机实测（02，双路 PCM 离线分析）：
//! - far（TTS 播放参考）与 near（麦克风采回）在波形层面零相关——
//!   最优线性滤波器的 LS 上限 ERLE = 0dB。扬声器非线性 + 麦克风 AGC
//!   （时变增益破坏线性卷积模型）让任何线性方法（时域 NLMS / 频域线性）无解。
//! - 但能量包络经全部失真仍强相关（段对段包络相关 conf 0.63~0.86）。
//!
//! 因此本模块两层：
//! 1. 延迟锁定：far 活动段 vs near 窗的 20ms RMS 包络互相关（50Hz 域），
//!    估计绝对延迟 D（near 位置 - far 位置）。近端尾窗整体相关因语音音节
//!    节奏产生多峰（次峰比 0.9+），段对段剪裁后次峰比 ~0。
//! 2. 回声抑制：STFT（32ms 帧 / 16ms hop）子带过减法。
//!    每个频带学回声传递增益 G[b] = |N|/|F|（下界跟踪：近端独立语音只会
//!    抬高比值，min 跟踪天然抗 double-talk；缓慢上爬适应音量变化）。
//!    far 活动帧对 near 谱做 (|N|-over*G|F|)/|N|，floor 即最大抑制深度。
//!    far 静音帧完全不碰（g=1，人声无损）。
//!
//! 接口与 [`crate::NlmsAec`] 对齐（push_far / cancel / reset / stats）。
//! 差异：`cancel` 返回"输出流中已就绪"的样本，长度可短于输入（STFT 有
//! frame-hop=16ms 群延迟；长期速率守恒，下游按实际长度消费即可）。

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;

use rustfft::num_complex::Complex;
use serde_json::json;

use crate::fft::{Rfft, RfftCache};

const PCM_MAX: f32 = 32768.0;

fn decode_pcm(b: &[i16]) -> Vec<f32> {
    b.iter().map(|&s| s as f32 / PCM_MAX).collect()
}

/// `(np.clip(a,-1,1) * 32767.0).astype("<i2")`：向零截断（astype 的 C 语义）。
fn encode_pcm(a: &[f32]) -> Vec<i16> {
    a.iter()
        .map(|&v| ((v.clamp(-1.0, 1.0) * 32767.0f32) as i32) as i16)
        .collect()
}

/// Python `int(x)`（向零截断）。
fn trunc_usize(x: f64) -> usize {
    x.max(0.0) as usize
}

/// 构造参数（与 Python `SubbandSuppressor.__init__` 一一对应，默认值相同）。
#[derive(Debug, Clone)]
pub struct SubbandParams {
    pub sample_rate: u32,
    /// 延迟搜索窗上限（默认 1000ms）。
    pub max_delay_ms: f64,
    /// STFT 帧长（毫秒，取整到 2 的幂），默认 32。
    pub frame_ms: f64,
    /// STFT 帧移（毫秒），默认 16（须为 frame/2：OLA 恒定增益）。
    pub hop_ms: f64,
    /// 频带数（~100Hz~Nyquist 对数分布），默认 20。
    pub bands: usize,
    /// 最大抑制深度（≈-24dB），默认 0.06。
    pub floor: f64,
    /// 过减因子，默认 2.2。
    pub over: f64,
    /// far 帧活动门限（RMS），默认 1e-3。
    pub gate_rms: f64,
    /// far 停止后的尾沿浅抑制（ms），默认 350。
    pub hangover_ms: f64,
    /// G 滑动分位数（中位：错位帧鲁棒），默认 0.50。
    pub g_quantile: f64,
    /// 包络锁定置信门限（次峰比双条件防伪峰），默认 0.35。
    pub min_corr: f64,
    /// 包络锁定次峰比上限（配投票），默认 0.75。
    pub max_sec_corr: f64,
    /// far 停推多久判定欠载（默认 900ms）。
    pub stale_ms: f64,
    /// 调试转储目录。
    pub dump_dir: Option<std::path::PathBuf>,
}

impl Default for SubbandParams {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            max_delay_ms: 1000.0,
            frame_ms: 32.0,
            hop_ms: 16.0,
            bands: 20,
            floor: 0.06,
            over: 2.2,
            gate_rms: 1.0e-3,
            hangover_ms: 350.0,
            g_quantile: 0.50,
            min_corr: 0.35,
            max_sec_corr: 0.75,
            stale_ms: 900.0,
            dump_dir: None,
        }
    }
}

/// STFT 子带回声抑制（+ 包络延迟锁定）。参数默认偏保守：先不啃话。
pub struct SubbandSuppressor {
    sample_rate: u32,
    maxlag: usize,
    /// 20ms 包络窗。
    w5: usize,
    frame: usize,
    hop: usize,
    floor: f64,
    over: f64,
    /// far 帧能量门限 = gate_rms² * frame。
    gate: f64,
    hangover: usize,
    g_q: f64,
    min_corr: f64,
    // max_sec_corr 只作参数保留（与 Python 一致：锁定决策走连续投票，不读它）
    #[allow(dead_code)]
    max_sec_corr: f64,
    stale_margin: usize,
    /// STFT 窗（Hann，hop=frame/2 时 OLA 恒定增益 1）。np.hanning 是
    /// `0.5 + 0.5*cos(pi*(2k-(M-1))/(M-1))`，f64 算完转 f32。
    win: Vec<f32>,
    /// 频带边界（bin 下标，首 0 尾 nbin）。
    band_edges: Vec<usize>,
    nb: usize,
    /// far 带能量学习门限：帧能量门限按带宽占比分摊。
    band_frac: Vec<f64>,
    // 环形缓冲：far 回看 2.5s（活动段检测）+ D + 帧；near 对应窗
    far_ring: Vec<f32>,
    near_ring: Vec<f32>,
    far_n: usize,
    near_n: usize,
    far_last_n: usize,
    far_adv_at: usize,
    // 延迟锁定状态
    locked: bool,
    /// 绝对延迟 D（near - far），样本。
    delay: i64,
    next_try: usize,
    next_recheck: usize,
    diag: String,
    /// 延迟锁定投票缓存。
    cand: Vec<f64>,
    /// 锁定后大跳变投票缓存。
    jump: Vec<f64>,
    // STFT 流式状态
    /// 待入帧尾部（稳态 < frame）。
    in_buf: VecDeque<f32>,
    /// OLA 待定段（稳态 = hop）。
    pend: Vec<f32>,
    /// 输出流队列。
    out_ready: VecDeque<f32>,
    g_val: Vec<f32>,
    g_seen: Vec<bool>,
    /// 比值历史深度（far 帧）。
    gh: usize,
    ghist: Vec<f32>,
    ghist_n: usize,
    /// 平滑后的 far/near 带能量。
    ef_s: Option<Vec<f64>>,
    en_s: Option<Vec<f64>>,
    far_hang: usize,
    pow_in: f64,
    pow_out: f64,
    far_frames: usize,
    #[allow(dead_code)] // 照 Python 版保留（更新不读）
    suppressed_frames: usize,
    fft: RfftCache,
    dump_far: Option<File>,
    dump_near: Option<File>,
}

impl SubbandSuppressor {
    /// 按参数构造（hop 必须为 frame/2，否则 panic，对应 Python 的 assert）。
    pub fn new(p: SubbandParams) -> Self {
        let sr = p.sample_rate as usize;
        let maxlag = (sr as f64 * p.max_delay_ms / 1000.0).round_ties_even() as usize;
        let w5 = trunc_usize(0.02 * sr as f64).max(160);
        let frame = (1usize << (sr as f64 * p.frame_ms / 1000.0).log2().round_ties_even() as u32)
            .max(64);
        let hop = (sr as f64 * p.hop_ms / 1000.0).round_ties_even() as usize;
        let hop = hop.max(16);
        assert!(frame == 2 * hop, "hop 需为 frame/2（OLA 恒定增益）");
        let gate = p.gate_rms * p.gate_rms * frame as f64;
        let hangover =
            (p.hangover_ms * sr as f64 / 1000.0 / hop as f64).round_ties_even() as usize;
        let hangover = hangover.max(1);
        let stale_margin = (sr as f64 * p.stale_ms / 1000.0).round_ties_even() as usize;

        // np.hanning(M) = 0.5 + 0.5*cos(pi*n/(M-1))，n = 2k-(M-1)
        let win: Vec<f32> = (0..frame)
            .map(|k| {
                let n = 2.0 * k as f64 - (frame - 1) as f64;
                0.5 + 0.5 * (std::f64::consts::PI * n / (frame - 1) as f64).cos()
            })
            .map(|v| v as f32)
            .collect();

        // 频带边界（~100Hz ~ Nyquist 对数分布，并到 bin）：
        // geomspace(100, sr/2, bands+1) = logspace 两端线性，末点精确对齐 stop
        let nbin = frame / 2 + 1;
        let la = (100.0f64).log10();
        let lb = (sr as f64 / 2.0).log10();
        let mut freqs = Vec::with_capacity(p.bands + 1);
        for k in 0..=p.bands {
            let v = if k == p.bands {
                sr as f64 / 2.0
            } else {
                10f64.powf(la + (lb - la) * k as f64 / p.bands as f64)
            };
            freqs.push(v);
        }
        let bin_hz = sr as f64 / frame as f64;
        let mut edges: Vec<usize> =
            freqs.iter().map(|&f| (f / bin_hz).round_ties_even() as usize).collect();
        edges.sort_unstable();
        edges.dedup();
        edges[0] = 0;
        let last = edges.len() - 1;
        edges[last] = nbin;
        let nb = (edges.len() - 1).max(1);
        let band_frac: Vec<f64> = edges
            .windows(2)
            .map(|w| (w[1] - w[0]) as f64 / (nbin - 1) as f64)
            .collect();

        let (dump_far, dump_near) = match &p.dump_dir {
            Some(dir) => open_dump(dir),
            None => (None, None),
        };

        let gh = 48usize;
        let mut s = Self {
            sample_rate: p.sample_rate,
            maxlag,
            w5,
            frame,
            hop,
            floor: p.floor,
            over: p.over,
            gate,
            hangover,
            g_q: p.g_quantile,
            min_corr: p.min_corr,
            max_sec_corr: p.max_sec_corr,
            stale_margin,
            win,
            band_edges: edges,
            nb,
            band_frac,
            far_ring: vec![0.0; (4.2 * sr as f64) as usize],
            near_ring: vec![0.0; (4.0 * sr as f64) as usize],
            far_n: 0,
            near_n: 0,
            far_last_n: 0,
            far_adv_at: 0,
            locked: false,
            delay: 0,
            next_try: 0,
            next_recheck: 0,
            diag: "未尝试".to_string(),
            cand: Vec::new(),
            jump: Vec::new(),
            in_buf: VecDeque::new(),
            pend: Vec::new(),
            out_ready: VecDeque::new(),
            g_val: vec![0.0; nb],
            g_seen: vec![false; nb],
            gh,
            ghist: vec![f32::NAN; nb * gh],
            ghist_n: 0,
            ef_s: None,
            en_s: None,
            far_hang: 0,
            pow_in: 0.0,
            pow_out: 0.0,
            far_frames: 0,
            suppressed_frames: 0,
            fft: RfftCache::new(),
            dump_far,
            dump_near,
        };
        s.reset();
        s
    }

    // ---------------- 对外接口（与 NlmsAec 对齐） ----------------

    /// 扬声器侧参考信号（16k mono PCM16）。
    pub fn push_far(&mut self, far16k: &[i16]) {
        let d = decode_pcm(far16k);
        if d.is_empty() {
            return;
        }
        if let Some(f) = &mut self.dump_far {
            let _ = write_pcm16(f, far16k);
        }
        ring_write(&mut self.far_ring, self.far_n, &d);
        self.far_n += d.len();
    }

    /// 麦克风信号 -> 抑制后的输出（长度可短于输入，甚至为空——STFT 群延迟）。
    ///
    /// 长期速率守恒；上游按实际长度消费即可。
    pub fn cancel(&mut self, near: &[i16]) -> Vec<i16> {
        let d = decode_pcm(near);
        if d.is_empty() {
            return Vec::new();
        }
        if let Some(f) = &mut self.dump_near {
            let _ = write_pcm16(f, near);
        }
        ring_write(&mut self.near_ring, self.near_n, &d);
        self.near_n += d.len();
        // far 欠载检测（与 NLMS 版同语义：far 停推超阈值 → 丢弃帧队列
        // 透传；far keepalive 正常时不会触发）
        if self.far_n != self.far_last_n {
            self.far_last_n = self.far_n;
            self.far_adv_at = self.near_n;
        }
        if self.near_n - self.far_adv_at > self.stale_margin {
            self.clear_stream();
            return encode_pcm(&d);
        }
        self.maybe_lock();
        self.in_buf.extend(d.iter().copied());
        self.drain_frames();
        let k = d.len().min(self.out_ready.len());
        if k == 0 {
            return Vec::new(); // 开头 frame-hop 内还没有产出，下块补
        }
        let out: Vec<f32> = self.out_ready.drain(..k).collect();
        encode_pcm(&out)
    }

    /// near 采集断裂后补时钟：把断裂长度注入静音，保持 far/near 锚定。
    ///
    /// far 由 speaker 的 keepalive（空闲也推静音）维持时钟连续；near 断裂
    /// 会让两边计数差 = 断裂量，之后所有 far 参考读取全部错位（AI 说话
    /// 期间采集被驱动层挤占是常态，错位后抑制实际失效——偶发循环的
    /// 根源）。补静音不重置锁定/G，抑制无缝继续；断裂段本来就没采到，
    /// 没有真实数据可丢。
    pub fn resync(&mut self, gap_seconds: f64) {
        let n = (gap_seconds * self.sample_rate as f64) as usize;
        if n == 0 {
            return;
        }
        let zeros = vec![0f32; n];
        ring_write(&mut self.near_ring, self.near_n, &zeros);
        self.near_n += n;
    }

    /// 时钟计数 (far 累计样本, near 累计样本)——far 时钟伺服估计速率差用。
    pub fn clocks(&self) -> (u64, u64) {
        (self.far_n as u64, self.near_n as u64)
    }

    /// 清空全部状态（缓冲、锁定、G 学习、统计）。
    pub fn reset(&mut self) {
        self.far_n = 0;
        self.near_n = 0;
        self.far_last_n = 0;
        self.far_adv_at = 0;
        self.locked = false;
        self.delay = 0;
        self.next_try = 0;
        self.next_recheck = 0;
        self.diag = "未尝试".to_string();
        self.cand.clear(); // 延迟锁定投票缓存
        self.jump.clear(); // 锁定后大跳变投票缓存
        self.clear_stream();
        self.g_val.fill(0.0);
        self.g_seen.fill(false);
        self.ghist.fill(f32::NAN);
        self.ghist_n = 0;
        self.ef_s = None;
        self.en_s = None;
        self.far_hang = 0;
        self.pow_in = 0.0;
        self.pow_out = 0.0;
        self.far_frames = 0;
        self.suppressed_frames = 0;
    }

    /// 诊断信息（键与 Python `stats()` 一致）。
    pub fn stats(&self) -> serde_json::Value {
        let sr = self.sample_rate as f64;
        let sup = if self.pow_out > 0.0 && self.pow_in > 0.0 {
            10.0 * (self.pow_in / self.pow_out).log10()
        } else {
            0.0
        };
        json!({
            "locked": self.locked,
            "delay_ms": self.delay as f64 / sr * 1000.0,
            "adapted": self.g_seen.iter().any(|&b| b),
            "far_s": self.far_n as f64 / sr,
            "near_s": self.near_n as f64 / sr,
            "suppress_db": sup,
            "far_frames": self.far_frames,
        })
    }

    /// 最近一次包络互相关诊断（锁定排查用）。
    pub fn diag(&self) -> &str {
        &self.diag
    }

    // ---------------- 内部：缓冲 ----------------

    fn clear_stream(&mut self) {
        self.in_buf.clear();
        self.pend.clear();
        self.out_ready.clear();
    }

    /// 读绝对区间 `[start, start+length)`；越界/已覆盖返回 None。
    fn ring_read(ring: &[f32], total: usize, start: i64, length: usize) -> Option<Vec<f32>> {
        if length == 0 || start < 0 {
            return None;
        }
        let r = ring.len() as i64;
        let total = total as i64;
        if start < total - r || start + length as i64 > total {
            return None;
        }
        let p = start % r;
        let p = p as usize;
        if p + length <= ring.len() {
            Some(ring[p..p + length].to_vec())
        } else {
            let mut out = Vec::with_capacity(length);
            let k = ring.len() - p;
            out.extend_from_slice(&ring[p..]);
            out.extend_from_slice(&ring[..length - k]);
            Some(out)
        }
    }

    fn far_read(&self, start: i64, length: usize) -> Option<Vec<f32>> {
        Self::ring_read(&self.far_ring, self.far_n, start, length)
    }

    fn near_read(&self, start: i64, length: usize) -> Option<Vec<f32>> {
        Self::ring_read(&self.near_ring, self.near_n, start, length)
    }

    // ---------------- 内部：包络延迟锁定 ----------------

    fn maybe_lock(&mut self) {
        let sr = self.sample_rate as usize;
        let near_end = self.near_n;
        if !self.locked {
            if near_end < self.next_try {
                return;
            }
            self.next_try = near_end + trunc_usize(0.35 * sr as f64);
            let est = self.env_xcorr(near_end);
            if let Some((d_new, conf, _sec)) = est {
                // 物理下限：far put -> aplay 缓冲 -> 出声 -> 声学 -> 采集，
                // 全程 >=100ms。UI+全模态高负载下采集被挤掉大段会让包络峰
                // 漂到 0ms（真机复现：锁 0ms、真实延迟 635ms，跳变投票想
                // 纠正都来不及）——0/极小延迟一律视为伪峰丢弃。
                if d_new < 0.10 * sr as f64 {
                    log::info!(
                        "[sub] 候选 {:.0}ms < 物理下限 100ms，丢弃",
                        d_new / sr as f64 * 1000.0
                    );
                    return;
                }
                // 短句多音节峰让单次次峰比偏高——不用单次强判据，用连续
                // 候选的时间一致性投票：最近 5 次里 >=3 次落在 ±40ms 内才锁
                // （0.35s 间隔，AI 第一句 1.5s 内即可锁上）。
                self.cand.push(d_new);
                if self.cand.len() > 5 {
                    self.cand.remove(0);
                }
                if self.cand.len() >= 3 {
                    let med = median(&self.cand);
                    let hits = self
                        .cand
                        .iter()
                        .filter(|&&c| (c - med).abs() <= 0.04 * sr as f64)
                        .count();
                    if hits >= 3 {
                        self.delay = med.round_ties_even() as i64;
                        self.locked = true;
                        self.cand.clear();
                        self.next_recheck = near_end + trunc_usize(2.0 * sr as f64);
                        log::info!(
                            "[sub] 延迟锁定 {:.0}ms（投票 {}/5，conf={:.2}）",
                            self.delay as f64 / sr as f64 * 1000.0,
                            hits,
                            conf
                        );
                    }
                }
            }
            return;
        }
        if near_end >= self.next_recheck {
            self.next_recheck = near_end + trunc_usize(2.0 * sr as f64);
            let (d_new, conf, _sec) = match self.env_xcorr(near_end) {
                Some(v) => v,
                None => return,
            };
            if d_new < 0.10 * sr as f64 {
                // 物理下限（同锁定路径，防采集断裂伪峰）
                log::info!("[sub] 重估 {:.0}ms < 物理下限，忽略", d_new / sr as f64 * 1000.0);
                return;
            }
            if (d_new - self.delay as f64).abs() > 0.15 * sr as f64 {
                // 大跳变不直接拒：aplay 积压会让真实延迟漂移几百 ms
                // （实测 276->524ms），用连续一致性投票接受——连续 2 次
                // 重估彼此 ±100ms 一致才切过去，单次大跳仍视为伪峰。
                self.jump.push(d_new);
                if self.jump.len() > 2 {
                    self.jump.remove(0);
                }
                if self.jump.len() == 2
                    && (self.jump[0] - self.jump[1]).abs() <= 0.10 * sr as f64
                {
                    self.delay = ((self.jump[0] + self.jump[1]) / 2.0).round_ties_even() as i64;
                    self.jump.clear();
                    log::info!(
                        "[sub] 延迟跳变接受 {:.0}ms（连续一致，conf={:.2}）",
                        self.delay as f64 / sr as f64 * 1000.0,
                        conf
                    );
                } else {
                    log::info!(
                        "[sub] 重估跳变 {:.0}->{:.0}ms 待投票",
                        self.delay as f64 / sr as f64 * 1000.0,
                        d_new / sr as f64 * 1000.0
                    );
                }
                return;
            }
            self.jump.clear();
            self.delay = d_new.round_ties_even() as i64;
            log::info!(
                "[sub] 延迟更新 {:.0}ms（conf={:.2}）",
                self.delay as f64 / sr as f64 * 1000.0,
                conf
            );
        }
    }

    /// far 活动段 vs near 窗的包络互相关。返回 `Some((D 样本, conf, 次峰比))`。
    fn env_xcorr(&mut self, near_end: usize) -> Option<(f64, f64, f64)> {
        let sr = self.sample_rate as usize;
        let w5 = self.w5;
        let tip = self.far_n.min(near_end) as i64;
        let a0 = (tip - (self.maxlag as i64 + (1.5 * sr as f64) as i64)).max(0);
        let win = match self.far_read(a0, (tip - a0) as usize) {
            Some(v) => v,
            None => {
                self.diag = "far 缓冲读取失败".into();
                return None;
            }
        };
        let mm = win.len() / w5 * w5;
        if mm < 5 * w5 {
            self.diag = "far 历史不足".into();
            return None;
        }
        let envf = envelope(&win[..mm], w5);
        let thr = ((self.gate / self.frame as f64).sqrt()).max(1.0e-4);
        let act5: Vec<bool> = envf.iter().map(|&v| v > thr).collect();
        if !act5.iter().any(|&b| b) {
            self.diag = "far 全静音".into();
            return None;
        }
        // 包含末尾的连续活动段（容忍 0.3s = 15 格间隙）
        let mut ae = act5.len();
        while ae > 0 && !act5[ae - 1] {
            ae -= 1;
        }
        if ae == 0 {
            self.diag = "far 尾静音".into();
            return None;
        }
        let mut as_ = ae;
        let mut gap = 0usize;
        while as_ > 0 {
            if act5[as_ - 1] {
                gap = 0;
            } else {
                gap += 1;
                if gap > 15 {
                    break;
                }
            }
            as_ -= 1;
        }
        let seg = (ae - as_) * w5;
        if seg < trunc_usize(0.5 * sr as f64) {
            self.diag = format!("far 活动段过短 {:.0}ms", seg as f64 / sr as f64 * 1000.0);
            return None;
        }
        if seg > trunc_usize(3.0 * sr as f64) {
            as_ = ae - trunc_usize((3.0 * sr as f64) / w5 as f64);
        }
        let fs = a0 + (as_ * w5) as i64;
        let fe = a0 + (ae * w5) as i64;
        let ne = (fe + self.maxlag as i64).min(near_end as i64);
        if ne - fs < seg as i64 + trunc_usize(0.05 * sr as f64) as i64 {
            self.diag = "near 窗不足".into();
            return None;
        }
        let x = match self.far_read(fs, (fe - fs) as usize) {
            Some(v) => v,
            None => {
                self.diag = "段读取失败".into();
                return None;
            }
        };
        let y = match self.near_read(fs, (ne - fs) as usize) {
            Some(v) => v,
            None => {
                self.diag = "段读取失败".into();
                return None;
            }
        };
        let lx = x.len() / w5 * w5;
        let ly = y.len() / w5 * w5;
        if lx < 10 * w5 || ly < 2 * w5 {
            self.diag = "包络窗不足".into();
            return None;
        }
        let xe = envelope(&x[..lx], w5);
        let ye = envelope(&y[..ly], w5);
        let l5 = xe.len();
        let m5 = ye.len() - l5;
        if m5 < 1 {
            self.diag = "near 覆盖不足".into();
            return None;
        }
        let ex5: f64 = xe.iter().map(|&v| v * v).sum();
        if ex5 <= 1.0e-12 {
            self.diag = "far 段包络零".into();
            return None;
        }
        let nfft = 1usize << ((ye.len() as f64 + 1.0).log2().ceil() as u32);
        let mut r = vec![0.0f64; m5 + 1];
        {
            let plan: &Rfft = self.fft.get(nfft);
            plan.xcorr(&xe, &ye, &mut r);
        }
        // 逐延迟能量归一化（ye 的局部能量前缀和）
        let mut cye = Vec::with_capacity(ye.len() + 1);
        cye.push(0.0);
        let mut run = 0.0;
        for &v in &ye {
            run += v * v;
            cye.push(run);
        }
        let mut nc = vec![0.0f64; m5 + 1];
        let mut p = 0usize;
        let mut best = f64::NEG_INFINITY;
        for idx in 0..=m5 {
            let ey5 = cye[idx + l5] - cye[idx];
            let den = (ex5 * ey5).max(1.0e-30).sqrt();
            let v = r[idx] / den;
            nc[idx] = v;
            if v > best {
                best = v;
                p = idx;
            }
        }
        let conf = nc[p];
        // ±60ms 排除主峰后的次峰比
        let mut sec = 0.0f64;
        let lo_ex = p.saturating_sub(3);
        let hi_ex = (p + 4).min(nc.len());
        let mut has_other = false;
        let mut sec_max = f64::NEG_INFINITY;
        for (i, &v) in nc.iter().enumerate() {
            if i >= lo_ex && i < hi_ex {
                continue;
            }
            has_other = true;
            if v > sec_max {
                sec_max = v;
            }
        }
        if has_other {
            sec = sec_max / (conf.abs() + 1.0e-12);
        }
        let mut pf = p as f64;
        if p >= 1 && p < m5 {
            // 抛物线细化
            let (aa, bb, cc) = (nc[p - 1], nc[p], nc[p + 1]);
            let den2 = aa - 2.0 * bb + cc;
            if den2.abs() > 1.0e-12 {
                pf = p as f64 + 0.5 * ((aa - cc) / den2).clamp(-1.0, 1.0);
            }
        }
        self.diag = format!(
            "env conf={:.2} sec={:.2} D={:.0}ms 段长={:.1}s",
            conf,
            sec,
            pf * w5 as f64 / sr as f64 * 1000.0,
            seg as f64 / sr as f64
        );
        if conf < self.min_corr {
            return None;
        }
        // 次峰比不在此拒绝（短句多音节峰天然偏高）：单次难分真假，
        // 由 maybe_lock 的连续投票判时间一致性
        Some((pf * w5 as f64, conf, sec))
    }

    // ---------------- 内部：STFT 帧处理 ----------------

    fn drain_frames(&mut self) {
        let fr = self.frame;
        let hop = self.hop;
        while self.in_buf.len() >= fr {
            let t_abs = self.near_n - self.in_buf.len(); // 帧起点（绝对）
            let frame: Vec<f64> = self.in_buf.iter().take(fr).map(|&v| v as f64).collect();
            self.in_buf.drain(..hop);
            self.process_frame(&frame, t_abs);
        }
    }

    fn process_frame(&mut self, x: &[f64], t_abs: usize) {
        let fr = self.frame;
        let nbin = fr / 2 + 1;
        let mut xw = vec![0f64; fr];
        for j in 0..fr {
            xw[j] = x[j] * self.win[j] as f64;
        }
        let mut spec_n = vec![Complex::new(0.0, 0.0); nbin];
        {
            let plan: &Rfft = self.fft.get(fr);
            plan.rfft(&xw, &mut spec_n);
        }
        let mag_n: Vec<f64> = spec_n.iter().map(|c| (c.re * c.re + c.im * c.im).sqrt()).collect();

        let far_seg = if self.locked {
            self.far_read(t_abs as i64 - self.delay, fr)
        } else {
            None
        };
        let far_seg = match far_seg {
            Some(v) => v,
            None => {
                // 未锁定 / far 历史不足：本帧不抑制（远端参考缺失）
                if self.far_hang > 0 {
                    self.far_hang -= 1;
                }
                self.emit(&xw);
                return;
            }
        };
        let fd: Vec<f64> = far_seg.iter().map(|&v| v as f64).collect();
        let ef = fd.iter().map(|&v| v * v).sum::<f64>() / fr as f64;
        let far_active = ef > self.gate / fr as f64;
        if far_active {
            self.far_hang = self.hangover;
        } else if self.far_hang > 0 {
            self.far_hang -= 1;
        }
        if !far_active && self.far_hang == 0 {
            self.emit(&xw); // far 静音：完全不碰
            return;
        }
        let mut fw = vec![0f64; fr];
        for j in 0..fr {
            fw[j] = fd[j] * self.win[j] as f64;
        }
        let mut spec_f = vec![Complex::new(0.0, 0.0); nbin];
        {
            let plan: &Rfft = self.fft.get(fr);
            plan.rfft(&fw, &mut spec_f);
        }
        let mag_f: Vec<f64> = spec_f.iter().map(|c| (c.re * c.re + c.im * c.im).sqrt()).collect();

        let edges = &self.band_edges;
        // 带能量（|N|² / |F|² 前缀和后按带求差）
        let mut pn = vec![0f64; nbin + 1];
        let mut pf = vec![0f64; nbin + 1];
        for k in 0..nbin {
            pn[k + 1] = pn[k] + mag_n[k] * mag_n[k];
            pf[k + 1] = pf[k] + mag_f[k] * mag_f[k];
        }
        let mut en = vec![0f64; self.nb];
        let mut ef_band = vec![0f64; self.nb];
        for b in 0..self.nb {
            en[b] = pn[edges[b + 1]] - pn[edges[b]];
            ef_band[b] = pf[edges[b + 1]] - pf[edges[b]];
        }
        // G 学习：带比值历史的滑动分位数。帧级错位（包络锁定 ±20ms
        // × 帧 32ms）会让 far 能量峰对上 near 谷，单帧比值虚低 1~2 个量级，
        // min 跟踪会锁死在谷值——分位数对错位帧鲁棒；滑动窗 ~48 帧自适应
        // 音量变化。eF 不足门的带本帧记 NaN（不参与该带分位）。
        let j = self.ghist_n % self.gh;
        for b in 0..self.nb {
            let fgate = 4.0 * self.gate * self.band_frac[b];
            self.ghist[b * self.gh + j] = if ef_band[b] > fgate {
                ((en[b].max(0.0) / ef_band[b].max(1.0e-30)) as f32).sqrt()
            } else {
                f32::NAN
            };
        }
        self.ghist_n += 1;
        if far_active && self.ghist_n >= 8 {
            for b in 0..self.nb {
                let q = nan_percentile(&self.ghist[b * self.gh..b * self.gh + self.gh], self.g_q);
                if q.is_finite() && q > 0.0 {
                    self.g_val[b] = q;
                    self.g_seen[b] = true;
                }
            }
        }
        // 抑制增益：(|N|-over*G|F|)/|N|，下限 floor；尾沿用浅抑制。
        // 帧级错位（±20ms × 音节边界陡峭）让单帧 eF/eN 不同步——先做
        // 攻快放慢的带能量平滑（攻击 ~2 帧 / 释放 ~7 帧），g 在平滑域算。
        smooth_band(&mut self.ef_s, &ef_band);
        smooth_band(&mut self.en_s, &en);
        let hang_soft = if far_active { 1.0 } else { 0.3 };
        let efs = self.ef_s.as_ref().unwrap();
        let ens = self.en_s.as_ref().unwrap();
        let mut g = vec![0f64; self.nb];
        let mut g_mean = 0.0;
        for b in 0..self.nb {
            let sqrt_efs = efs[b].max(0.0).sqrt();
            let est = self.g_val[b] as f64 * sqrt_efs;
            let e_ns = ens[b].max(1.0e-30);
            let sqrt_ens = e_ns.sqrt();
            let mut gb = ((sqrt_ens - self.over * est * hang_soft) / sqrt_ens)
                .max(self.floor / hang_soft);
            gb = gb.min(1.0);
            g[b] = gb;
            g_mean += gb;
        }
        g_mean /= self.nb as f64;
        // 展开到 bin
        let mut gbin = vec![0f64; nbin];
        let mut idx = 0usize;
        for (b, w) in edges.windows(2).enumerate() {
            for _ in w[0]..w[1] {
                gbin[idx] = g[b];
                idx += 1;
            }
        }
        self.far_frames += 1;
        let mut pow_out_acc = 0.0;
        let mut spec2 = vec![Complex::new(0.0, 0.0); nbin];
        for k in 0..nbin {
            let m2 = mag_n[k] * gbin[k];
            pow_out_acc += m2 * m2;
            spec2[k] = Complex::new(spec_n[k].re * gbin[k], spec_n[k].im * gbin[k]);
        }
        self.pow_in += en.iter().sum::<f64>();
        self.pow_out += pow_out_acc;
        if g_mean < 0.999 {
            self.suppressed_frames += 1;
        }
        let mut y = vec![0f64; fr];
        {
            let plan: &Rfft = self.fft.get(fr);
            plan.irfft(&spec2, &mut y);
        }
        let mut yw = vec![0f64; fr];
        for k in 0..fr {
            yw[k] = y[k] * self.win[k] as f64;
        }
        self.emit(&yw);
    }

    /// 帧结果并入 OLA 待定段，产出的 hop 样本追加进输出流。
    fn emit(&mut self, framed: &[f64]) {
        let fr = self.frame;
        let hop = self.hop;
        if self.pend.len() < fr {
            self.pend.resize(fr, 0.0);
        }
        for (pd, fr) in self.pend.iter_mut().zip(framed) {
            *pd += *fr as f32;
        }
        self.out_ready.extend(self.pend[..hop].iter().copied());
        self.pend.drain(..hop);
    }
}

/// 带能量平滑：上升沿 0.6/0.4（攻击 ~2 帧），下降沿 0.85/0.15（释放 ~7 帧）。
/// 首帧直接存当前值（Python 的 `s is None` 分支）。
fn smooth_band(s: &mut Option<Vec<f64>>, cur: &[f64]) {
    match s {
        None => *s = Some(cur.to_vec()),
        Some(sm) => {
            for k in 0..sm.len() {
                if cur[k] > sm[k] {
                    sm[k] = 0.6 * sm[k] + 0.4 * cur[k];
                } else {
                    sm[k] = 0.85 * sm[k] + 0.15 * cur[k];
                }
            }
        }
    }
}

/// 环形写入（Python `_ring_write`；本模块的推送块恒小于环长）。
fn ring_write(ring: &mut [f32], pos: usize, d: &[f32]) {
    let r = ring.len();
    let p = pos % r;
    let e = (r - p).min(d.len());
    ring[p..p + e].copy_from_slice(&d[..e]);
    if p + d.len() > r {
        let tail = (p + d.len()) % r;
        ring[..tail].copy_from_slice(&d[e..]);
    }
}

/// `sqrt(mean(reshape(-1, w5)², axis=1))`：w5 窗 RMS 包络（f64）。
fn envelope(x: &[f32], w5: usize) -> Vec<f64> {
    let n = x.len() / w5;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut s = 0.0;
        for j in 0..w5 {
            let v = x[i * w5 + j] as f64;
            s += v * v;
        }
        out.push((s / w5 as f64).sqrt());
    }
    out
}

/// `np.nanpercentile(a, q*100)`（linear 方法；全 NaN 返回 NaN；f32 域计算）。
fn nan_percentile(a: &[f32], q: f64) -> f32 {
    let mut vals: Vec<f32> = a.iter().copied().filter(|v| !v.is_nan()).collect();
    if vals.is_empty() {
        return f32::NAN;
    }
    vals.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let n = vals.len();
    if n == 1 {
        return vals[0];
    }
    let idx = q * (n - 1) as f64;
    let lo = idx.floor() as usize;
    let t = (idx - lo as f64) as f32;
    if lo + 1 >= n || t == 0.0 {
        vals[lo]
    } else {
        // numpy _lerp：t<0.5 用 a+(b-a)*t，t>=0.5 用 b-(b-a)*(1-t)（对称减误差）
        let (a, b) = (vals[lo], vals[lo + 1]);
        if t >= 0.5 {
            b - (b - a) * (1.0 - t)
        } else {
            a + (b - a) * t
        }
    }
}

/// `np.median`（偶数个取中间两值的平均，f64）。
fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// 打开调试转储（far.pcm / near.pcm）。失败仅告警（不影响主流程）。
fn open_dump(dir: &std::path::Path) -> (Option<File>, Option<File>) {
    let mk = |name: &str| -> Option<File> {
        match std::fs::create_dir_all(dir) {
            Ok(()) => match File::create(dir.join(name)) {
                Ok(f) => Some(f),
                Err(e) => {
                    log::warn!("子带抑制调试转储 {} 创建失败：{}", name, e);
                    None
                }
            },
            Err(e) => {
                log::warn!("子带抑制调试转储目录 {} 创建失败：{}", dir.display(), e);
                None
            }
        }
    };
    let far = mk("far.pcm");
    let near = mk("near.pcm");
    if far.is_some() && near.is_some() {
        log::info!("子带抑制调试转储: {}", dir.display());
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

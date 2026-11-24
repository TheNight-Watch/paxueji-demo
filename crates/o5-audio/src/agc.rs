//! 近端软件 AGC + far 时钟伺服（arecord/aplay 裸 ALSA 路径的补偿层）。
//!
//! 背景（02 机 dump 实测，2026-11）：MaixPy Recorder 走 Axera 私有管线，
//! 其 AGC 是 Python 版隐藏的回声抑制器——近场回声（人声 4~10 倍电平）
//! 被压 -26dB 而中等人声保持。Rust 的 arecord 裸路径没有这层，麦克风
//! 软增益 ×3.98（补人声电平）同时把回声放大 +12dB，环路增益实测
//! +13.7~+20.6dB，subband(4.6dB)+duck(-14dB) 组合压不住，残留回声
//! 仍触发服务端 VAD → 自触发循环（"对，就是一百二十八"×N）。
//!
//! 两层补偿：
//! - [`Agc`]：AEC 之后对 near 流做快压慢放的下行压缩（只压不放大，
//!   避免抬底噪），阈值按 dump 定标（人声峰 ≤4037 / 回声均值 ~5000 /
//!   底噪 p95=154，取 4500 分界）；
//! - [`FarServo`]：far/near 两路时钟存在相对速率差（采集实测 -0.16%、
//!   合计漂移 0.55%/s，24.6s 内回声 lag 单调漂 134ms），延迟锁定被拖垮。
//!   伺服持续估计 far 实际推进/near 实际推进之比，把 far 参考流按比率
//!   微重采样后喂 AEC——无论偏差来自采集还是播放侧都吸收掉，far 位置
//!   始终贴着 near 时钟走，锁定不再漂移（far 只是参考，音质无关紧要）。

/// 近端软件 AGC（下行压缩器）：块级 RMS 超 [`Agc::threshold`] 快速压、
/// 回落后慢速放；增益上限 1.0（不做上行放大，不抬底噪/残留回声）。
///
/// 块粒度 = 调用方给的任意长度，内部按 20ms 子块估计与更新增益，块内
/// 用当前增益统一缩放（压低方向的阶跃听感是小音量突变，无爆音）。
pub struct Agc {
    /// 触发阈值（i16 域 RMS，20ms 子块）。
    threshold: f32,
    /// 当前增益（乘性，∈ [MIN_GAIN, 1.0]）。
    gain: f32,
}

/// 最深压制（-24dB，对齐 MaixPy AGC 实测压制深度：持续音 RMS 4259→210）。
const MIN_GAIN: f32 = 0.06;
/// 快攻系数（每 20ms 子块；时间常数 ~57ms，MaixPy 实测 ~0.5s 压到位的量级）。
const ATTACK: f32 = 0.35;
/// 慢释系数（每 20ms 子块；时间常数 ~4s——回声词间停顿 100~300ms 恢复
/// 不到 10%，句间停顿 <1s 恢复 <25%，下句开头重新快攻兜住）。
const RELEASE: f32 = 0.005;
/// 释放线：RMS 低于 threshold×0.4 才开始恢复（回声词间停顿不误放）。
const RELEASE_RATIO: f32 = 0.4;

impl Agc {
    /// `threshold`：i16 域 RMS 阈值（真机 dump 定标 4500）。
    pub fn new(threshold: f32) -> Self {
        Self { threshold: threshold.max(1.0), gain: 1.0 }
    }

    /// 当前增益（诊断/测试用）。
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// 原地处理一块 PCM16（i16 样本域）。
    pub fn process(&mut self, x: &mut [i16]) {
        const SUB: usize = 320; // 20ms @16k
        for sub in x.chunks_mut(SUB) {
            let rms = (sub.iter().map(|&s| {
                let v = f64::from(s);
                v * v
            }).sum::<f64>() / sub.len() as f64).sqrt() as f32;
            if rms > self.threshold {
                // 快攻：目标 = 等比压到阈值（最深 MIN_GAIN）
                let target = (self.threshold / rms).max(MIN_GAIN);
                self.gain += (target - self.gain) * ATTACK;
            } else if self.gain < 1.0 && rms < self.threshold * RELEASE_RATIO {
                // 慢释：安静（远低于阈值）才恢复
                self.gain += (1.0 - self.gain) * RELEASE;
            }
            if self.gain < 1.0 {
                for s in sub.iter_mut() {
                    *s = ((f64::from(*s) * f64::from(self.gain)).round() as i64)
                        .clamp(-32768, 32767) as i16;
                }
            }
        }
    }
}

/// far 时钟伺服：吸收 far/near 两路时钟的相对速率差。
///
/// 比率定义 `ratio = far 原始推进 / near 推进`（>1 = far 快）。真机
/// 实测 ~1.0055（near 慢 0.16% + 播放侧慢 ~0.4%）。估计窗口 5s、EMA
/// 平滑、异常值（超出 ±5%）丢弃。估计稳定后，`process` 把 far 块按
/// 1/ratio 线性重采样（输出变短）再喂 AEC，far 位置即与 near 同步。
pub struct FarServo {
    /// 估计比率（far 实际/near 实际）。
    ratio: f64,
    /// 伺服前 far 累计样本数（原始输入计数）。
    raw_far: u64,
    /// 上次估计锚点 (raw_far, near_n)。
    anchor: Option<(u64, u64)>,
}

/// 比率估计窗口（near 样本数；5s）。
const OBSERVE_WINDOW: u64 = 16_000 * 5;
/// EMA 平滑系数。
const EMA_ALPHA: f64 = 0.4;
/// 接受带（±5%）。
const RATIO_BAND: f64 = 0.05;

impl FarServo {
    pub fn new() -> Self {
        Self { ratio: 1.0, raw_far: 0, anchor: None }
    }

    /// 当前比率估计（诊断/测试用）。
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// 用 AEC 的时钟计数更新比率估计（speaker 线程每块调用一次）。
    /// `near_n`：AEC 已处理的 near 累计样本数。
    pub fn observe(&mut self, near_n: u64) {
        match self.anchor {
            None => self.anchor = Some((self.raw_far, near_n)),
            Some((af, an)) => {
                let dn = near_n.saturating_sub(an);
                if dn < OBSERVE_WINDOW {
                    return;
                }
                let df = self.raw_far.saturating_sub(af) as f64;
                let r = df / dn as f64;
                if (r - 1.0).abs() < RATIO_BAND {
                    self.ratio += (r - self.ratio) * EMA_ALPHA;
                } // 带外：异常（如播放通道重建），丢弃等下窗
                self.anchor = Some((self.raw_far, near_n));
            }
        }
    }

    /// far 块按当前比率微重采样（ratio>1 压短），返回喂 AEC 的样本。
    pub fn process(&mut self, x: &[i16]) -> Vec<i16> {
        self.raw_far += x.len() as u64;
        if (self.ratio - 1.0).abs() < 1e-6 || x.len() < 2 {
            return x.to_vec();
        }
        let n_out = ((x.len() as f64) / self.ratio).round() as usize;
        let step = (x.len() - 1) as f64 / n_out.max(1) as f64;
        let mut out = Vec::with_capacity(n_out);
        for j in 0..n_out {
            let t = j as f64 * step;
            let i = t as usize;
            let f = (t - i as f64) as f32;
            let a = f32::from(x[i]);
            let b = f32::from(x[(i + 1).min(x.len() - 1)]);
            out.push((a + (b - a) * f).round().clamp(-32768.0, 32767.0) as i16);
        }
        out
    }
}

impl Default for FarServo {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[i16]) -> f32 {
        (x.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>()
            / x.len() as f64).sqrt() as f32
    }

    #[test]
    fn agc_compresses_loud_and_keeps_speech() {
        let mut agc = Agc::new(4500.0);
        // 12000 振幅回声块：0.6s 后应压到阈值附近以下
        let mut x = vec![12000i16; 16000];
        agc.process(&mut x);
        let after_first = rms(&x);
        assert!(after_first < 12000.0 * 0.8, "首块应有明显压制: {after_first}");
        for _ in 0..28 {
            let mut more = vec![12000i16; 16000];
            agc.process(&mut more);
            x = more;
        }
        assert!(rms(&x) < 4500.0 * 1.3, "持续回声应压到阈值附近: {}", rms(&x));
        assert!(agc.gain() >= MIN_GAIN - 1e-6);

        // 中等人声（RMS ~3000）不受影响
        let mut speech: Vec<i16> = (0..16000)
            .map(|i| (3000.0 * (i as f32 * 0.06).sin()) as i16)
            .collect();
        let before = rms(&speech);
        let mut agc2 = Agc::new(4500.0);
        agc2.process(&mut speech);
        assert!((rms(&speech) - before).abs() / before < 0.05, "人声不应被动");
    }

    #[test]
    fn agc_releases_slowly_and_never_amplifies() {
        let mut agc = Agc::new(4500.0);
        let mut l = vec![12000i16; 16000];
        agc.process(&mut l);
        let g_compressed = agc.gain();
        assert!(g_compressed < 1.0);
        // 词间停顿（0.3s 安静）基本不释放（恢复 <10%）
        for _ in 0..15 {
            let mut quiet = vec![50i16; 320];
            agc.process(&mut quiet);
        }
        assert!(
            agc.gain() < g_compressed + (1.0 - g_compressed) * 0.1,
            "短停顿不应快速释放: {} -> {}",
            g_compressed,
            agc.gain()
        );
        // 长安静（>14s）后恢复，但永不超过 1.0
        for _ in 0..700 {
            let mut quiet = vec![50i16; 320];
            agc.process(&mut quiet);
        }
        assert!(
            agc.gain() <= 1.0 + 1e-6 && agc.gain() > 0.95,
            "长安静应恢复: {}",
            agc.gain()
        );
        // 底噪（RMS 150）在增益 ≈1.0 下基本原样
        let mut noise = vec![150i16; 320];
        agc.process(&mut noise);
        assert!(rms(&noise) >= 140.0 && rms(&noise) <= 151.0);
    }

    #[test]
    fn servo_passes_through_when_ratio_unit() {
        let mut s = FarServo::new();
        let x: Vec<i16> = (0..1000).map(|i| (i % 97) as i16 - 48).collect();
        let y = s.process(&x);
        assert_eq!(y, x);
        assert_eq!(s.raw_far, 1000);
    }

    #[test]
    fn servo_resamples_by_ratio() {
        let mut s = FarServo::new();
        // 用 observe 序列驱动：far 每 5s 窗推 80800 样本对 near 80000 →
        // ratio≈1.01（估计窗口 5s = 80000 near 样本）
        for k in 0..3 {
            // 每轮 far 推 80800（分 80 块），near_n 推进 80000
            for _ in 0..80 {
                let x = vec![100i16; 1010];
                s.process(&x);
            }
            s.observe(80000 * (k + 1) as u64);
        }
        // 第一窗只建锚点，第二窗起估出 ~1.01
        assert!((s.ratio() - 1.01).abs() < 0.005, "ratio={}", s.ratio());
        // ratio>1：输出应短于输入（约 1/1.01）
        let x = vec![1000i16; 5050];
        let y = s.process(&x);
        let expect = (5050.0 / s.ratio()).round() as usize;
        assert_eq!(y.len(), expect);
        assert!(y.len() < 5050, "ratio>1 应压缩: {}", y.len());
    }

    #[test]
    fn servo_drops_outlier_ratio() {
        let mut s = FarServo::new();
        s.observe(0); // 锚点
        // far 静止 5s（near 推 80000）：ratio=0 → 带外丢弃
        s.observe(80000);
        assert_eq!(s.ratio(), 1.0, "带外比率应被丢弃");
        // 锚点应已滚动（下一窗重新估）
        s.observe(160000);
        assert_eq!(s.ratio(), 1.0);
    }
}

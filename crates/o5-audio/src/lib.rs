//! o5-audio：回声消除（NLMS / 子带包络）+ 麦克风采集 / 扬声器流式播放。
//!
//! `o5/aec.py`（时域 NLMS 自适应滤波 + FFT 互相关延迟对齐 + 漂移补偿）、
//! `o5/subband_aec.py`（STFT 子带包络抑制）与 `o5/audio_io.py`（mic/speaker
//! 线程 + 门控/闪避 + far 参考）的忠实移植。本 crate 是全项目数值最重的部分，
//! 移植时逐函数对照 Python 源码，float32/float64 的分工也逐处对齐。

pub mod device;

mod aec;
mod agc;
mod audio_io;
mod fft;
mod subband;

pub use audio_io::{AudioIo, INPUT_RATE, MicLevelTracker, SinkFactory, UNIT_BYTES, vol_gain};
pub use aec::{NlmsAec, NlmsAecParams};
pub use agc::{Agc, FarServo};
pub use subband::{SubbandParams, SubbandSuppressor};

/// 线性插值重采样 PCM16（`o5/protocol.py` 同款）：播放端 48k -> 16k 之类的
/// 兜底换率，供调用方在 [`AudioIo::put_speaker`] 前预处理。
pub use o5_protocol::resample_pcm16;

/// 三档回声实现（[`AudioIo`](crate::AudioIo) 内部组合用）。
///
/// - [`AecImpl::Nlms`]：波形域自适应滤波（波形相关的链路才有效）；
/// - [`AecImpl::Subband`]：STFT 子带包络抑制（扬声器非线性 + AGC 链路上
///   波形零相关，只能按包络压）；
/// - [`AecImpl::Off`]：关闭（直通）。
pub enum AecImpl {
    Off,
    Nlms(NlmsAec),
    Subband(SubbandSuppressor),
}

impl AecImpl {
    /// 扬声器侧参考信号（16k mono PCM16）。
    pub fn push_far(&mut self, far16k: &[i16]) {
        match self {
            Self::Off => {}
            Self::Nlms(a) => a.push_far(far16k),
            Self::Subband(s) => s.push_far(far16k),
        }
    }

    /// 麦克风信号 -> 消除回声后的输出（NLMS 等长；子带可短于输入）。
    pub fn cancel(&mut self, near: &[i16]) -> Vec<i16> {
        match self {
            Self::Off => near.to_vec(),
            Self::Nlms(a) => a.cancel(near),
            Self::Subband(s) => s.cancel(near),
        }
    }

    /// 清空全部状态。
    pub fn reset(&mut self) {
        match self {
            Self::Off => {}
            Self::Nlms(a) => a.reset(),
            Self::Subband(s) => s.reset(),
        }
    }

    /// 诊断信息（JSON 键与各 Python 实现 `stats()` 一致）。
    pub fn stats(&self) -> serde_json::Value {
        match self {
            Self::Off => serde_json::json!({}),
            Self::Nlms(a) => a.stats(),
            Self::Subband(s) => s.stats(),
        }
    }

    /// near 采集断裂补时钟（只有子带实现有意义，其余 no-op）。
    pub fn resync(&mut self, gap_seconds: f64) {
        if let Self::Subband(s) = self {
            s.resync(gap_seconds);
        }
    }

    /// 时钟计数 (far 累计样本, near 累计样本)——far 时钟伺服估计速率差用。
    pub fn clocks(&self) -> (u64, u64) {
        match self {
            Self::Off => (0, 0),
            Self::Nlms(a) => a.clocks(),
            Self::Subband(s) => s.clocks(),
        }
    }
}

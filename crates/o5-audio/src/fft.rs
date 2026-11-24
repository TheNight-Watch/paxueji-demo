//! FFT 封装：rustfft 上的实数正/逆变换（`np.fft.rfft` / `np.fft.irfft` 的等价物）。
//!
//! 两个 AEC 实现只用实数信号：延迟估计的归一化互相关（`irfft(conj(a)*b)`）
//! 与子带 STFT（`rfft` / `irfft`）。rustfft 本体是复数变换，这里补上实数的
//! 打包/解包（输入虚部置零取前 `n/2+1` 个 bin；逆变换按共轭对称补全后取实部）。
//!
//! 计划按长度缓存（`FftPlanner` 内部不是 `Send`，只在未命中时临时创建并
//! 丢弃；产出的 `Arc<dyn Fft>` 是 `Send + Sync` 的，宿主结构因此可跨线程）。

use std::collections::HashMap;
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

/// 一个长度（2 的幂）的正/逆复数 FFT 对。
#[derive(Clone)]
pub(crate) struct Rfft {
    nfft: usize,
    fwd: Arc<dyn rustfft::Fft<f64>>,
    inv: Arc<dyn rustfft::Fft<f64>>,
}

impl Rfft {
    /// 为指定长度创建 FFT 对（长度建议为 2 的幂，rustfft 内部自选算法）。
    pub(crate) fn new(nfft: usize) -> Self {
        let mut planner = FftPlanner::<f64>::new();
        Self {
            nfft,
            fwd: planner.plan_fft_forward(nfft),
            inv: planner.plan_fft_inverse(nfft),
        }
    }

    /// `np.fft.rfft(x, n)`：实数信号（自动补零到 n）-> 前 `n/2+1` 个谱 bin。
    ///
    /// `out` 长度须为 `n/2+1`；`x.len()` 必须 `<= n`。
    pub(crate) fn rfft(&self, x: &[f64], out: &mut [Complex<f64>]) {
        debug_assert!(x.len() <= self.nfft);
        debug_assert_eq!(out.len(), self.nfft / 2 + 1);
        let mut buf = vec![Complex::<f64>::new(0.0, 0.0); self.nfft];
        for (b, v) in buf.iter_mut().zip(x) {
            b.re = *v;
        }
        self.fwd.process(&mut buf);
        out.copy_from_slice(&buf[..self.nfft / 2 + 1]);
    }

    /// `np.fft.irfft(X, n)`：前 `n/2+1` 个谱 bin -> n 个实数样本。
    ///
    /// 共轭对称补全时，Nyquist bin（n 为偶数时的 `X[n/2]`）与 DC bin 只出现
    /// 一次，虚部被丢弃（numpy 同语义：输入是"半谱"，输出必为实数）。
    /// 注意 rustfft 的逆变换**不带 1/n 归一化**（`ifft(fft(x)) = n·x`），
    /// 这里除回 n 与 `np.fft.irfft` 对齐。
    pub(crate) fn irfft(&self, spec: &[Complex<f64>], out: &mut [f64]) {
        debug_assert_eq!(spec.len(), self.nfft / 2 + 1);
        debug_assert_eq!(out.len(), self.nfft);
        let mut buf = vec![Complex::<f64>::new(0.0, 0.0); self.nfft];
        buf[..spec.len()].copy_from_slice(spec);
        for k in 1..self.nfft / 2 {
            let c = buf[k];
            buf[self.nfft - k] = Complex::new(c.re, -c.im);
        }
        self.inv.process(&mut buf);
        let s = 1.0 / self.nfft as f64;
        for (o, b) in out.iter_mut().zip(buf) {
            *o = b.re * s;
        }
    }

    /// `np.fft.irfft(np.conj(a) * b, n)` 的前 `take` 个值：互相关核心。
    ///
    /// `r[p] = sum_k x[k] * y[k+p]`（x 与 a 对应、y 与 b 对应）。两路都是实
    /// 数信号，频域乘积 `conj(A)·B` 的逆变换即循环互相关；`n >= len(x)+p_max`
    /// 时所需正滞后不回绕，与线性相关一致（调用方保证）。
    pub(crate) fn xcorr(&self, x: &[f64], y: &[f64], out: &mut [f64]) {
        debug_assert!(x.len() <= self.nfft && y.len() <= self.nfft);
        let half = self.nfft / 2 + 1;
        let mut xa = vec![Complex::<f64>::new(0.0, 0.0); self.nfft];
        let mut yb = vec![Complex::<f64>::new(0.0, 0.0); self.nfft];
        for (d, v) in xa.iter_mut().zip(x) {
            d.re = *v;
        }
        for (d, v) in yb.iter_mut().zip(y) {
            d.re = *v;
        }
        self.fwd.process(&mut xa);
        self.fwd.process(&mut yb);
        // c[k] = conj(a[k]) * b[k]，再按共轭对称补全后半谱
        let mut buf = vec![Complex::<f64>::new(0.0, 0.0); self.nfft];
        for k in 0..half {
            let ak = xa[k];
            let bk = yb[k];
            let c = Complex::new(ak.re * bk.re + ak.im * bk.im, ak.re * bk.im - ak.im * bk.re);
            buf[k] = c;
            if k >= 1 && k < self.nfft / 2 {
                buf[self.nfft - k] = Complex::new(c.re, -c.im);
            }
        }
        self.inv.process(&mut buf);
        // rustfft 逆变换不带 1/n 归一化，这里除回 n（np.fft.irfft 语义）。
        let s = 1.0 / self.nfft as f64;
        for (o, v) in out.iter_mut().zip(buf) {
            *o = v.re * s;
        }
    }
}

/// 按长度缓存 FFT 计划的容器（AEC 结构持有一个，按需扩容）。
#[derive(Default)]
pub(crate) struct RfftCache {
    plans: HashMap<usize, Rfft>,
}

impl RfftCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 取（或创建）指定长度的计划。长度必须 > 1。
    pub(crate) fn get(&mut self, n: usize) -> &Rfft {
        self.plans.entry(n).or_insert_with(|| Rfft::new(n))
    }
}

//! 音频设备通道：`CaptureSource` / `PlaybackSink` 与各平台实现。
//!
//! 对应 Python 侧 `maix.audio.Recorder` / aplay 子进程两条通道：
//! - 采集（linux）：`arecord` 子进程（`-D plughw:CARD=Audio,DEV=0`，实测该
//!   声卡名比索引稳；`--buffer-size/--period-size` 缩小 ALSA 缓冲，采集间隙
//!   从 500ms 级降到 ~50ms）。管道 fd 上 `poll(2)` 等数据，100ms 超时返回
//!   `Ok(0)`——采集循环据此走 25ms 节流的空转分支，停止请求也能及时观测。
//! - 播放（linux）：`aplay` 子进程（`-D plughw:0,1` 管道写入：阻塞在 pipe
//!   上，不占进程里别的锁，播放节奏由 aplay 维护）。
//! - PC（Windows 等）：内存环回 mock（采集=队列/静音节拍，播放=实时速率
//!   睡眠），供无声卡的 Windows 上跑全链路联调。
//!
//! 子进程生命周期：`Drop` 时 SIGTERM + 限时等待 + SIGKILL（对齐 Python
//! `_close_sinks` 的 terminate/wait/kill 链）。

use std::collections::VecDeque;
use std::io;
#[cfg(target_os = "linux")]
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 麦克风采集通道：读一块 PCM16（小端字节序）。
///
/// 语义（与契约一致）：
/// - 阻塞到有数据或短超时（≤100ms 量级）；
/// - 返回写入 `buf` 的字节数；`Ok(0)` 表示本拍无数据（非 EOF，调用方继续轮询）；
/// - `Err` 表示通道异常（调用方记日志、退避后重试）。
pub trait CaptureSource: Send {
    fn read_pcm16(&mut self, buf: &mut [u8]) -> io::Result<usize>;
}

/// 扬声器播放通道：写一块 PCM16（小端字节序）。
///
/// 阻塞语义：写不进（管道满）时由实现内部等待，返回即表示数据已进入
/// 播放通道（对 aplay 是"已入内核管道"）。
pub trait PlaybackSink: Send {
    fn write_pcm16(&mut self, pcm: &[u8]) -> io::Result<()>;
}

/// 麦克风采集增益（复刻 MaixPy `Recorder.volume()` 的管内软增益）。
///
/// 真机定标（02 机，同一测试音三路实测）：MaixPy 走 Axera 私有媒体管线，
/// `volume()` 是管线内软增益，不改任何 ALSA 控件；v50=1.0x、v100=4.0x
/// （线性 dB 曲线，即 dB=(v-50)×0.24）。arecord 走 ALSA 原始路径电平
/// ≈ v50，且 `TX LEFT DIG GAIN` 控件实测定为无效（15→60 无变化），故
/// 在采集层施加同曲线软件增益，且**在 AEC 之前**——Python 的 AEC 吃的
/// 也是 boost 后的样本，两边语义一致。
pub fn mic_gain(volume: u8) -> f32 {
    (10f64.powf((f64::from(volume) - 50.0) * 0.012)) as f32
}

/// 原地放大 PCM16 LE 块（饱和到 i16 范围）；gain≈1 时零开销直通。
/// 长度为奇数时末尾单字节原样保留。
pub fn apply_pcm16_gain(buf: &mut [u8], gain: f32) {
    if (gain - 1.0).abs() < 1e-6 {
        return;
    }
    for chunk in buf.chunks_exact_mut(2) {
        let s = i16::from_le_bytes([chunk[0], chunk[1]]);
        let v = ((f64::from(s) * f64::from(gain)).round() as i64).clamp(-32768, 32767) as i16;
        chunk.copy_from_slice(&v.to_le_bytes());
    }
}

/// 交织立体声 → 左声道抽取（跨 read 边界保持相位）。
///
/// plughw:CARD=Audio,DEV=0 的采集怪癖（02 机实测定标）：请求 `-c 1` 时
/// plug 层**不做立体声下混**，直接交付原生交织流——左声道有效、右声道
/// 恒零，32k 样本/s（= 16k 帧/s）。若照单全收，每个 1s unit 里只有半秒
/// 真实音频且音高减半，服务端 VAD 永远不触发。此抽取器攒住原始字节、
/// 两字节一样本地取偶数位（左声道）样本，剩余半样本/放不下的部分留在
/// stash 里等下一轮，永不丢字节。
pub struct StereoToMono {
    stash: Vec<u8>,
    next_left: bool,
}

impl StereoToMono {
    pub fn new() -> Self {
        StereoToMono { stash: Vec::new(), next_left: true }
    }

    /// 输入本轮原始字节（可为空），抽取左声道样本写入 `out`，
    /// 返回写入字节数（恒为偶数）。
    pub fn extract(&mut self, raw: &[u8], out: &mut [u8]) -> usize {
        self.stash.extend_from_slice(raw);
        let mut written = 0usize;
        let mut i = 0usize;
        while i + 1 < self.stash.len() && written + 2 <= out.len() {
            if self.next_left {
                out[written] = self.stash[i];
                out[written + 1] = self.stash[i + 1];
                written += 2;
            }
            self.next_left = !self.next_left;
            i += 2;
        }
        self.stash.drain(..i);
        written
    }
}

impl Default for StereoToMono {
    fn default() -> Self {
        Self::new()
    }
}

/// 子进程收尾：SIGTERM + 限时等待 + SIGKILL（对齐 Python terminate/wait/kill 链）。
#[cfg(target_os = "linux")]
fn stop_child(child: &mut Child) {
    unsafe {
        let pid = child.id() as libc::pid_t;
        if libc::kill(pid, libc::SIGTERM) != 0 {
            let _ = libc::kill(pid, libc::SIGKILL);
        }
    }
    let deadline = Instant::now() + Duration::from_millis(1000);
    while child.try_wait().ok().flatten().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill(); // SIGKILL
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// arecord 采集（linux）
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod arecord {
    use super::*;
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    use std::process::{Command, Stdio};

    /// `arecord` 子进程采集（16k 单声道 S16_LE，管道读 stdout）。
    pub struct ArecordCapture {
        child: Child,
        stdout: std::process::ChildStdout,
        gain: f32,
        /// plughw 对 -c 1 不做下混（交付原生交织立体声，右声道恒零），
        /// 采集层抽左声道还原 16k 单声道；跨 read 保持相位。
        stereo: StereoToMono,
        tmp: Vec<u8>,
        /// 码流自标定（驱动 2× 翻倍检测），见 [`Self::calibrate`]。
        calib: Calib,
        /// 缓冲参数是否可用（重起时沿用）。
        buf_params_ok: bool,
    }

    /// 采集码流标定状态机。
    ///
    /// 02 机实测：该平台 ALSA 驱动无论请求什么都交付 **2× 码流**
    /// （-r 16000 -c 1 → 32k samples/s；-r 8000 -c 2 → 32k；-r 16000 -c 2
    /// → 64k；hw: 设备同样翻倍）。策略：请求 `-r 8000 -c 2`（标称
    /// 32000 B/s），排空启动积压后测 2 秒实际码流——翻倍驱动
    /// （≈64000 B/s）保持原参数，抽左声道恰好得到实时 16k；不翻倍的
    /// 设备改请求 `-r 16000 -c 2`（标称 64000 B/s），同样抽左声道。
    #[derive(Clone, Copy)]
    enum Calib {
        /// 等第一个数据字节（区分"还没数据"与"管道已空"）。
        WaitFirst,
        /// 排空启动积压：arecord 启动到采集循环首读之间，管道+ALSA
        /// 缓冲里已积累约秒级数据（02 机实测会把速率虚测成 ~90KB/s），
        /// 丢到"连续多轮小量读取"（排空后每轮 poll 只就绪 1~3 个
        /// period ≈ 4800B 原始；不能等空读——period 25ms 一个源源不断，
        /// 空读永不出现）。
        Drain { settles: u8 },
        /// 累计码流测速率。
        Measure { bytes: u64, t0: Instant },
        Done,
    }

    impl ArecordCapture {
        /// 启动 arecord。`input_volume`（0-100）按 MaixPy 同曲线换算成
        /// 采集软增益（见 [`super::mic_gain`]）。
        ///
        /// 首选带 `--buffer-size=1600 --period-size=400`（100ms/25ms 缓冲），
        /// 启动失败（个别内核 alsa 配置不接受）去掉这两个参数重试一次。
        pub fn new(input_volume: u8) -> io::Result<Self> {
            let gain = super::mic_gain(input_volume);
            match Self::spawn(true, 8000) {
                Ok(mut child) => {
                    let stdout = child.stdout.take().expect("arecord stdout");
                    log::info!(
                        "采集走 arecord 子进程: plughw:CARD=Audio,DEV=0（-r 8000 -c 2 码流自标定中，麦克风增益 v={input_volume} → ×{gain:.2}）"
                    );
                    Ok(Self { child, stdout, gain, stereo: StereoToMono::new(), tmp: vec![0u8; 16384], calib: Calib::WaitFirst, buf_params_ok: true })
                }
                Err(e) => {
                    log::warn!(
                        "arecord 带缓冲参数启动失败（{e}），去掉 --buffer-size/--period-size 重试"
                    );
                    let mut child = Self::spawn(false, 8000)?;
                    let stdout = child.stdout.take().expect("arecord stdout");
                    log::info!(
                        "采集走 arecord 子进程: plughw:CARD=Audio,DEV=0（-r 8000 -c 2 默认缓冲，码流自标定中，麦克风增益 v={input_volume} → ×{gain:.2}）"
                    );
                    Ok(Self { child, stdout, gain, stereo: StereoToMono::new(), tmp: vec![0u8; 16384], calib: Calib::WaitFirst, buf_params_ok: false })
                }
            }
        }

        fn spawn(with_buf_params: bool, rate: u32) -> io::Result<Child> {
            let mut cmd = Command::new("arecord");
            // -c 2：此卡 plughw 请求 -c 1 不做下混（实测交付交织立体声，
            // 右声道恒零），显式请求原生双声道再抽左声道才是干净单声道
            // （真机频谱验证：440Hz 音调左声道归位）。rate 由码流标定决定：
            // 翻倍驱动用 8000（实得 32k samples/s = 实时 16k），正常驱动
            // 用 16000（标称 64k B/s）。见 [`Calib`]。
            cmd.args([
                "-q",
                "-D",
                "plughw:CARD=Audio,DEV=0",
                "-r",
                &rate.to_string(),
                "-f",
                "S16_LE",
                "-c",
                "2",
            ]);
            if with_buf_params {
                cmd.args(["--buffer-size=1600", "--period-size=400"]);
            }
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
        }

        /// 码流自标定：排空启动积压后测 2 秒实际速率，决定是否换请求参数。
        /// 返回 true 表示本轮字节已被标定消费（调用方跳过抽取）。
        fn calibrate(&mut self, n: usize) -> bool {
            let rate = match self.calib {
                Calib::WaitFirst => {
                    if n > 0 {
                        self.calib = Calib::Drain { settles: 0 };
                        return true; // 首批必是积压，丢
                    }
                    return false;
                }
                Calib::Drain { settles } => {
                    // 排空判据：本轮读量 ≤ 6400B（≈100ms 原始数据，排空后
                    // 稳态每轮 1~3 个 period）连续 4 轮才算积压排完
                    let small = n <= 6400;
                    let settles = if small { settles + 1 } else { 0 };
                    if settles >= 4 {
                        self.calib = Calib::Measure { bytes: 0, t0: Instant::now() };
                        return false;
                    }
                    self.calib = Calib::Drain { settles };
                    return true; // 积压数据丢弃
                }
                Calib::Measure { bytes, t0 } => {
                    let total = bytes + n as u64;
                    let el = t0.elapsed().as_secs_f64();
                    if el < 2.0 {
                        self.calib = Calib::Measure { bytes: total, t0 };
                        return false;
                    }
                    self.calib = Calib::Done;
                    total as f64 / el
                }
                Calib::Done => return false,
            };
            // -r 8000 -c 2 标称 32000 B/s；翻倍驱动 ≈ 64000 B/s，阈值取中间
            if rate < 44800.0 {
                log::info!(
                    "采集码流 {rate:.0} B/s ≈ 标称（驱动不翻倍），改请求 -r 16000 -c 2"
                );
                // 必须先停旧再起新（同一 hw 设备不能并发打开）；万一 16000
                // 起不来，回退原参数保住采集通道。
                stop_child(&mut self.child);
                match Self::spawn(self.buf_params_ok, 16000)
                    .or_else(|_| Self::spawn(self.buf_params_ok, 8000))
                {
                    Ok(mut child) => {
                        let stdout = child.stdout.take().expect("arecord stdout");
                        self.child = child;
                        self.stdout = stdout;
                        self.stereo = StereoToMono::new(); // 新流相位重置
                        true
                    }
                    Err(e) => {
                        log::error!("arecord 重起失败（{e}），采集通道失效");
                        true
                    }
                }
            } else {
                log::info!(
                    "采集码流 {rate:.0} B/s（驱动 2× 翻倍确认），-r 8000 -c 2 抽左声道 = 实时 16k 单声道"
                );
                false
            }
        }
    }

    impl CaptureSource for ArecordCapture {
        fn read_pcm16(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // poll(2) 等数据（100ms 超时）：超时返回 Ok(0) 让采集循环走节流分支，
            // 同时保证停止请求（running=false）最多 100ms 内被观测到。
            let fd = self.stdout.as_raw_fd();
            let mut pfd = [libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            }];
            let r = unsafe { libc::poll(pfd.as_mut_ptr(), 1, 100) };
            if r < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    return Ok(0);
                }
                return Err(err);
            }
            if r == 0 {
                return Ok(0); // 本拍无数据
            }
            // 有数据（或 HUP/EOF）：读一次。EOF（arecord 挂了）返回 0，
            // 采集循环按"持续无数据"处理并最终触发健康检查。
            let n = self.stdout.read(&mut self.tmp)?;
            // 码流自标定（排空积压 + 测速，2s 内完成；期间字节被消费）
            if self.calibrate(n) {
                return Ok(0);
            }
            let m = self.stereo.extract(&self.tmp[..n], buf);
            // MaixPy 同曲线采集软增益（AEC 之前，与 Python 语义一致）
            super::apply_pcm16_gain(&mut buf[..m], self.gain);
            Ok(m)
        }
    }

    impl Drop for ArecordCapture {
        fn drop(&mut self) {
            stop_child(&mut self.child);
        }
    }
}

#[cfg(target_os = "linux")]
pub use arecord::ArecordCapture;

// ---------------------------------------------------------------------------
// aplay 播放（linux）
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod aplay {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// `aplay` 子进程播放（stdin 管道写入；rate 由构造参数指定）。
    pub struct AplaySink {
        child: Child,
        stdin: Option<std::process::ChildStdin>,
    }

    impl AplaySink {
        pub fn new(rate: u32) -> io::Result<Self> {
            let mut child = Command::new("aplay")
                .args([
                    "-q",
                    "-D",
                    "plughw:0,1",
                    "-r",
                    &rate.to_string(),
                    "-f",
                    "S16_LE",
                    "-c",
                    "1",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let stdin = child.stdin.take();
            Ok(Self { child, stdin })
        }
    }

    impl PlaybackSink for AplaySink {
        fn write_pcm16(&mut self, pcm: &[u8]) -> io::Result<()> {
            // 管道满时 write 阻塞在内核里（播放节奏由 aplay 维护）
            let stdin = self.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "aplay 管道已关闭")
            })?;
            stdin.write_all(pcm)?;
            stdin.flush()
        }
    }

    impl Drop for AplaySink {
        fn drop(&mut self) {
            // 先关 stdin：aplay 读到 EOF 自然排空退出；超时再杀
            self.stdin = None;
            stop_child(&mut self.child);
        }
    }
}

#[cfg(target_os = "linux")]
pub use aplay::AplaySink;

// ---------------------------------------------------------------------------
// PC mock（非 linux / 测试）
// ---------------------------------------------------------------------------

/// 内存环回采集源（PC / 测试）：`push` 进来的数据排队供 `read_pcm16` 读走。
///
/// 无数据时最多等 100ms 返回 `Ok(0)`（保持"短超时"语义，采集循环的停止请求
/// 及时被观测）。`silence_clock` 模式下：25ms 内没等到数据就合成等长静音
/// 返回——PC 上没有声卡时钟时用它撑起音频时钟（unit 每秒照发）。
pub struct MockCaptureSource {
    inner: Arc<(Mutex<VecDeque<Vec<u8>>>, Condvar)>,
    /// 半包残留（push 的块比一次读的 buf 大时）。
    remainder: Vec<u8>,
    closed: Arc<AtomicBool>,
    silence_fallback: bool,
}

/// [`MockCaptureSource`] 的投递端（测试里当"录音驱动"用）。
#[derive(Clone)]
pub struct MockCaptureHandle {
    inner: Arc<(Mutex<VecDeque<Vec<u8>>>, Condvar)>,
    closed: Arc<AtomicBool>,
}

impl MockCaptureSource {
    /// 队列模式：只回显 push 进来的数据。
    pub fn new() -> (Self, MockCaptureHandle) {
        Self::build(false)
    }

    /// 静音时钟模式：空闲时按 25ms 粒度返回静音（PC 全链路联调用）。
    pub fn silence_clock() -> (Self, MockCaptureHandle) {
        Self::build(true)
    }

    fn build(silence_fallback: bool) -> (Self, MockCaptureHandle) {
        let inner = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let closed = Arc::new(AtomicBool::new(false));
        (
            Self {
                inner: inner.clone(),
                remainder: Vec::new(),
                closed: closed.clone(),
                silence_fallback,
            },
            MockCaptureHandle { inner, closed },
        )
    }
}

impl MockCaptureHandle {
    /// 投递一块 PCM16（字节）。
    pub fn push(&self, pcm: &[u8]) {
        if self.closed.load(Ordering::Relaxed) || pcm.is_empty() {
            return;
        }
        let (q, cv) = &*self.inner;
        q.lock().unwrap().push_back(pcm.to_vec());
        cv.notify_one();
    }

    /// 关闭（此后 read 一直返回 Ok(0)）。
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }
}

impl CaptureSource for MockCaptureSource {
    fn read_pcm16(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // 先吐半包残留
        if !self.remainder.is_empty() {
            let n = self.remainder.len().min(buf.len());
            buf[..n].copy_from_slice(&self.remainder[..n]);
            self.remainder.drain(..n);
            return Ok(n);
        }
        let deadline = Instant::now() + Duration::from_millis(100);
        // 静音时钟模式：先等 25ms 真数据，没等到合成静音
        let quiet_at = if self.silence_fallback {
            deadline - Duration::from_millis(75)
        } else {
            deadline
        };
        let (q, cv) = &*self.inner;
        let mut guard = q.lock().unwrap();
        loop {
            if let Some(mut chunk) = guard.pop_front() {
                drop(guard);
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                if n < chunk.len() {
                    chunk.drain(..n);
                    self.remainder = chunk;
                }
                return Ok(n);
            }
            if self.closed.load(Ordering::Relaxed) {
                return Ok(0);
            }
            let now = Instant::now();
            if now >= quiet_at {
                if self.silence_fallback {
                    // 空闲且开启了静音时钟：回 25ms 静音（800B），音频时钟不停
                    let n = buf.len().min(800);
                    buf[..n].fill(0);
                    return Ok(n);
                }
                return Ok(0);
            }
            if now >= deadline {
                return Ok(0);
            }
            // 等待目标：静音时钟等到 quiet_at（25ms 后回静音，保实时速率），
            // 普通模式等到 deadline（100ms 内等不到数据回 Ok(0)）
            let wait_until = if self.silence_fallback { quiet_at } else { deadline };
            let (g, _) = cv.wait_timeout(guard, wait_until - now).unwrap();
            guard = g;
        }
    }
}

/// PC 播放 mock：按实时速率"播放"（睡眠块时长），让上层门控/保活时序与
/// 真机一致；数据直接丢弃。
pub struct NullPlaybackSink {
    rate: u32,
}

impl NullPlaybackSink {
    pub fn new(rate: u32) -> Self {
        Self { rate }
    }
}

impl PlaybackSink for NullPlaybackSink {
    fn write_pcm16(&mut self, pcm: &[u8]) -> io::Result<()> {
        let dur = pcm.len() as f64 / 2.0 / f64::from(self.rate);
        std::thread::sleep(Duration::from_secs_f64(dur));
        Ok(())
    }
}

/// 平台默认采集通道：linux = arecord；否则 = 静音时钟 mock。
/// `input_volume`（0-100）只在 arecord 路径生效（MaixPy 同曲线软增益）。
pub fn platform_capture(input_volume: u8) -> Box<dyn CaptureSource> {
    #[cfg(target_os = "linux")]
    {
        match arecord::ArecordCapture::new(input_volume) {
            Ok(c) => Box::new(c),
            Err(e) => {
                log::error!("arecord 启动失败（{e}），退化为静音时钟 mock");
                let (src, _h) = MockCaptureSource::silence_clock();
                Box::new(src)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = input_volume;
        let (src, _h) = MockCaptureSource::silence_clock();
        Box::new(src)
    }
}

/// 平台默认播放通道工厂：linux = aplay；否则 = 实时速率 mock。
pub fn platform_sink_factory() -> Box<dyn FnMut(u32) -> io::Result<Box<dyn PlaybackSink>> + Send> {
    #[cfg(target_os = "linux")]
    {
        Box::new(|rate: u32| Ok(Box::new(aplay::AplaySink::new(rate)?) as Box<dyn PlaybackSink>))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Box::new(|rate: u32| {
            Ok(Box::new(NullPlaybackSink::new(rate)) as Box<dyn PlaybackSink>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机定标锚点（02 机三路实测）：v50=1.0x、v100=4.0x（线性 dB）。
    #[test]
    fn mic_gain_curve_anchors() {
        assert!((mic_gain(50) - 1.0).abs() < 1e-6);
        assert!((mic_gain(100) - 3.98).abs() < 0.02, "v100 ≈ ×3.98（+12dB）");
        // v50 与 v100 中点 = +6dB = ×2
        assert!((mic_gain(75) - 2.0).abs() < 0.02);
        // 低端对称：v0 = ×0.25（-12dB，与 v100 的 ×4 关于 v50 对称）
        assert!((mic_gain(0) - 0.2512).abs() < 0.001);
        // 单调
        let mut prev = 0f32;
        for v in 0u8..=100 {
            let g = mic_gain(v);
            assert!(g >= prev, "增益曲线必须单调不减");
            prev = g;
        }
    }

    #[test]
    fn apply_gain_scales_and_saturates() {
        let mut pcm = i16::from_le_bytes([0x00, 0x10]); // 4096
        let mut buf = pcm.to_le_bytes().to_vec();
        apply_pcm16_gain(&mut buf, 2.0);
        assert_eq!(i16::from_le_bytes([buf[0], buf[1]]), pcm * 2);
        // 饱和：×100 必然夹到 ±满幅
        pcm = 30000;
        let mut buf = pcm.to_le_bytes().to_vec();
        apply_pcm16_gain(&mut buf, 100.0);
        assert_eq!(i16::from_le_bytes([buf[0], buf[1]]), 32767);
        pcm = -30000;
        let mut buf = pcm.to_le_bytes().to_vec();
        apply_pcm16_gain(&mut buf, 100.0);
        assert_eq!(i16::from_le_bytes([buf[0], buf[1]]), -32768);
        // 零放大后仍是零
        let mut buf = [0u8, 0, 0, 0];
        apply_pcm16_gain(&mut buf, 4.0);
        assert_eq!(buf, [0, 0, 0, 0]);
    }

    #[test]
    fn apply_gain_passthrough_and_odd_length() {
        // gain=1 直通：逐位不变
        let mut buf: Vec<u8> = (0..64u16).flat_map(|v| v.wrapping_mul(513).to_le_bytes()).collect();
        let orig = buf.clone();
        apply_pcm16_gain(&mut buf, 1.0);
        assert_eq!(buf, orig);
        // 奇数长度：样本区正常放大，末尾单字节原样保留
        let mut odd = vec![0x01u8, 0x00, 0x02, 0x00, 0xAB];
        apply_pcm16_gain(&mut odd, 4.0);
        assert_eq!(&odd[..4], &[0x04, 0x00, 0x08, 0x00]);
        assert_eq!(odd[4], 0xAB);
    }

    /// L,0,L,0 交织流 → 只留左声道，右声道（恒零）被丢弃。
    #[test]
    fn stereo_extract_drops_dead_right_channel() {
        let mut s = StereoToMono::new();
        // 4 帧：L=0x0100, R=0x0200, L=0x0300, R=0x0400
        let raw: Vec<u8> = [0x01u16, 0x02, 0x03, 0x04]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut out = [0u8; 16];
        let n = s.extract(&raw, &mut out);
        assert_eq!(n, 4);
        assert_eq!(&out[..4], &[0x01, 0x00, 0x03, 0x00]);
    }

    /// 跨 read 边界（含半样本劈开）相位不乱。
    #[test]
    fn stereo_extract_across_read_boundaries() {
        let mut s = StereoToMono::new();
        // 完整流：L1 R1 L2 R2 L3 R3 L4 R4（L 为有效样本）
        let full: Vec<u8> = (1u16..=8)
            .flat_map(|v| (v * 0x100).to_le_bytes())
            .collect();
        let mut got = Vec::new();
        let mut out = [0u8; 8];
        // 按 3、3、2、... 任意字节数劈开喂入
        for c in full.chunks(3) {
            let n = s.extract(c, &mut out);
            got.extend_from_slice(&out[..n]);
        }
        // 左声道 = L1 L2 L3 L4 = 样本 1/3/5/7（0x0100/0x0300/0x0500/0x0700）
        assert_eq!(got, vec![0x00, 0x01, 0x00, 0x03, 0x00, 0x05, 0x00, 0x07]);
    }

    /// out 放不下时剩余字节滞留 stash，下一轮补齐，永不丢数据。
    #[test]
    fn stereo_extract_backlog_when_out_full() {
        let mut s = StereoToMono::new();
        let raw: Vec<u8> = (1u16..=8)
            .flat_map(|v| (v * 0x100).to_le_bytes())
            .collect();
        let mut tiny = [0u8; 2]; // 只够 1 个左声道样本
        let n = s.extract(&raw, &mut tiny);
        assert_eq!(n, 2);
        assert_eq!(&tiny, &[0x00, 0x01]);
        // 再来一个空读 + 大 out：把滞留的都吐出来（剩左声道 L2 L3 L4）
        let mut big = [0u8; 64];
        let m = s.extract(&[], &mut big);
        assert_eq!(m, 6);
        assert_eq!(&big[..6], &[0x00, 0x03, 0x00, 0x05, 0x00, 0x07]);
    }
}

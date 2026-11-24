//! 音频管线：麦克风采集（1 秒 unit 聚合）+ 扬声器流式播放。
//! （`o5/audio_io.py` 的忠实移植）
//!
//! 设计要点（针对测试报告暴露的问题）：
//! - 采集线程只做一件事：read_pcm16 轮询 -> 累积 -> 凑满 32000 字节立即回调，
//!   回调内部只投递（channel send 级别），绝不因为网络/发送拖慢录音节奏。
//!   旧脚本的"created 后 30s 才发第一个 unit / 发送间隔不均匀"就是阻塞式调度
//!   造成的。
//! - 静音 = 全零片段照发（音频时钟红线：服务端按 1s unit 计步）。
//! - 播放线程独立，queue 有界丢最旧；播放走 aplay 子进程（plughw:0,1 管道
//!   写入）：阻塞写在 pipe 上不占进程里别的锁，播放节奏由 aplay 维护。
//! - 回声缓解（三层，按侵入性递增）：
//!   1) AEC（audio.echoCancel，默认开）：NLMS 自适应滤波 / 子带包络抑制，
//!      扬声器写出流作为远端参考，在麦克风上行前对冲公放回灌，保留全双工
//!      打断能力；
//!   2) ducking（audio.micDuckWhilePlaying，默认 0.4）：播放期间麦克风增益
//!      压到 0.4，压制 AEC 残余回声（近场人声能量高、打断不受影响）；
//!   3) 硬门控（audio.micGateWhilePlaying，默认关）：播放期间直接置零——
//!      半双工，放弃打断，仅在前两层失效时兜底。
//!
//! 与 Python 版的有意偏差：
//! - 没有 maix Player 兜底播放路径（Rust 目标平台只有 aplay），因此也就没有
//!   块缘淡入/淡出（`FADE_MS`，那本来就是给"每块 play() 都重起播"的 Player
//!   通道掩盖波形跳变用的；aplay 是连续流，淡变反而造成周期性凹陷）；
//! - `input_volume`（maix `Recorder.volume()` 等价物）：MaixPy 的 volume 是
//!   Axera 管线内软增益（不改 ALSA 控件；真机定标 v50=1.0x / v100=4.0x），
//!   arecord 路径在采集层按同曲线施加（[`crate::device::mic_gain`]），且在
//!   AEC 之前——与 Python 的 AEC 吃 boost 后样本的语义一致；
//! - AEC 调试转储目录用 Rust 版部署路径 `/root/o5_harness_rs/o5_dbg`。

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use o5_config::Config;
use o5_perf::Timing;
use o5_protocol::resample_pcm16;

use crate::device::{platform_sink_factory, CaptureSource, PlaybackSink};
use crate::{AecImpl, Agc, FarServo, NlmsAec, NlmsAecParams, SubbandParams, SubbandSuppressor};

/// 采集/上行采样率（16k mono S16LE）。
pub const INPUT_RATE: u32 = 16000;
/// 1 秒 unit 的字节数（16000 采样 * 2 字节）。
pub const UNIT_BYTES: usize = INPUT_RATE as usize * 2;
/// 采集轮询周期。
const POLL_INTERVAL_S: f64 = 0.025;
const SUPPORTED_OUTPUT_RATES: [u32; 3] = [16000, 24000, 48000];
const FALLBACK_RATE: u32 = 48000;
/// ~ 每块几十 ms，余量充足。
const PLAY_QUEUE_CAP: usize = 120;
/// 播放队列字节水位（24k S16 mono × 3s）：服务器 ~1.5x 实时 burst 下发，
/// 只按块数限积压可达数秒（打断后排空慢的根源），双限封顶 ~3s。
const MAX_QUEUE_BYTES: usize = 24000 * 2 * 3;
/// 播放突发判定间隔。
const BURST_GAP_S: f64 = 0.3;
/// 合块播放上限：减少 play() 边界间隙（爆音主源）。
const MAX_BLOCK_S: f64 = 0.60;
/// 突发首块前导缓冲：吸收调度抖动造成的供流中断。
const LEAD_S: f64 = 0.12;
/// far 保活排水余量：距登记的播放结束不足它时不补静音。
const FAR_DRAIN_MARGIN_S: f64 = 0.20;
/// AEC 调试转储目录（Rust 版部署路径）。
const AEC_DUMP_DIR: &str = "/root/o5_harness_rs/o5_dbg";
/// 近端 AGC 阈值（i16 域 RMS，20ms 子块）。02 机 dump 定标：人声峰
/// 4037 / 回声均值 ~5000（×3.98 麦克风增益后的近场回声）/ 底噪 p95=154，
/// 取 4500：人声不触发、回声压到 1/2~1/8。
const NEAR_AGC_THRESHOLD: f32 = 4500.0;

/// 播放通道工厂（按设备速率建 sink；测试注入 mock 用）。
pub type SinkFactory = Box<dyn FnMut(u32) -> std::io::Result<Box<dyn PlaybackSink>> + Send>;

/// 0-100 音量 -> 幅度增益（感知曲线，50% 约三分之一幅度）。
pub fn vol_gain(vol: u8) -> f64 {
    let v = vol.min(100) as f64 / 100.0;
    v.powf(1.6)
}

// ---------------------------------------------------------------------------
// 小工具：PCM16 字节 <-> i16 数组 / 增益缩放
// ---------------------------------------------------------------------------

/// `<i2` 字节流 -> i16 数组（奇数尾巴按 numpy frombuffer 语义丢弃）。
fn bytes_to_i16(pcm: &[u8]) -> Vec<i16> {
    pcm.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// i16 数组 -> `<i2` 字节流。
fn i16_to_bytes(v: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 2);
    for &s in v {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// `np.rint(np.clip(a.astype(np.float32) * gain, -32768, 32767)).astype("<i2")`。
/// 音量增益 / duck 压制共用（f32 域乘、round-half-even）。
fn scale_pcm16(pcm: &[u8], gain: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() & !1);
    for c in pcm.chunks_exact(2) {
        let s = i16::from_le_bytes([c[0], c[1]]) as f32 * gain;
        let s = s.clamp(-32768.0, 32767.0).round_ties_even().clamp(-32768.0, 32767.0);
        out.extend_from_slice(&(s as i16).to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// MicLevelTracker（ui 用的电平条）
// ---------------------------------------------------------------------------

/// 给 UI / 回声排查用的电平（dBFS + 线性 RMS，同一次遍历算出）。
#[derive(Debug)]
pub struct MicLevelTracker {
    st: Mutex<LevelState>,
}

#[derive(Debug)]
struct LevelState {
    db: f64,
    rms: f64,
}

impl MicLevelTracker {
    pub fn new() -> Self {
        Self {
            st: Mutex::new(LevelState { db: -90.0, rms: 0.0 }),
        }
    }

    /// 用一块 PCM16 更新电平（f32 域，与 numpy 实现同精度档）。
    pub fn update(&self, pcm16: &[u8]) {
        if pcm16.len() < 2 {
            return;
        }
        let n = pcm16.len() / 2;
        // np.square 后 np.mean：f32 累加
        let mut sum = 0f32;
        for c in pcm16.chunks_exact(2) {
            let f = i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0;
            sum += f * f;
        }
        let rms = (sum / n as f32).sqrt() as f64;
        let db = if rms > 0.0 {
            (-90.0f64).max(20.0 * rms.log10())
        } else {
            -90.0
        };
        let mut st = self.st.lock().unwrap();
        st.rms = rms;
        st.db = db;
    }

    /// dBFS（下限 -90）。
    pub fn db(&self) -> f64 {
        self.st.lock().unwrap().db
    }

    /// 线性 RMS（0~1，除以 32768 归一）。
    pub fn rms(&self) -> f64 {
        self.st.lock().unwrap().rms
    }
}

impl Default for MicLevelTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 共享状态
// ---------------------------------------------------------------------------

/// mic 线程与 `AudioIo` 共享的运行时状态（音频门面两侧都碰它）。
struct MicCtl {
    /// 采集循环运行中（`stop_mic` 置 false；speaker 的 far 保活也看它）。
    running: AtomicBool,
    /// 静音（门控置零照发，音频时钟不停）。
    muted: AtomicBool,
    bytes_total: AtomicU64,
    started_at: Mutex<Option<Instant>>,
    /// 采集间隙 >80ms 记录（诊断用）——10s 健康检查消费。
    gap_events: Mutex<Vec<f64>>,
    /// 采集断裂深压窗口（时间戳）。
    duck_emergency_until: Mutex<Option<Instant>>,
    /// AEC 锁定状态缓存 (时刻, 锁定)。
    aec_lock_cache: Mutex<(Option<Instant>, bool)>,
}

impl MicCtl {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            bytes_total: AtomicU64::new(0),
            started_at: Mutex::new(None),
            gap_events: Mutex::new(Vec::new()),
            duck_emergency_until: Mutex::new(None),
            aec_lock_cache: Mutex::new((None, true)),
        }
    }
}

/// speaker 线程与门控判断共享的状态（播放窗口 + 音量）。
#[derive(Default)]
struct SpeakerShared {
    /// 回声门控窗口：播放将结束的时刻（mono）。登记在起播前。
    play_until: Mutex<Option<Instant>>,
    /// 扬声器软件音量（0-100），播放线程下一块即生效。
    volume: AtomicU8,
}

/// 扬声器是否正在（或刚结束）出声——麦克风门控用。
fn playing(shared: &SpeakerShared, tail_s: f64) -> bool {
    let until = *shared.play_until.lock().unwrap();
    until.is_some_and(|t| Instant::now() < t + Duration::from_secs_f64(tail_s))
}

/// mic 循环从 Config 带走的门控参数。
#[derive(Clone)]
struct MicCfg {
    gate_enabled: bool,
    gate_tail_s: f64,
    duck: f64,
}

// ---------------------------------------------------------------------------
// 播放线程队列（有界 120，满时丢最旧）
// ---------------------------------------------------------------------------

enum SpkItem {
    /// PCM16 块 + 声明的采样率。
    Data(Vec<u8>, u32),
    /// 停止哨兵（`stop_speaker`）。
    Stop,
    /// 冲刷哨兵（打断快停）：丢当前合块 + 重建 aplay（立刻静音），
    /// 播放线程继续运行。
    Flush,
}

impl SpkItem {
    /// 队列水位统计用的数据字节数（哨兵为 0）。
    fn data_len(&self) -> usize {
        match self {
            SpkItem::Data(p, _) => p.len(),
            _ => 0,
        }
    }
}

struct SpkQueue {
    items: Mutex<VecDeque<SpkItem>>,
    cv: Condvar,
    dropped: AtomicU64,
}

impl SpkQueue {
    fn new() -> Self {
        Self {
            items: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            dropped: AtomicU64::new(0),
        }
    }

    /// 入队（有界丢最旧保实时：块数上限 + 字节水位双限）。
    ///
    /// 字节水位：服务器以 ~1.5x 实时 burst 下发，只按块数限会积压数秒
    /// 音频（实测打断后停不下来）；按字节限到 ~3s，起播快的同时排空
    /// 上限可控。
    fn push_data(&self, pcm: Vec<u8>, rate: u32) {
        let mut q = self.items.lock().unwrap();
        let mut bytes: usize = q.iter().map(|i| i.data_len()).sum();
        bytes += pcm.len();
        while q.len() >= PLAY_QUEUE_CAP || bytes > MAX_QUEUE_BYTES {
            match q.pop_front() {
                Some(SpkItem::Data(p, _)) => {
                    bytes -= p.len();
                    let d = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 10 == 1 {
                        log::warn!("播放队列溢出，已丢弃 {} 块", d);
                    }
                }
                _ => break, // Stop/Flush 哨兵不吞
            }
        }
        q.push_back(SpkItem::Data(pcm, rate));
        drop(q);
        self.cv.notify_one();
    }

    fn push_stop(&self) {
        self.items.lock().unwrap().push_back(SpkItem::Stop);
        self.cv.notify_one();
    }

    /// 清队并注入 Flush 哨兵（打断快停：丢旧块 + speaker 线程重建 aplay）。
    fn push_flush(&self) {
        let mut q = self.items.lock().unwrap();
        q.clear();
        q.push_back(SpkItem::Flush);
        drop(q);
        self.cv.notify_one();
    }

    /// 等一项数据（超时返回 `None` = 本拍空闲，走 far 保活）。
    fn get(&self, timeout: Duration) -> Option<SpkItem> {
        let mut q = self.items.lock().unwrap();
        if let Some(item) = q.pop_front() {
            return Some(item);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = self.cv.wait_timeout(q, deadline - now).unwrap();
            q = guard;
            if let Some(item) = q.pop_front() {
                return Some(item);
            }
        }
    }

    fn get_nowait(&self) -> Option<SpkItem> {
        self.items.lock().unwrap().pop_front()
    }

    /// 清队（`stop_speaker`：未播的直接丢）。
    fn clear(&self) {
        self.items.lock().unwrap().clear();
    }
}

// ---------------------------------------------------------------------------
// 采集线程
// ---------------------------------------------------------------------------

struct MicCtx {
    capture: Arc<Mutex<Box<dyn CaptureSource>>>,
    ctl: Arc<MicCtl>,
    spk_shared: Arc<SpeakerShared>,
    cfg: MicCfg,
    aec: Option<Arc<Mutex<AecImpl>>>,
    /// 近端软件 AGC（AEC 后、duck 前；arecord 裸路径补 MaixPy 管线的
    /// 隐藏回声抑制，见 [`crate::agc`]）。
    agc: Option<Agc>,
    timing: Arc<Timing>,
    level: Arc<MicLevelTracker>,
    on_unit: Box<dyn FnMut(Vec<u8>, Instant) + Send>,
    dump: Option<File>,
    dump_path: String,
}

/// 线程提权：SCHED_RR prio 10（root 下成功）；EPERM 回退 nice(-5)。
/// UI+全模态把 CPU 打满时音频线程被饿会丢数据（实测 10s 内 17 次间隙、
/// 最大 1.9s），far/near 时间轴一断，AEC/子带抑制全部失效。
#[cfg(target_os = "linux")]
fn promote_thread() {
    let param = libc::sched_param { sched_priority: 10 };
    let r = unsafe { libc::sched_setscheduler(0, libc::SCHED_RR, &param) };
    if r == 0 {
        log::info!("线程已提权（SCHED_RR prio 10）");
        return;
    }
    let e = std::io::Error::last_os_error();
    log::info!("SCHED_RR 提权失败（{e}），回退 nice(-5)");
    unsafe { libc::nice(-5) };
    log::info!("线程已提权（nice -5）");
}

#[cfg(not(target_os = "linux"))]
fn promote_thread() {}

/// 带超时的 join（Rust 标准库没有；借一个辅助线程等）。
fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) -> bool {
    let (tx, rx) = mpsc::channel();
    let _helper = thread::spawn(move || {
        let _ = handle.join();
        let _ = tx.send(());
    });
    rx.recv_timeout(timeout).is_ok()
}

/// 回声门控是否激活（硬门控：播放期间直接置零）。
fn gate_active(cfg: &MicCfg, muted: bool, shared: &SpeakerShared) -> bool {
    muted || (cfg.gate_enabled && playing(shared, cfg.gate_tail_s))
}

/// ducking 窗口是否激活。
///
/// 尾沿 2.0s：`playing()` 判断的是 put 时的队列时间轴，CPU 过载时 aplay
/// 积压会让实际出声晚于预估结束 0.5~2s（实测 UI+全模态下 AI 复述的回声
/// 尾巴漏进服务端音频流触发自言自语循环），1.2s 仍不够。用户插话音频被压
/// 低但服务端仍可听到，可接受。
fn duck_active(cfg: &MicCfg, shared: &SpeakerShared) -> bool {
    playing(shared, cfg.gate_tail_s.max(2.0))
}

/// duck 压制。deep=true 时用一半增益（回声抑制失效窗口的兜底深压）。
fn duck_scale(pcm: &[u8], duck: f64, deep: bool) -> Vec<u8> {
    let g = if deep { duck * 0.5 } else { duck } as f32;
    scale_pcm16(pcm, g)
}

/// 深压判定：采集断裂紧急窗内，或 AEC 未锁定（抑制不工作）时。
///
/// AEC 锁定状态 0.5s 缓存一次（stats() 构造 JSON 有开销）。
fn duck_deep_now(aec: Option<&Arc<Mutex<AecImpl>>>, ctl: &MicCtl, now: Instant) -> bool {
    if let Some(until) = *ctl.duck_emergency_until.lock().unwrap() {
        if now < until {
            return true;
        }
    }
    let mut cache = ctl.aec_lock_cache.lock().unwrap();
    let (t, mut locked) = *cache;
    if t.is_none_or(|t| now.duration_since(t).as_secs_f64() > 0.5) {
        locked = match aec {
            Some(a) => a
                .lock()
                .unwrap()
                .stats()
                .get("locked")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            None => true,
        };
        *cache = (Some(now), locked);
    }
    !locked
}

/// 采集循环（`_mic_loop`）：record 轮询 -> 门控/AEC/duck -> 1s unit 凑包。
fn mic_loop(mut ctx: MicCtx) {
    promote_thread();
    let started_at = *ctx.ctl.started_at.lock().unwrap();
    let mut acc: Vec<u8> = Vec::with_capacity(UNIT_BYTES * 2);
    let mut idle_polls = 0u32;
    let mut last_health_log = Instant::now();
    let mut t_last_data: Option<Instant> = None;
    let mut buf = vec![0u8; 8192];
    let mut gate_prev = false;
    while ctx.ctl.running.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        let data: Vec<u8> = {
            let mut cap = ctx.capture.lock().unwrap();
            match cap.read_pcm16(&mut buf) {
                Ok(0) => Vec::new(),
                Ok(n) => buf[..n].to_vec(),
                Err(e) => {
                    log::error!("record() 异常：{e}");
                    drop(cap);
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        };

        if !data.is_empty() {
            idle_polls = 0;
            let now = Instant::now();
            if let Some(tld) = t_last_data {
                let dt = now.duration_since(tld).as_secs_f64();
                if dt > 0.08 {
                    ctx.ctl.gap_events.lock().unwrap().push(dt);
                    if dt > 0.3 {
                        // 采集断裂 >300ms：驱动层播放挤占。far 时钟由 speaker
                        // keepalive 维持连续，near 落后会让 AEC 的 far 参考从此
                        // 错位（抑制实际失效）。根治：给 near 补等长静音钟，
                        // 锁定/G 全保留，抑制无缝继续；深压只盖恢复瞬间。
                        if let Some(a) = &ctx.aec {
                            a.lock().unwrap().resync(dt);
                        }
                        *ctx.ctl.duck_emergency_until.lock().unwrap() =
                            Some(now + Duration::from_millis(500));
                        log::warn!("采集断裂 {:.0}ms，已补时钟+深压 0.5s", dt * 1000.0);
                    }
                }
            }
            t_last_data = Some(now);
            let mut data = data;
            let gate = gate_active(&ctx.cfg, ctx.ctl.muted.load(Ordering::Relaxed), &ctx.spk_shared);
            if gate != gate_prev {
                gate_prev = gate;
                o5_perf::trace_ev!("gate", on = gate);
            }
            if gate {
                // 静音 / 回声门控：全零但照发（音频时钟不能停）。near 也要
                // 过 AEC（零进零出）：far keepalive 一直在推进，near 时间轴
                // 停走会让 far 环形缓冲覆盖掉对齐窗——解除静音后"环形缓冲
                // 读取失败"、AEC 状态错乱（03 实测 mute 切换后锁定全掉）。
                data.fill(0);
                if let Some(a) = &ctx.aec {
                    let near = bytes_to_i16(&data);
                    data = i16_to_bytes(&a.lock().unwrap().cancel(&near));
                }
                } else {
                    // 回声消除：远端参考（扬声器写出流）在此对冲公放回灌
                    if let Some(a) = &ctx.aec {
                        let near = bytes_to_i16(&data);
                        data = i16_to_bytes(&a.lock().unwrap().cancel(&near));
                    }
                    // 近端 AGC（AEC 后、duck 前）：arecord 裸路径补 MaixPy 管线的
                    // 隐藏回声抑制——近场回声（人声 4~10 倍电平）快压慢放，
                    // 中等人声不动。阈值按 02 机 dump 定标（人声峰 4037 /
                    // 回声均值 ~5000，取 4500 分界）。
                    if let Some(g) = ctx.agc.as_mut() {
                        let mut near = bytes_to_i16(&data);
                        g.process(&mut near);
                        data = i16_to_bytes(&near);
                    }
                    // ducking：播放期间压制 AEC 残余回声（近场人声能量高不受影响）
                if ctx.cfg.duck < 1.0 && duck_active(&ctx.cfg, &ctx.spk_shared) {
                    let deep = duck_deep_now(ctx.aec.as_ref(), &ctx.ctl, now);
                    data = duck_scale(&data, ctx.cfg.duck, deep);
                }
            }
            ctx.ctl.bytes_total.fetch_add(data.len() as u64, Ordering::Relaxed);
            ctx.level.update(&data);
            if let Some(f) = ctx.dump.as_mut() {
                let _ = f.write_all(&data);
            }
            acc.extend_from_slice(&data);
            while acc.len() >= UNIT_BYTES {
                let unit: Vec<u8> = acc.drain(..UNIT_BYTES).collect();
                let t_recorded = ctx.timing.unit_recorded();
                // _emit：unit 回调异常只记日志，绝不打断采集节奏
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (ctx.on_unit)(unit, t_recorded)
                }));
                if r.is_err() {
                    log::error!("unit 回调异常");
                }
            }
        } else {
            idle_polls += 1;
            if idle_polls == 100 {
                // ~2.5s 无数据
                log::warn!("麦克风持续无数据（已轮询 100 次空）");
            }
        }

        // 节流：每轮总时长 ~25ms（read 短超时返回，读的是缓存）
        let elapsed = t0.elapsed();
        let interval = Duration::from_secs_f64(POLL_INTERVAL_S);
        if elapsed < interval {
            thread::sleep(interval - elapsed);
        }

        if last_health_log.elapsed().as_secs_f64() > 10.0 {
            last_health_log = Instant::now();
            {
                let mut gaps = ctx.ctl.gap_events.lock().unwrap();
                if !gaps.is_empty() {
                    let mx = gaps.iter().cloned().fold(0.0f64, f64::max);
                    let sum: f64 = gaps.iter().sum();
                    log::warn!(
                        "采集间隙：10s 内 {} 次 >80ms（最大 {:.0}ms，合计 {:.2}s）",
                        gaps.len(),
                        mx * 1000.0,
                        sum
                    );
                    gaps.clear();
                }
            }
            if let Some(a) = &ctx.aec {
                let st = a.lock().unwrap().stats();
                let num = |k: &str| st.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let er = st
                    .get("erle_db")
                    .or_else(|| st.get("suppress_db"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                if st.get("adapted").and_then(|v| v.as_bool()).unwrap_or(false) {
                    log::info!("[aec] 抑制 {:.1}dB 回声延迟 {:.0}ms", er, num("delay_ms"));
                } else if num("far_s") > 1.0 {
                    // 有远端参考却没收敛：把关键状态亮出来，现场排查用。
                    // 锁定=false 是互相关没找到峰（延迟超窗/回声太弱），
                    // 锁定=true 但抑制<6dB 是收敛慢或延迟微偏。
                    log::info!(
                        "[aec] 未收敛 锁定={} 延迟={:.0}ms 抑制={:.1}dB far={:.1}s",
                        st.get("locked").and_then(|v| v.as_bool()).unwrap_or(false),
                        num("delay_ms"),
                        er,
                        num("far_s")
                    );
                }
            }
            let expect = started_at
                .map(|t| t.elapsed().as_secs_f64() * f64::from(INPUT_RATE) * 2.0)
                .unwrap_or(0.0);
            let got = ctx.ctl.bytes_total.load(Ordering::Relaxed) as f64;
            if expect > (UNIT_BYTES * 2) as f64 && got < expect * 0.9 {
                log::warn!(
                    "采集落后：已收 {:.1}s，预期 {:.1}s（录音缓冲可能欠载）",
                    got / (f64::from(INPUT_RATE) * 2.0),
                    expect / (f64::from(INPUT_RATE) * 2.0)
                );
            }
        }
    }
    // 线程退出：关转储（_close_dump）
    if ctx.dump.take().is_some() {
        log::info!(
            "麦克风转储完成: {}（16k 单声道 S16LE，可直接导入 Audacity）",
            ctx.dump_path
        );
    }
}

// ---------------------------------------------------------------------------
// 播放线程
// ---------------------------------------------------------------------------

struct SpeakerCtx {
    queue: Arc<SpkQueue>,
    shared: Arc<SpeakerShared>,
    near_ctl: Arc<MicCtl>,
    aec: Option<Arc<Mutex<AecImpl>>>,
    timing: Arc<Timing>,
    factory: Arc<Mutex<SinkFactory>>,
}

/// 播放循环（`Speaker._loop`）：有界队列 -> 合块 -> 增益 -> 写通道 -> 喂 far。
fn speaker_loop(ctx: SpeakerCtx) {
    // 播放线程提权（同 mic 线程）：高负载下 aplay 写入被饿会积压，
    // 实际出声尾巴比 _play_until 预估晚 0.5~2s——duck/gate 尾沿盖不住，
    // TTS 结尾漏进服务端（自触发循环路径）。
    promote_thread();
    let SpeakerCtx {
        queue,
        shared,
        near_ctl,
        aec,
        timing,
        factory,
    } = ctx;
    let mut sink: Option<Box<dyn PlaybackSink>> = None;
    let mut device_rate: Option<u32> = None;
    let mut closed = false;
    let mut far_wall: Option<Instant> = None; // far 保活基准（上次覆盖到的墙钟）
    let mut servo = FarServo::new(); // far 时钟伺服（吸收采集/播放速率差）
    let mut last_activity: Option<Instant> = None;
    let mut last_play_end: Option<Instant> = None; // 上一块写完返回时刻（块间隙埋点用）

    while !closed {
        let item = match queue.get(Duration::from_millis(100)) {
            Some(i) => i,
            None => {
                // 空闲也要推进 AEC far 时钟（推等长静音，内部有排水护栏）：
                // 对齐映射锚定在 far 推进位置上，一停推，句尾回声（aplay
                // 排水 + 混响）就对不上参考，~900ms 后还会被判 far 欠载直接
                // 透传——TTS 结尾几个字漏进麦克风的主因。get 超时 0.1s 即为
                // 保活节拍。
                far_keepalive(aec.as_ref(), &near_ctl, &shared, &mut far_wall, &mut servo);
                continue;
            }
        };
        let (pcm16, rate) = match item {
            SpkItem::Data(p, r) => (p, r),
            SpkItem::Stop => break,
            SpkItem::Flush => {
                // 打断快停：杀 aplay（Drop 连管道积压一起丢），下一块
                // 到达时 ensure_sink 重建；门控/duck 窗口立即关闭——
                // 用户马上要说话，别再压麦克风。far 时钟不断（keepalive
                // 续推，AEC 状态保留）。
                sink = None;
                device_rate = None;
                *shared.play_until.lock().unwrap() = None;
                log::info!("打断：播放缓冲已清空（aplay 重建）");
                o5_perf::trace_ev!("flush");
                continue;
            }
        };
        let need_fade = ensure_sink(&factory, &mut sink, &mut device_rate, rate);
        // aplay（重）建后的首块做头部淡入：新流从任意振幅起播会 click
        // （打断快停后偶发爆音的来源之一）。覆盖所有重建路径：首次
        // 启动 / Flush 两处 / 写入错误杀 sink / 速率切换。
        let dev_rate = match device_rate {
            Some(r) => r,
            None => {
                // 播放通道没建立，丢块不刷栈
                far_keepalive(aec.as_ref(), &near_ctl, &shared, &mut far_wall, &mut servo);
                continue;
            }
        };
        let now0 = Instant::now();
        let is_burst_start = match last_activity {
            None => true,
            Some(t) => now0.duration_since(t).as_secs_f64() > BURST_GAP_S,
        };
        // 合块：凑 ~300ms 一播。每块单独写时块间设备瞬时断流，
        // 是"劈里啪啦"爆音的主源；合块把边界数降一个数量级。
        let mut block = resample_if_needed(&pcm16, rate, dev_rate);
        let max_bytes = (f64::from(dev_rate) * 2.0 * MAX_BLOCK_S) as usize;
        let mut flushed = false;
        if is_burst_start {
            // 突发首块先攒前导音频再起播：供流抖动时短暂中断不再欠载
            let lead_bytes = (f64::from(dev_rate) * 2.0 * LEAD_S) as usize;
            let deadline = now0 + Duration::from_millis(150);
            while block.len() < lead_bytes && Instant::now() < deadline {
                match queue.get(Duration::from_millis(20)) {
                    None => continue,
                    Some(SpkItem::Data(nxt, nrate)) => {
                        if nrate != rate {
                            continue;
                        }
                        block.extend_from_slice(&resample_if_needed(&nxt, nrate, dev_rate));
                    }
                    Some(SpkItem::Stop) => {
                        closed = true;
                        break;
                    }
                    Some(SpkItem::Flush) => {
                        flushed = true;
                        break;
                    }
                }
            }
            if !closed && block.len() < lead_bytes {
                timing.underrun(); // 攒满 0.15s 还是没够前导：供流欠载
            }
        }
        while !flushed && block.len() < max_bytes {
            match queue.get_nowait() {
                None => break,
                Some(SpkItem::Data(nxt, nrate)) => {
                    if nrate != rate {
                        continue;
                    }
                    block.extend_from_slice(&resample_if_needed(&nxt, nrate, dev_rate));
                }
                Some(SpkItem::Stop) => {
                    closed = true;
                    break;
                }
                Some(SpkItem::Flush) => {
                    flushed = true;
                    break;
                }
            }
        }
        if flushed {
            // 合块途中收到打断：整块作废，杀 aplay 丢弃管道积压
            sink = None;
            device_rate = None;
            *shared.play_until.lock().unwrap() = None;
            log::info!("打断：合块作废（aplay 重建）");
            continue;
        }
        // 音量增益（软件音量；aplay 是连续流不做块缘淡变——淡变反而会造成
        // 周期性凹陷）
        let gain = vol_gain(shared.volume.load(Ordering::Relaxed)) as f32;
        let mut data = scale_pcm16(&block, gain);
        if need_fade {
            fade_in_head(&mut data, dev_rate);
        }
        let dur = data.len() as f64 / 2.0 / f64::from(dev_rate);
        // 写通道阻塞到本块放完，门控窗口必须在起播前登记
        // （登记晚了 playing() 会在放音期间返回 false、放完后误报 true）。
        // 基准取"此刻"（前导/合块等待之后），与 Python 的 time.monotonic()+dur 对齐。
        {
            let mut pu = shared.play_until.lock().unwrap();
            let end = Instant::now() + Duration::from_secs_f64(dur);
            *pu = Some(pu.map_or(end, |t| t.max(end)));
        }
        // 突发首块：起播前上报"扬声器开始播放"（返回后再报就晚了整块时长）
        let now_burst = Instant::now();
        let burst_now = match last_activity {
            None => true,
            Some(t) => now_burst.duration_since(t).as_secs_f64() > BURST_GAP_S,
        };
        if burst_now {
            timing.playback_started();
        }
        let t_play = Instant::now();
        if let Some(lpe) = last_play_end {
            // 同一突发内块与块之间本应无缝衔接，>30ms 就是听得见的断
            let gap = t_play.duration_since(lpe).as_secs_f64();
            if gap < BURST_GAP_S && gap > 0.03 {
                timing.play_gap(gap * 1000.0);
            }
        }
        let err = match sink.as_mut() {
            Some(s) => s.write_pcm16(&data).err(),
            None => None,
        };
        if err.is_none() {
            // 实际发声的流 = AEC 远端参考
            push_far(aec.as_ref(), &near_ctl, dev_rate, &data, &mut far_wall, &mut servo);
        }
        timing.play(t_play.elapsed().as_secs_f64() * 1000.0);
        let now_end = Instant::now();
        let ahead_ms = shared
            .play_until
            .lock()
            .unwrap()
            .map_or(0, |t| t.saturating_duration_since(now_end).as_millis());
        o5_perf::trace_ev!(
            "play",
            blk_ms = (dur * 1000.0) as u64,
            write_ms = now_end.duration_since(t_play).as_millis(),
            ahead_ms = ahead_ms,
            gap_ms = last_play_end.map_or(0, |l| t_play.duration_since(l).as_millis()),
            ok = err.is_none()
        );
        last_play_end = Some(now_end);
        last_activity = Some(now_end); // _mark_played
        if let Some(e) = err {
            // aplay 挂了：关掉，下一轮 ensure_sink 会重建
            log::warn!("播放写入: {e}");
            sink = None;
        }
    }
    log::info!("扬声器线程退出");
}

/// 建/换播放通道（`_ensure_player`）：目标速率不在支持表里回退 48000。
/// 返回是否（重）建了通道——首块淡入的触发条件。
fn ensure_sink(
    factory: &Arc<Mutex<SinkFactory>>,
    sink: &mut Option<Box<dyn PlaybackSink>>,
    device_rate: &mut Option<u32>,
    rate: u32,
) -> bool {
    let target = if SUPPORTED_OUTPUT_RATES.contains(&rate) {
        rate
    } else {
        FALLBACK_RATE
    };
    if *device_rate == Some(target) && sink.is_some() {
        return false;
    }
    *sink = None; // _close_sinks（Drop 杀子进程）
    let built = factory.lock().unwrap()(target);
    match built {
        Ok(s) => {
            log::info!("播放通道就绪: {target} Hz");
            o5_perf::trace_ev!("sink_open", rate = target);
            *device_rate = Some(target);
            *sink = Some(s);
            true
        }
        Err(e) => {
            log::warn!("播放通道建立失败（{e}），本块丢弃");
            *device_rate = None;
            false
        }
    }
}

fn resample_if_needed(pcm16: &[u8], rate: u32, device_rate: u32) -> Vec<u8> {
    if rate == device_rate {
        pcm16.to_vec()
    } else {
        resample_pcm16(pcm16, rate, device_rate)
    }
}

/// 播放通道重建后的首块头部 ~3ms 线性淡入：aplay 新流从任意振幅起播
/// 会 click。只淡重建后第一块——后续连续流不做淡变（周期性淡变反而
/// 造成凹陷，见合块注释）。
fn fade_in_head(data: &mut [u8], dev_rate: u32) {
    let n = ((f64::from(dev_rate) * 0.003) as usize)
        .min(data.len() / 2)
        .max(1);
    for (i, s) in data.chunks_exact_mut(2).take(n).enumerate() {
        let g = (i as f32 + 1.0) / n as f32;
        let v = i16::from_le_bytes([s[0], s[1]]);
        let b = ((v as f32 * g) as i16).to_le_bytes();
        s[0] = b[0];
        s[1] = b[1];
    }
}

/// 播放流 -> AEC 远端参考（16k；写出成功才喂，写失败的数据没出声）。
fn push_far(
    aec: Option<&Arc<Mutex<AecImpl>>>,
    near_ctl: &MicCtl,
    device_rate: u32,
    data: &[u8],
    far_wall: &mut Option<Instant>,
    servo: &mut FarServo,
) {
    let aec = match aec {
        Some(a) => a,
        None => return,
    };
    if data.is_empty() {
        return;
    }
    if !near_ctl.running.load(Ordering::Relaxed) {
        // 近端停跑：far 时钟同步冻结（两端同启同停，位置差不累积）
        *far_wall = None;
        return;
    }
    let far = if device_rate == INPUT_RATE {
        data.to_vec()
    } else {
        resample_pcm16(data, device_rate, INPUT_RATE)
    };
    let far_i16 = bytes_to_i16(&far);
    // far 时钟伺服：采集/播放两路时钟有 ~0.5% 相对速率差（02 机 dump：
    // lag 单调漂 5.5ms/s + 突发结束后跳变），把 far 流按估计比率微重
    // 采样，让 far 位置贴着 near 时钟走，锁定不再漂移。far 只是参考
    // 信号，音质无关紧要。
    let far_i16 = {
        let mut guard = aec.lock().unwrap();
        servo.observe(guard.clocks().1);
        let out = servo.process(&far_i16);
        guard.push_far(&out);
        out
    };
    // 保活基准 = far 末梢"放完"的时刻（写入即视为从现在起占用块时长）。
    // 这样排水护栏打开后补的静音正好接在块尾之后，不重叠不缺段。
    // max 单调化：连续供流时小块提前于上一块的结束时刻到达，
    // 基准倒退会让保活多补静音（far 超前，锚点修复后无害但没必要）。
    let wall =
        Instant::now() + Duration::from_secs_f64(far_i16.len() as f64 / f64::from(INPUT_RATE));
    if far_wall.is_none_or(|w| wall > w) {
        *far_wall = Some(wall);
    }
}

/// 空闲时按墙钟给 AEC 推等长静音，让 far 时钟与近端同步推进。
///
/// AEC 的约定是"静音期间也要推全零块，不要断流"：far 的推进位置就是
/// 对齐锚点，断流后 ①句尾回声（aplay 缓冲排水 + 混响）匹配不到参考、
/// 消不掉；②欠载判定直接透传；③下一句起播要重新锁延迟 + 预热，每句
/// 都是冷启动。保活后滤波器权重跨句连续。
///
/// 排水护栏：距登记的播放结束（play_until）不足余量时不补——aplay
/// 可能还在排积压音频，此时推零等于在 far 里插"假静音"，把 far 与
/// 实际发声的对应关系剪坏。播放中（含句间空档）far 靠下一块续推，
/// 排空之后才轮到保活接管。
fn far_keepalive(
    aec: Option<&Arc<Mutex<AecImpl>>>,
    near_ctl: &MicCtl,
    shared: &SpeakerShared,
    far_wall: &mut Option<Instant>,
    servo: &mut FarServo,
) {
    let aec = match aec {
        Some(a) => a,
        None => return,
    };
    if !near_ctl.running.load(Ordering::Relaxed) {
        // 近端（麦克风）不在跑：far 时钟同步冻结——不推零，否则空闲期
        // far 独自推进，两端位置差累积进延迟估计。基准作废，恢复时重起。
        *far_wall = None;
        return;
    }
    let now = Instant::now();
    let play_until = *shared.play_until.lock().unwrap();
    if play_until.is_some_and(|t| now < t + Duration::from_secs_f64(FAR_DRAIN_MARGIN_S)) {
        return; // 可能还在排水：这一拍不补，等下一拍
    }
    let wall = match *far_wall {
        None => {
            *far_wall = Some(now);
            return;
        }
        Some(w) => w,
    };
    let dt = now.saturating_duration_since(wall).as_secs_f64();
    if dt < 0.04 {
        return; // 粒度之下不推（避免高频小包开销）
    }
    let n = (dt * f64::from(INPUT_RATE)) as i64;
    if n <= 0 {
        return;
    }
    let silence = vec![0i16; n as usize];
    // keepalive 也过时钟伺服（far 全流统一速率，位置闭环收敛）
    let pushed = {
        let mut guard = aec.lock().unwrap();
        servo.observe(guard.clocks().1);
        let out = servo.process(&silence);
        guard.push_far(&out);
        out.len()
    };
    // 按实际推入的样本量推进基准（不取 now），墙钟误差不累积
    *far_wall = Some(wall + Duration::from_secs_f64(pushed as f64 / f64::from(INPUT_RATE)));
}

// ---------------------------------------------------------------------------
// AudioIo 门面
// ---------------------------------------------------------------------------

/// 麦克风采集 + 扬声器播放的组合门面。
pub struct AudioIo {
    mic_cfg: MicCfg,
    mic_dump: bool,
    mic_dump_path: String,
    /// 近端软件 AGC 开关（audio.nearAgc；构造 MicCtx 时实例化）。
    near_agc: bool,
    timing: Arc<Timing>,
    /// 回声消除：麦克风近端 - 扬声器远端参考 -> 残余上行。
    aec: Option<Arc<Mutex<AecImpl>>>,
    /// 生效的模式名（"nlms"/"subband"/"off"，诊断用）。
    aec_mode: String,
    level: Arc<MicLevelTracker>,
    ctl: Arc<MicCtl>,
    mic_thread: Mutex<Option<JoinHandle<()>>>,
    /// 采集通道（mic 线程按块借用；stop 后由本结构继续持有到 drop）。
    capture: Arc<Mutex<Box<dyn CaptureSource>>>,
    /// 播放队列（stop_speaker/start 周期换新实例；旧线程消费旧队列退出）。
    queue: Mutex<Arc<SpkQueue>>,
    spk_shared: Arc<SpeakerShared>,
    spk_thread: Mutex<Option<JoinHandle<()>>>,
    spk_started: AtomicBool,
    sink_factory: Arc<Mutex<SinkFactory>>,
}

impl AudioIo {
    /// 按配置组装（对应 main.py 里 `AudioIO(...)` 的接线）。
    pub fn new(cfg: &Config, timing: Arc<Timing>, capture: Box<dyn CaptureSource>) -> Self {
        // echoMode: "nlms"（波形域自适应滤波，波形相关的链路才有效）
        //          "subband"（STFT 子带包络抑制：扬声器非线性+AGC 链路上
        //           波形零相关（LS 上限 0dB），只能按包络压）
        //          "off"（关闭）。空 = 按 echoCancel 布尔回退（True->nlms）。
        let mode = if cfg.audio_echo_mode.is_empty() {
            if cfg.audio_echo_cancel {
                "nlms"
            } else {
                "off"
            }
        } else {
            cfg.audio_echo_mode.as_str()
        };
        let max_delay_ms = cfg.audio_echo_max_delay_ms.max(100.0);
        let dump_dir = if cfg.audio_echo_debug_dump {
            Some(PathBuf::from(AEC_DUMP_DIR))
        } else {
            None
        };
        let (aec, aec_mode) = match mode {
            "nlms" => {
                // 延迟搜索窗要盖住 aplay 管道积压 + 设备内部缓冲 + 声学路径：
                // 合块播放下真实延迟轻松超过 500ms，窗口不够就永远锁不上。
                let params = NlmsAecParams {
                    sample_rate: INPUT_RATE,
                    max_delay_ms,
                    dump_dir,
                    ..Default::default()
                };
                let taps = params.taps;
                let aec = NlmsAec::new(params);
                log::info!(
                    "回声消除已启用（NLMS，{taps} taps，延迟窗 {:.0}ms）",
                    max_delay_ms
                );
                (Some(Arc::new(Mutex::new(AecImpl::Nlms(aec)))), "nlms")
            }
            "subband" => {
                let aec = SubbandSuppressor::new(SubbandParams {
                    sample_rate: INPUT_RATE,
                    max_delay_ms,
                    dump_dir,
                    ..Default::default()
                });
                log::info!("回声抑制已启用（子带包络，延迟窗 {:.0}ms）", max_delay_ms);
                (
                    Some(Arc::new(Mutex::new(AecImpl::Subband(aec)))),
                    "subband",
                )
            }
            _ => (None, "off"),
        };
        Self {
            mic_cfg: MicCfg {
                gate_enabled: cfg.audio_mic_gate_while_playing,
                gate_tail_s: cfg.audio_gate_tail_ms as f64 / 1000.0,
                duck: cfg.audio_mic_duck_while_playing.clamp(0.0, 1.0),
            },
            mic_dump: cfg.audio_mic_debug_dump,
            mic_dump_path: cfg.audio_mic_dump_path.clone(),
            near_agc: cfg.audio_near_agc,
            timing,
            aec,
            aec_mode: aec_mode.to_string(),
            level: Arc::new(MicLevelTracker::new()),
            ctl: Arc::new(MicCtl::new()),
            mic_thread: Mutex::new(None),
            capture: Arc::new(Mutex::new(capture)),
            queue: Mutex::new(Arc::new(SpkQueue::new())),
            spk_shared: Arc::new(SpeakerShared {
                play_until: Mutex::new(None),
                volume: AtomicU8::new(cfg.audio_output_volume.min(100)),
            }),
            spk_thread: Mutex::new(None),
            spk_started: AtomicBool::new(false),
            sink_factory: Arc::new(Mutex::new(platform_sink_factory())),
        }
    }

    /// 启动扬声器线程（幂等；对应 `start_speaker`）。
    pub fn start(&mut self) {
        if self.spk_started.load(Ordering::Relaxed) {
            return;
        }
        // 换新队列：上一线程 join 超时未死时它消费的是旧队列（已收到 Stop），
        // 不会与新线程抢新队列。
        let queue = Arc::new(SpkQueue::new());
        *self.queue.lock().unwrap() = Arc::clone(&queue);
        self.spk_started.store(true, Ordering::Relaxed);
        let ctx = SpeakerCtx {
            queue,
            shared: Arc::clone(&self.spk_shared),
            // near_active：far 时钟跟随麦克风同启同停（AEC 两端位置差不累积）
            near_ctl: Arc::clone(&self.ctl),
            aec: self.aec.clone(),
            timing: Arc::clone(&self.timing),
            factory: Arc::clone(&self.sink_factory),
        };
        let handle = thread::Builder::new()
            .name("o5-speaker".to_string())
            .spawn(move || speaker_loop(ctx))
            .expect("spawn speaker thread");
        *self.spk_thread.lock().unwrap() = Some(handle);
    }

    /// 播放块入队（drop-oldest，cap 120）；rate ∈ {16000,24000,48000}，
    /// 其他速率由播放线程走 FALLBACK_RATE=48000 + 线性重采样（等价于调用方
    /// 先 [`resample_pcm16`] 到 48k 再传入）。
    pub fn put_speaker(&self, pcm16: Vec<u8>, rate: u32) {
        if !self.spk_started.load(Ordering::Relaxed) || pcm16.is_empty() {
            return;
        }
        self.queue.lock().unwrap().push_data(pcm16, rate);
    }

    /// 扬声器是否正在（或刚结束）出声——网关窗口。
    pub fn playing(&self, tail_s: f64) -> bool {
        playing(&self.spk_shared, tail_s)
    }

    /// 打断快停（让位收束，见 listener.rs `on_interrupted`）：清播放队列 +
    /// 重建 aplay，旧回合积压音频立刻丢弃；播放线程保持运行（新回合块
    /// 照常播）。未启动播放时是 no-op。
    pub fn interrupt_playback(&self) {
        if !self.spk_started.load(Ordering::Relaxed) {
            return;
        }
        let q = Arc::clone(&self.queue.lock().unwrap());
        q.push_flush();
    }

    /// 扬声器软件音量；播放器未建立时记住，建立时生效。
    pub fn set_volume(&self, v: u8) {
        let v = v.min(100);
        self.spk_shared.volume.store(v, Ordering::Relaxed);
        log::info!("扬声器音量: {v}%");
    }

    /// 当前扬声器软件音量（0-100）。
    pub fn volume(&self) -> u8 {
        self.spk_shared.volume.load(Ordering::Relaxed)
    }

    /// 麦克风静音（静音 = 全零片段照发，音频时钟红线不停）。
    pub fn set_muted(&self, m: bool) {
        self.ctl.muted.store(m, Ordering::Relaxed);
        log::info!("麦克风静音: {m}");
    }

    pub fn muted(&self) -> bool {
        self.ctl.muted.load(Ordering::Relaxed)
    }

    /// 清空播放队列并等播放线程退出（≤3s；对应 `stop_speaker`）。
    pub fn stop_speaker(&self) {
        if !self.spk_started.swap(false, Ordering::Relaxed) {
            return;
        }
        let q = Arc::clone(&self.queue.lock().unwrap());
        q.clear(); // 清队：未播的直接丢
        q.push_stop();
        let t = self.spk_thread.lock().unwrap().take();
        if let Some(h) = t {
            if !join_with_timeout(h, Duration::from_secs(3)) {
                log::error!("扬声器线程 3s 内未退出（泄漏等待其自然退出）");
            }
        }
        *self.spk_shared.play_until.lock().unwrap() = None;
    }

    /// 启动麦克风采集（on_unit 在 mic 线程回调：1s unit=32000B + t_recorded，
    /// 实现方必须快——只投递）。
    pub fn start_mic(&mut self, on_unit: Box<dyn FnMut(Vec<u8>, Instant) + Send>) {
        if self.ctl.running.load(Ordering::Relaxed) {
            return;
        }
        let dump = self.open_dump();
        self.ctl.running.store(true, Ordering::Relaxed);
        if let Some(a) = &self.aec {
            // 开麦 = far/near 时钟重新同起点：清掉跨会话累积的位置差
            // （配合 speaker 的 near_active 门控，两端冻结期也保持对齐）
            a.lock().unwrap().reset();
        }
        self.ctl.bytes_total.store(0, Ordering::Relaxed);
        *self.ctl.started_at.lock().unwrap() = Some(Instant::now());
        *self.ctl.duck_emergency_until.lock().unwrap() = None;
        *self.ctl.aec_lock_cache.lock().unwrap() = (None, true);
        self.ctl.gap_events.lock().unwrap().clear();
        let ctx = MicCtx {
            capture: Arc::clone(&self.capture),
            ctl: Arc::clone(&self.ctl),
            spk_shared: Arc::clone(&self.spk_shared),
            cfg: self.mic_cfg.clone(),
            aec: self.aec.clone(),
            agc: self.near_agc.then(|| Agc::new(NEAR_AGC_THRESHOLD)),
            timing: Arc::clone(&self.timing),
            level: Arc::clone(&self.level),
            on_unit,
            dump,
            dump_path: self.mic_dump_path.clone(),
        };
        let handle = thread::Builder::new()
            .name("o5-mic".to_string())
            .spawn(move || mic_loop(ctx))
            .expect("spawn mic thread");
        *self.mic_thread.lock().unwrap() = Some(handle);
        log::info!("麦克风已启动（16k 单声道）");
    }

    /// 停止采集线程（≤2s join；对应 `stop_mic`）。
    pub fn stop_mic(&mut self) {
        self.ctl.running.store(false, Ordering::Relaxed);
        let t = self.mic_thread.lock().unwrap().take();
        if let Some(h) = t {
            if !join_with_timeout(h, Duration::from_secs(2)) {
                log::error!("麦克风线程 2s 内未退出（泄漏等待其自然退出）");
            }
        }
        // 转储由采集线程退出时关闭（文件归线程所有）
        log::info!(
            "麦克风已停止（本次共采集 {:.1} 秒）",
            self.ctl.bytes_total.load(Ordering::Relaxed) as f64 / UNIT_BYTES as f64
        );
    }

    /// 启动 3 秒后仍一个字节都没采到 -> 麦克风异常。
    pub fn mic_healthy(&self) -> bool {
        if !self.ctl.running.load(Ordering::Relaxed) {
            return false;
        }
        match *self.ctl.started_at.lock().unwrap() {
            Some(t) if t.elapsed().as_secs_f64() < 3.0 => true,
            Some(_) => self.ctl.bytes_total.load(Ordering::Relaxed) > 0,
            None => false,
        }
    }

    /// UI 用的电平条（dBFS + 线性 RMS）。
    pub fn level(&self) -> &MicLevelTracker {
        &self.level
    }

    /// AEC 诊断信息（无 AEC 时 `None`；键与各 Python 实现 `stats()` 一致）。
    pub fn aec_stats(&self) -> Option<serde_json::Value> {
        self.aec.as_ref().map(|a| a.lock().unwrap().stats())
    }

    /// 生效的回声模式（"nlms"/"subband"/"off"）。
    pub fn aec_mode(&self) -> &str {
        &self.aec_mode
    }

    /// 注入播放通道工厂（测试用；须在 [`AudioIo::start`] 前调用）。
    pub fn set_playback_sink_factory(&mut self, f: SinkFactory) {
        *self.sink_factory.lock().unwrap() = f;
    }

    /// 麦克风调试转储（回声排查用）：`_open_dump`。
    fn open_dump(&self) -> Option<File> {
        if !self.mic_dump || self.mic_dump_path.is_empty() {
            return None;
        }
        let path = PathBuf::from(&self.mic_dump_path);
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        match File::create(&path) {
            Ok(f) => {
                log::info!("麦克风原始流转储已开启: {}", self.mic_dump_path);
                Some(f)
            }
            Err(e) => {
                log::error!("打开麦克风转储失败：{e}");
                None
            }
        }
    }
}

impl Drop for AudioIo {
    fn drop(&mut self) {
        // 尽力收尾：两个线程都是短超时循环，置标志后 ≤100ms 自行退出；
        // 不在 Drop 里 join（避免 drop 路径卡 5s）。
        self.ctl.running.store(false, Ordering::Relaxed);
        if self.spk_started.load(Ordering::Relaxed) {
            let q = Arc::clone(&self.queue.lock().unwrap());
            q.clear();
            q.push_stop();
        }
    }
}

// ---------------------------------------------------------------------------
// 单元测试（私有结构直达）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vol_gain_curve() {
        assert_eq!(vol_gain(0), 0.0);
        assert!((vol_gain(100) - 1.0).abs() < 1e-12);
        // 感知曲线：50% 约三分之一幅度
        assert!((vol_gain(50) - 0.5f64.powf(1.6)).abs() < 1e-12);
        assert_eq!(vol_gain(200), 1.0); // clamp 到 100
    }

    #[test]
    fn scale_pcm16_numpy_semantics() {
        // rint(clip(x * gain))：half-even 舍入、溢出夹取
        let pcm = i16_to_bytes(&[1, 3, 5, -1, -3, 7]);
        let v = bytes_to_i16(&scale_pcm16(&pcm, 0.5));
        assert_eq!(v, vec![0, 2, 2, 0, -2, 4]); // 0.5->0, 1.5->2, 2.5->2
        let pcm = i16_to_bytes(&[30000, -30000]);
        let v = bytes_to_i16(&scale_pcm16(&pcm, 1.5));
        assert_eq!(v, vec![32767, -32768]); // clip
        // 奇数字节尾巴按 numpy frombuffer 语义丢弃
        let mut odd = i16_to_bytes(&[1000, -1000]);
        odd.push(0);
        assert_eq!(scale_pcm16(&odd, 1.0).len(), 4);
    }

    #[test]
    fn fade_in_head_only_first_samples() {
        // 24000Hz -> 3ms = 72 样本淡入：头部递增、其余原样
        let pcm = i16_to_bytes(&[10000; 400]);
        let mut d = pcm.clone();
        fade_in_head(&mut d, 24000);
        let v = bytes_to_i16(&d);
        assert_eq!(v.len(), 400);
        assert_eq!(v[0], 10000 / 72); // 第一个样本 ≈ 1/72
        assert!(v[36] > v[0] && v[36] < 10000); // 中点仍在爬升
        assert_eq!(v[71], 10000); // 淡入末样本到位
        assert_eq!(v[72], 10000); // 尾部不受影响
        assert_eq!(v[399], 10000);
        // 短块（不足 3ms）：全块淡入，幅度递增、末样本到位
        let mut d = i16_to_bytes(&[-5000; 10]);
        fade_in_head(&mut d, 24000);
        let v = bytes_to_i16(&d);
        assert_eq!(v[9], -5000);
        assert!(v[0].abs() < v[5].abs() && v[5].abs() < v[9].abs());
    }

    #[test]
    fn level_tracker_silence_and_signal() {
        let t = MicLevelTracker::new();
        assert_eq!(t.db(), -90.0);
        assert_eq!(t.rms(), 0.0);
        t.update(&[]);
        assert_eq!(t.db(), -90.0);
        t.update(&i16_to_bytes(&[0; 100]));
        assert_eq!(t.db(), -90.0);
        // 满幅方波：RMS≈1 -> dBFS≈0
        let sq: Vec<i16> = (0..100).map(|i| if i % 2 == 0 { 32767 } else { -32767 }).collect();
        t.update(&i16_to_bytes(&sq));
        assert!((t.rms() - 1.0).abs() < 1e-4);
        assert!(t.db() > -0.001 && t.db() <= 0.0);
        // 常幅 16384：RMS=0.5 -> -6.02dB
        t.update(&i16_to_bytes(&[16384; 100]));
        assert!((t.rms() - 0.5).abs() < 1e-6);
        assert!((t.db() + 6.0206).abs() < 0.01);
    }

    #[test]
    fn playing_window_semantics() {
        let sh = SpeakerShared {
            play_until: Mutex::new(None),
            volume: AtomicU8::new(80),
        };
        assert!(!playing(&sh, 5.0)); // 从未播放
        *sh.play_until.lock().unwrap() = Some(Instant::now() + Duration::from_millis(100));
        assert!(playing(&sh, 0.0));
        thread::sleep(Duration::from_millis(150));
        assert!(!playing(&sh, 0.01)); // 已过结束时刻
        assert!(playing(&sh, 0.2)); // 尾沿内仍算"刚结束出声"
    }

    #[test]
    fn spk_queue_bounded_drop_oldest() {
        let q = SpkQueue::new();
        for i in 0..(PLAY_QUEUE_CAP + 5) {
            q.push_data(vec![i as u8; 8], 16000);
        }
        assert_eq!(q.items.lock().unwrap().len(), PLAY_QUEUE_CAP);
        assert_eq!(q.dropped.load(Ordering::Relaxed), 5);
        // 最旧的 5 块被丢，队首应是第 5 块（i=5）
        match q.get_nowait() {
            Some(SpkItem::Data(d, r)) => {
                assert_eq!(d[0], 5);
                assert_eq!(r, 16000);
            }
            _ => panic!("应为 Data"),
        }
        while q.get_nowait().is_some() {} // 排空剩余
        assert!(q.get(Duration::from_millis(20)).is_none()); // 空等超时
        q.push_stop();
        assert!(matches!(q.get_nowait(), Some(SpkItem::Stop)));
        q.push_data(vec![1; 4], 24000);
        q.clear(); // stop_speaker 清队
        assert!(q.get_nowait().is_none());
    }

    #[test]
    fn spk_queue_get_wakes_on_push() {
        let q = Arc::new(SpkQueue::new());
        let q2 = Arc::clone(&q);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            q2.push_data(vec![7; 2], 16000);
        });
        let t0 = Instant::now();
        match q.get(Duration::from_secs(2)) {
            Some(SpkItem::Data(d, _)) => assert_eq!(d, vec![7, 7]),
            _ => panic!("应被推送唤醒"),
        }
        assert!(t0.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn duck_deep_now_paths() {
        let ctl = MicCtl::new();
        let now = Instant::now();
        // 无 AEC：按"已锁定"处理 -> 不深压
        assert!(!duck_deep_now(None, &ctl, now));
        // 采集断裂紧急窗内 -> 深压
        *ctl.duck_emergency_until.lock().unwrap() = Some(now + Duration::from_millis(100));
        assert!(duck_deep_now(None, &ctl, now));
        // AEC 存在但未锁定（Off 的 stats() 无 locked 键 -> false）-> 深压。
        // 0.5s 缓存：上一问在 now，这里取 >0.5s 之后，强制走重估分支。
        let aec = Arc::new(Mutex::new(AecImpl::Off));
        let later = now + Duration::from_millis(600);
        assert!(duck_deep_now(Some(&aec), &ctl, later));
        // 缓存窗口内再问：仍深压（走缓存）
        assert!(duck_deep_now(Some(&aec), &ctl, later + Duration::from_millis(1)));
    }

    #[test]
    fn duck_and_gate_predicates() {
        let shared = SpeakerShared {
            play_until: Mutex::new(None),
            volume: AtomicU8::new(80),
        };
        let cfg = MicCfg { gate_enabled: true, gate_tail_s: 0.2, duck: 0.4 };
        // 未播放：门控/闪避都不激活（静音除外）
        assert!(!gate_active(&cfg, false, &shared));
        assert!(!duck_active(&cfg, &shared));
        assert!(gate_active(&cfg, true, &shared)); // muted 恒置零
        // 播放中：都激活
        *shared.play_until.lock().unwrap() = Some(Instant::now() + Duration::from_millis(300));
        assert!(gate_active(&cfg, false, &shared));
        assert!(duck_active(&cfg, &shared)); // 尾沿 max(0.2, 2.0)=2s
        // duck 压制数值：deep 用一半增益
        let pcm = i16_to_bytes(&[1000, -1000]);
        assert_eq!(bytes_to_i16(&duck_scale(&pcm, 0.4, false)), vec![400, -400]);
        assert_eq!(bytes_to_i16(&duck_scale(&pcm, 0.4, true)), vec![200, -200]);
    }

    #[test]
    fn resample_if_needed_passthrough() {
        let pcm = i16_to_bytes(&[1, 2, 3]);
        assert_eq!(resample_if_needed(&pcm, 16000, 16000), pcm);
        assert!(!resample_if_needed(&pcm, 24000, 48000).is_empty());
    }
}

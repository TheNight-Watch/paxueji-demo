//! o5-camera：omni/video 模式下 ~1fps JPEG，作为最新帧槽供 unit 绑定
//! （`o5/camera_feed.py` 的忠实移植）。
//!
//! 对齐两个参考实现的规则（Python 版文件头注释）：
//! - 每个 unit（1 秒）绑定"当前最新一帧"，没帧时用 1x1 黑帧占位（协议层兜底）。
//! - 模型输入像素预算 448²≈20 万像素，超出部分服务端会等比缩掉：640x480 超预算
//!   53%，白白多烧 JPEG 编码 / base64 / 拷贝，所以采集分辨率默认 512x384
//!   （4:3 不变形，19.7 万像素）。
//! - 迟到帧 / 采集线程与取帧线程之间用锁隔离；stop 后帧清空。
//! - MaixCAM2 上 camera 独占使用：与本应用运行时 launcher 不应同时占用摄像头。
//!
//! 与 Python 版的结构性差异（协议行为不变，均见各条注释）：
//! - 设备上没有 /dev/video\*（libmaix 私有 VI 管线，V4L2 不可用），采集改为
//!   `feeder_camera.py` 子进程喂流（stdout `[u32 LE len][RGB888 w*h*3]`）；
//!   hmirror/vflip 与 set_fps(30)+自动曝光恢复（白屏坑）全部由 feeder 处理，
//!   见 [`feeder`] 模块文档。
//! - JPEG 编码从采集线程移到调用侧：上行槽存原始 RGB 帧（每 1s 更新），
//!   [`CameraFeed::latest_jpeg_b64`] 按需编码并对同帧同质量缓存结果
//!   （Python 在采集线程里编好缓存 b64；Rust 无 GIL，按需编码即可，无会话时不白烧）。
//! - 预览用自有 [`PreviewFrame`]（RGB888），不依赖 o5-ui。
//! - 契约签名差异：`new` 在 CONTRACTS.md 草签名（7 参数）基础上追加
//!   `timing: Arc<Timing>` 参数（任务修订：直接依赖 o5-perf 的 `Arc<Timing>`
//!   契约参数，与 AudioIo / Ui 的注入方式一致）。
//!
//! 线程名 `o5-camera`（daemon 语义：进程退出即死；Rust 侧另有 Drop 兜底
//! kill feeder 子进程 + join，防孤儿进程）。

pub mod clock;
mod feeder;
mod resize;
pub mod source;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError};
use jpeg_encoder::{ColorType, Encoder};
use log::{error, info, warn};
use o5_perf::Timing;

use crate::clock::{Clock, SystemClock};
use crate::feeder::{kill_child, ChildSlot, NoopFrameSource};
use crate::source::FrameSource;

/// 上行抽帧：协议要求每 unit 绑 1 帧（Python `FRAME_INTERVAL_S = 1.0`）。
pub const FRAME_INTERVAL_S: f64 = 1.0;

/// 屏显副本上限 8Hz（Python `BG_INTERVAL_S = 0.125`）。原 0.06（~16Hz）：
/// UI+全模态实测 CPU 99-108%，麦克风采集线程被饿到丢 40% 音频
/// （subband 抑制掉到 0.1dB），预览降半频换音频连续。
pub const BG_INTERVAL_S: f64 = 0.125;

/// 帧源无新帧时的小睡（不忙转；Python 版同值 `time.sleep(0.005)`）。
/// 真实 feeder 阻塞在 stdout read 上，不会走到这条路径。
const IDLE_SLEEP: Duration = Duration::from_millis(5);

/// stop() 的 join 上限（Python `thread.join(timeout=2.5)`）。
const JOIN_TIMEOUT: Duration = Duration::from_millis(2500);

/// 屏显预览帧（RGB888，已缩放到 preview_size；不依赖 o5-ui）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl PreviewFrame {
    /// 拍学机快门：把屏上这一帧编码成上行用的 base64 JPEG，保证孩子看到的就是模型看到的。
    pub fn to_jpeg_b64(&self, quality: u8) -> Option<String> {
        let jpeg = encode_rgb_jpeg(self.width, self.height, &self.data, quality.clamp(1, 100))?;
        Some(base64::engine::general_purpose::STANDARD.encode(jpeg))
    }
}

/// 上行槽里的原始帧。
struct RawFrame {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

/// `latest_jpeg_b64` 的同帧缓存：同帧同质量直接复用上次编码结果
/// （Python `latest()` 返回缓存 b64 的等价物；帧指针相同才命中）。
struct JpegCacheEntry {
    quality: u8,
    frame: Arc<RawFrame>,
    b64: String,
}

/// 相机线程与取帧侧共享的状态（锁隔离迟到帧，Python `_lock` 的角色）。
struct Shared {
    /// stop 请求（线程循环的退出条件之一）。
    running: AtomicBool,
    /// 采集线程仍活着（stop / EOF 后为 false；供测试与诊断）。
    alive: AtomicBool,
    /// 上行槽：最新原始帧（1s 节流更新；None = 协议层补黑帧）。
    uplink: Mutex<Option<Arc<RawFrame>>>,
    /// 上行编码缓存。
    jpeg_cache: Mutex<Option<JpegCacheEntry>>,
    /// 预览槽：8Hz 缩放副本。
    preview: Mutex<Option<PreviewFrame>>,
    timing: Arc<Timing>,
    clock: Arc<dyn Clock>,
    preview_size: (u32, u32),
    frames_read: AtomicU64,
    frames_skipped: AtomicU64,
    frames_encoded: AtomicU64,
}

impl Shared {
    fn new(preview_size: (u32, u32), timing: Arc<Timing>, clock: Arc<dyn Clock>) -> Self {
        Self {
            running: AtomicBool::new(true),
            alive: AtomicBool::new(true),
            uplink: Mutex::new(None),
            jpeg_cache: Mutex::new(None),
            preview: Mutex::new(None),
            timing,
            clock,
            preview_size,
            frames_read: AtomicU64::new(0),
            frames_skipped: AtomicU64::new(0),
            frames_encoded: AtomicU64::new(0),
        }
    }
}

/// 采集线程内的节拍状态（上次读帧 / 上次上行更新 / 上次预览更新的时刻）。
#[derive(Default)]
struct Throttles {
    last_read: Option<Instant>,
    last_jpg: Option<Instant>,
    last_bg: Option<Instant>,
}

/// 摄像头：采集线程 + 上行/预览双速率槽。
///
/// 用法（设备）：`CameraFeed::new(512, 384, 30, false, false, &feeder, (640, 480), timing)`
/// 或 [`CameraFeed::from_config`]；会话侧每 unit 调 [`CameraFeed::latest_jpeg_b64`]，
/// UI 侧按帧率调 [`CameraFeed::latest_preview`]；退出时 [`CameraFeed::stop`]。
pub struct CameraFeed {
    shared: Arc<Shared>,
    done: Receiver<()>,
    /// feeder 子进程句柄（仅设备路径；`stop` 时 kill + 收尸）。
    child: Option<ChildSlot>,
    stopped: bool,
}

impl CameraFeed {
    /// 启动设备摄像头（feeder_camera.py 子进程，协议见 [`feeder`] 模块文档）。
    ///
    /// - linux：spawn `python3 <feeder_path> <w> <h> --fps <fps> [--hmirror] [--vflip]`
    ///   （feeder 路径来自 config `deviceCameraFeederPath` 解析，默认 exe 同目录）；
    ///   EOF/进程退出 → 停止并打日志。
    /// - 非 linux：no-op（打日志；PC 合成帧走 [`MockFrameSource`](source::MockFrameSource)，
    ///   `--test-pc` 用 [`CameraFeed::with_source`]）。
    ///
    /// 契约签名差异：较 CONTRACTS.md 草签名多末位 `timing` 参数（见 crate 文档）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        hmirror: bool,
        vflip: bool,
        feeder_path: &std::path::Path,
        preview_size: (u32, u32),
        timing: Arc<Timing>,
    ) -> Self {
        let child: ChildSlot = Arc::new(Mutex::new(None));
        let (source, has_child): (Box<dyn FrameSource>, bool) =
            make_device_source(width, height, fps, hmirror, vflip, feeder_path, &child);
        let feed = Self::start(
            source,
            if has_child { Some(child) } else { None },
            preview_size,
            timing,
            Arc::new(SystemClock),
        );
        info!(
            "摄像头已启动（feeder {width}x{height} fps<={fps}，预览 {}x{}）",
            preview_size.0, preview_size.1
        );
        feed
    }

    /// 从配置构造（video.\* 字段 + `cameraFeederPath` 解析；预览 = 屏幕 640x480：
    /// 512x384 采集的预览放大铺满，同 Python main.py）。
    pub fn from_config(cfg: &o5_config::Config, timing: Arc<Timing>) -> Self {
        Self::new(
            cfg.video_width,
            cfg.video_height,
            cfg.video_fps,
            cfg.video_hmirror,
            cfg.video_vflip,
            &cfg.camera_feeder_path(),
            (640, 480),
            timing,
        )
    }

    /// 注入帧源与时钟（`--test-pc` / 单测）：合成帧走
    /// [`MockFrameSource`](source::MockFrameSource)，时钟通常传
    /// [`SystemClock`](clock::SystemClock)。
    pub fn with_source(
        source: Box<dyn FrameSource>,
        preview_size: (u32, u32),
        timing: Arc<Timing>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::start(source, None, preview_size, timing, clock)
    }

    fn start(
        source: Box<dyn FrameSource>,
        child: Option<ChildSlot>,
        preview_size: (u32, u32),
        timing: Arc<Timing>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let shared = Arc::new(Shared::new(preview_size, timing, clock));
        let (done_tx, done_rx) = bounded(1);
        let thread_shared = Arc::clone(&shared);
        // 线程名对齐 Python threading.Thread(name="o5-camera")
        let spawned = thread::Builder::new()
            .name("o5-camera".to_string())
            .spawn(move || camera_loop(source, thread_shared, done_tx));
        match spawned {
            Ok(_handle) => {}
            Err(e) => {
                // 资源耗尽等极端情况：视同立即 EOF
                error!("o5-camera 线程启动失败：{e}");
                shared.alive.store(false, Ordering::Release);
            }
        }
        Self {
            shared,
            done: done_rx,
            child,
            stopped: false,
        }
    }

    /// 取最新一帧（base64 JPEG）；无帧返回 None（协议层会补黑帧）。
    ///
    /// 上行槽每 [`FRAME_INTERVAL_S`] 换一次最新原始帧；这里对槽内帧按需编码
    /// （jpeg-encoder，quality 1-100，默认语义 60 由调用方传），同帧同质量
    /// 直接复用上次结果。编码/复制耗时与 JPEG 大小喂 `timing.jpeg`。
    pub fn latest_jpeg_b64(&self, quality: u8) -> Option<String> {
        let clock = &self.shared.clock;
        let t0 = clock.now();
        // "拷贝"阶段：锁内 Arc 快照（Python 的 img.copy 角色挪到这里）
        let frame = self.shared.uplink.lock().unwrap().clone()?;
        let copy_ms = clock.now().saturating_duration_since(t0).as_secs_f64() * 1000.0;
        let quality = quality.clamp(1, 100);

        // 同帧同质量：直接用缓存（对应 Python latest() 的缓存 b64）
        {
            let cache = self.shared.jpeg_cache.lock().unwrap();
            if let Some(entry) = cache.as_ref() {
                if entry.quality == quality && Arc::ptr_eq(&entry.frame, &frame) {
                    return Some(entry.b64.clone());
                }
            }
        }

        let t1 = clock.now();
        let jpeg = encode_jpeg(&frame, quality)?; // 失败：log + None（槽不回退，同 Python 只 log）
        let t2 = clock.now();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
        let t3 = clock.now();
        let ms = |a: Instant, b: Instant| b.saturating_duration_since(a).as_secs_f64() * 1000.0;
        self.shared.timing.jpeg(
            jpeg.len(),
            ms(t1, t2), // encode_ms
            ms(t2, t3), // b64_ms
            copy_ms,    // copy_ms
        );
        self.shared.frames_encoded.fetch_add(1, Ordering::Relaxed);
        *self.shared.jpeg_cache.lock().unwrap() = Some(JpegCacheEntry {
            quality,
            frame,
            b64: b64.clone(),
        });
        Some(b64)
    }

    /// 取屏幕 UI 背景帧（独立缓冲，已双线性缩放到 preview_size）；无则 None。
    /// 上限 8Hz（见 [`BG_INTERVAL_S`] 的缘由注释）。
    pub fn latest_preview(&self) -> Option<PreviewFrame> {
        self.shared.preview.lock().unwrap().clone()
    }

    /// 停止：请求退出 → kill feeder 子进程（linux）→ join ≤2.5s → 清槽。
    /// 幂等；Drop 时兜底再调一次（防 feeder 孤儿进程）。
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.shared.running.store(false, Ordering::Release);
        // 读线程可能阻塞在 feeder stdout 上，kill 子进程让它走 EOF 退出
        if let Some(slot) = self.child.take() {
            kill_child(&slot);
        }
        match self.done.recv_timeout(JOIN_TIMEOUT) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => {
                warn!("相机线程 2.5s 内未退出（放弃 join，daemon 语义放行）");
            }
            Err(RecvTimeoutError::Disconnected) => {} // 线程已结束（含 panic）
        }
        // 清槽（Python stop：_frame_b64/_bg_img = None）
        *self.shared.uplink.lock().unwrap() = None;
        *self.shared.jpeg_cache.lock().unwrap() = None;
        *self.shared.preview.lock().unwrap() = None;
        info!(
            "摄像头已停止（读帧 {}，跳帧 {}，上行编码 {}）",
            self.shared.frames_read.load(Ordering::Relaxed),
            self.shared.frames_skipped.load(Ordering::Relaxed),
            self.shared.frames_encoded.load(Ordering::Relaxed)
        );
    }

    /// 采集线程是否仍在跑（stop 请求或 EOF 之后为 false）。
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Acquire)
    }

    /// 已读入的有效帧数（诊断/测试同步用）。
    pub fn frames_read(&self) -> u64 {
        self.shared.frames_read.load(Ordering::Relaxed)
    }

    /// 因长度不符被跳过的帧数（协议/源异常的计数）。
    pub fn frames_skipped(&self) -> u64 {
        self.shared.frames_skipped.load(Ordering::Relaxed)
    }

    /// 已完成的 JPEG 上行编码次数（不含缓存命中）。
    pub fn frames_encoded(&self) -> u64 {
        self.shared.frames_encoded.load(Ordering::Relaxed)
    }
}

impl Drop for CameraFeed {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// 采集线程
// ---------------------------------------------------------------------------

fn camera_loop(
    mut source: Box<dyn FrameSource>,
    shared: Arc<Shared>,
    done_tx: crossbeam_channel::Sender<()>,
) {
    let mut throttles = Throttles::default();
    while shared.running.load(Ordering::Acquire) {
        match source.read_frame() {
            Some((w, h, data)) => {
                // read 可能阻塞（feeder）/按节拍返回（mock），读完后取时刻
                let now = shared.clock.now();
                handle_frame(&mut throttles, &shared, w, h, data, now);
            }
            None => {
                if source.is_finished() {
                    // EOF / 子进程退出：停止（具体原因由帧源侧打日志）
                    info!("o5-camera: 帧源已结束，采集线程退出");
                    break;
                }
                // 无新帧不忙转（Mock 自带节奏；真实 feeder 阻塞在 read 不会到这）
                thread::sleep(IDLE_SLEEP);
            }
        }
    }
    shared.alive.store(false, Ordering::Release);
    let _ = done_tx.send(()); // 接收端可能已放弃（stop 超时），忽略
}

/// 一帧到达后的双速率分发（上行 1Hz / 预览 8Hz），纯逻辑便于确定性单测。
fn handle_frame(
    throttles: &mut Throttles,
    shared: &Shared,
    w: u32,
    h: u32,
    data: Vec<u8>,
    now: Instant,
) {
    // 帧格式校验：长度 != w*h*3 跳帧（feeder 协议失步 / mock 畸形帧）
    let expected = w as usize * h as usize * 3;
    if data.len() != expected {
        shared.frames_skipped.fetch_add(1, Ordering::Relaxed);
        warn!(
            "o5-camera: 帧长度不符（{} != {expected}），跳帧",
            data.len()
        );
        return;
    }
    shared.frames_read.fetch_add(1, Ordering::Relaxed);

    // 帧间间隔埋点：全量喂，Timing 侧只保留窗口最大（Python cam_frame_gap 口径；
    // ">阈值"的筛选在 Timing 内完成）。首帧无间隔不喂（Python `_last_read > 0`）。
    if let Some(last) = throttles.last_read.replace(now) {
        shared
            .timing
            .cam_gap(now.saturating_duration_since(last).as_secs_f64() * 1000.0);
    }

    let frame = Arc::new(RawFrame {
        width: w,
        height: h,
        data,
    });

    // 1) 上行：每秒换当前最新原始帧（协议：每 unit 绑 1 帧 JPEG）
    if throttles
        .last_jpg
        .is_none_or(|t| now.saturating_duration_since(t).as_secs_f64() >= FRAME_INTERVAL_S)
    {
        throttles.last_jpg = Some(now);
        *shared.uplink.lock().unwrap() = Some(Arc::clone(&frame));
    }

    // 2) 屏显：8Hz 上限的缩放副本（跟不上时丢中间帧，保最新）
    if throttles
        .last_bg
        .is_none_or(|t| now.saturating_duration_since(t).as_secs_f64() >= BG_INTERVAL_S)
    {
        throttles.last_bg = Some(now);
        let (pw, ph) = shared.preview_size;
        if pw > 0 && ph > 0 {
            let scaled = if (w, h) == (pw, ph) {
                frame.data.clone() // 尺寸相同无需缩放（Python 同判断）
            } else {
                resize::bilinear_resize(&frame.data, w, h, pw, ph)
            };
            *shared.preview.lock().unwrap() = Some(PreviewFrame {
                width: pw,
                height: ph,
                data: scaled,
            });
        }
    }
}

/// RGB888 → JPEG（jpeg-encoder，simd feature）。尺寸超 u16 或编码失败返回 None（已打日志）。
fn encode_jpeg(frame: &RawFrame, quality: u8) -> Option<Vec<u8>> {
    encode_rgb_jpeg(frame.width, frame.height, &frame.data, quality)
}

fn encode_rgb_jpeg(width: u32, height: u32, data: &[u8], quality: u8) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || width > u16::MAX as u32 || height > u16::MAX as u32 {
        error!("o5-camera: 帧尺寸 {width}x{height} 超出 JPEG 能力，放弃编码");
        return None;
    }
    let mut out = Vec::new();
    match Encoder::new(&mut out, quality).encode(data, width as u16, height as u16, ColorType::Rgb) {
        Ok(()) => Some(out),
        Err(e) => {
            error!("o5-camera: JPEG 编码失败：{e}");
            None
        }
    }
}

/// 设备帧源（linux：feeder 子进程；其他平台 no-op）。返回 (源, 是否持有子进程)。
fn make_device_source(
    width: u32,
    height: u32,
    fps: u32,
    hmirror: bool,
    vflip: bool,
    feeder_path: &std::path::Path,
    child: &ChildSlot,
) -> (Box<dyn FrameSource>, bool) {
    #[cfg(target_os = "linux")]
    {
        match feeder::FeederFrameSource::spawn(
            width,
            height,
            fps,
            hmirror,
            vflip,
            feeder_path,
            child,
        ) {
            Ok(s) => (Box::new(s), true),
            Err(e) => {
                error!("摄像头 feeder 拉起失败：{e}（本次运行无视频帧）");
                (Box::new(NoopFrameSource::new()), false)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // 参数仅在 linux 构建（设备源门控）里使用
        let _ = (width, height, fps, hmirror, vflip, feeder_path, child);
        warn!(
            "非 linux 平台没有设备摄像头（feeder 仅 linux）；PC 合成帧请用 \
             MockFrameSource（--test-pc）"
        );
        (Box::new(NoopFrameSource::new()), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockClock;
    use crate::source::{MockFrameSource, MockPattern};

    // -- 测试辅助 ---------------------------------------------------------

    /// 一次产一帧后 EOF 的脚本源；None 项 = "此刻没帧但未结束"。
    struct ScriptedSource {
        script: Vec<Option<(u32, u32, Vec<u8>)>>,
        pos: usize,
    }

    impl FrameSource for ScriptedSource {
        fn read_frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
            if self.pos < self.script.len() {
                let item = self.script[self.pos].clone();
                self.pos += 1;
                item
            } else {
                None
            }
        }
        fn is_finished(&self) -> bool {
            self.pos >= self.script.len()
        }
    }

    /// 恒定纯色帧源（JPEG 往返精度测试用）。不限速产出——所有帧内容相同，
    /// 不需要稳定等待，只要读数 ≥1 即可。
    struct SolidSource {
        w: u32,
        h: u32,
        v: u8,
    }

    impl FrameSource for SolidSource {
        fn read_frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
            Some((
                self.w,
                self.h,
                vec![self.v; self.w as usize * self.h as usize * 3],
            ))
        }
    }

    /// 等读数达到 n（不限速源用；有上限等待防挂死）。
    fn wait_frames_at_least(feed: &CameraFeed, n: u64) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while feed.frames_read() < n {
            assert!(
                Instant::now() < deadline,
                "等帧超时（读数 {} < {n}）",
                feed.frames_read()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn mock_feed(
        clock: &Arc<MockClock>,
        w: u32,
        h: u32,
        fps: u32,
        preview: (u32, u32),
    ) -> CameraFeed {
        let timing = Arc::new(Timing::new());
        let source = MockFrameSource::new(w, h, fps, Arc::clone(clock) as Arc<dyn Clock>);
        CameraFeed::with_source(
            Box::new(source),
            preview,
            timing,
            Arc::clone(clock) as Arc<dyn Clock>,
        )
    }

    /// 等相机线程消化完当前假时钟下的全部到期帧（读数连续两次稳定）。
    fn wait_frames_stable(feed: &CameraFeed) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = feed.frames_read();
        loop {
            thread::sleep(Duration::from_millis(25));
            let cur = feed.frames_read();
            if cur == last {
                return cur;
            }
            last = cur;
            assert!(
                Instant::now() < deadline,
                "等待帧稳定超时（最后读数 {last}）"
            );
        }
    }

    fn drain(feed: &CameraFeed, clock: &Arc<MockClock>, dur: Duration) -> u64 {
        clock.advance(dur);
        wait_frames_stable(feed)
    }

    // -- 纯逻辑：handle_frame 的确定性节拍测试（无线程无 sleep） -------------

    fn shared_for_test() -> (Shared, Arc<Timing>) {
        let timing = Arc::new(Timing::new());
        let shared = Shared::new((32, 24), Arc::clone(&timing), Arc::new(MockClock::new()));
        (shared, timing)
    }

    fn solid_frame(w: u32, h: u32, v: u8) -> Vec<u8> {
        vec![v; w as usize * h as usize * 3]
    }

    #[test]
    fn handle_frame_throttle_boundaries_exact() {
        let (shared, _timing) = shared_for_test();
        let mut thr = Throttles::default();
        let base = Instant::now();
        let mut frame = |t: Instant, v: u8| {
            handle_frame(&mut thr, &shared, 8, 6, solid_frame(8, 6, v), t);
        };

        // t=0：首帧，上行/预览都立槽
        frame(base, 10);
        let up0 = Arc::clone(shared.uplink.lock().unwrap().as_ref().unwrap());
        let pv0 = shared.preview.lock().unwrap().clone().unwrap();
        assert_eq!((pv0.width, pv0.height), (32, 24));
        assert_eq!(pv0.data.len(), 32 * 24 * 3);

        // t=0.999s：上行（<1.0s）与预览（≥0.125s 已过）——上行不换帧，预览换
        frame(base + Duration::from_millis(999), 20);
        assert!(
            Arc::ptr_eq(&up0, shared.uplink.lock().unwrap().as_ref().unwrap()),
            "0.999s < 1.0s：上行槽不该更新"
        );
        let pv1 = shared.preview.lock().unwrap().clone().unwrap();
        assert_ne!(pv0.data, pv1.data, "0.999s ≥ 0.125s：预览该更新");

        // t=1.0s：上行窗口到期，换帧
        frame(base + Duration::from_millis(1000), 30);
        assert!(
            !Arc::ptr_eq(&up0, shared.uplink.lock().unwrap().as_ref().unwrap()),
            "1.0s 到期：上行槽该更新"
        );

        // 预览 0.125s 边界（上次预览更新在 t=999）：124ms 不换 / 125ms 换
        let before = shared.preview.lock().unwrap().clone().unwrap();
        frame(base + Duration::from_millis(1123), 40); // 距上次预览 124ms
        assert_eq!(
            before.data,
            shared.preview.lock().unwrap().as_ref().unwrap().data,
            "124ms < 125ms：预览不该更新"
        );
        frame(base + Duration::from_millis(1124), 50); // 距上次预览 125ms
        assert_ne!(
            before.data,
            shared.preview.lock().unwrap().as_ref().unwrap().data,
            "125ms 到期：预览该更新"
        );

        assert_eq!(shared.frames_read.load(Ordering::Relaxed), 5);
        assert_eq!(shared.frames_skipped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn handle_frame_skips_bad_length_and_records_gap() {
        let (shared, timing) = shared_for_test();
        let mut thr = Throttles::default();
        let base = Instant::now();
        // 畸形帧：声明 8x6 但数据长度错
        handle_frame(&mut thr, &shared, 8, 6, vec![0u8; 10], base);
        assert_eq!(shared.frames_skipped.load(Ordering::Relaxed), 1);
        assert_eq!(shared.frames_read.load(Ordering::Relaxed), 0);
        assert!(shared.uplink.lock().unwrap().is_none(), "坏帧不该入槽");
        assert!(shared.preview.lock().unwrap().is_none());
        // 正常帧 + 间隔埋点
        handle_frame(
            &mut thr,
            &shared,
            8,
            6,
            solid_frame(8, 6, 1),
            base + Duration::from_millis(40),
        );
        handle_frame(
            &mut thr,
            &shared,
            8,
            6,
            solid_frame(8, 6, 2),
            base + Duration::from_millis(90),
        );
        assert_eq!(shared.frames_read.load(Ordering::Relaxed), 2);
        // 窗口最大帧间隔 = 50ms（40 -> 90）
        let w = timing.perf_window(1.0);
        assert!(
            (w.cam_gap_ms - 50.0).abs() < 0.5,
            "cam_gap={}",
            w.cam_gap_ms
        );
    }

    #[test]
    fn handle_frame_preview_disabled_when_zero() {
        let timing = Arc::new(Timing::new());
        let shared = Shared::new((0, 0), timing, Arc::new(MockClock::new()));
        let mut thr = Throttles::default();
        handle_frame(
            &mut thr,
            &shared,
            8,
            6,
            solid_frame(8, 6, 1),
            Instant::now(),
        );
        assert!(shared.preview.lock().unwrap().is_none());
        assert!(shared.uplink.lock().unwrap().is_some(), "上行不受影响");
    }

    // -- 线程级：假时钟驱动 MockFrameSource 走完整回路 -----------------------

    #[test]
    fn uplink_slot_throttles_to_1hz() {
        let clock = Arc::new(MockClock::new());
        let feed = mock_feed(&clock, 64, 48, 50, (32, 24)); // 50fps 源

        // 首帧（t=0）立槽
        wait_frames_stable(&feed);
        let j1 = feed.latest_jpeg_b64(60).expect("首帧后应有 JPEG");
        let r1 = feed.frames_read();

        // +0.5s：窗口内多次取，只在新帧后才可能变化——但 0.5 < 1.0，槽不换
        drain(&feed, &clock, Duration::from_millis(500));
        assert!(feed.frames_read() > r1, "0.5s 内应有新帧被读入");
        let j2 = feed.latest_jpeg_b64(60).expect("槽内仍有帧");
        assert_eq!(j1, j2, "0.5s < 1.0s：上行槽未更新，b64 不变");
        // 再取一次仍是同一结果（同帧缓存）
        assert_eq!(feed.latest_jpeg_b64(60).unwrap(), j1);

        // 再 +1.0s（累计 1.5s ≥ 上次换槽 + 1.0s）：换新帧 → b64 变化
        drain(&feed, &clock, Duration::from_millis(1000));
        let j3 = feed.latest_jpeg_b64(60).expect("槽内仍有帧");
        assert_ne!(j1, j3, "1s 节流到期后应换新帧");
    }

    #[test]
    fn preview_slot_throttles_to_8hz() {
        let clock = Arc::new(MockClock::new());
        let feed = mock_feed(&clock, 32, 24, 50, (16, 12));

        drain(&feed, &clock, Duration::from_millis(300));
        let p1 = feed.latest_preview().expect("应有预览帧");
        assert_eq!((p1.width, p1.height), (16, 12));
        assert_eq!(p1.data.len(), 16 * 12 * 3);

        // +0.05s（< 0.125s）：预览不换（数据逐字节相同 = 还是同一份缓冲）
        drain(&feed, &clock, Duration::from_millis(50));
        let p2 = feed.latest_preview().expect("应有预览帧");
        assert_eq!(p1.data, p2.data, "0.05s < 0.125s：预览槽不该更新");

        // +0.3s（累计 0.35s，距上次预览 ≥ 0.125s）：换新预览（内容不同）
        drain(&feed, &clock, Duration::from_millis(300));
        let p3 = feed.latest_preview().expect("应有预览帧");
        assert_ne!(p1.data, p3.data, "预览节流到期后应换新帧");
    }

    #[test]
    fn jpeg_decodable_back_to_same_dimensions() {
        let clock = Arc::new(MockClock::new());
        let feed = mock_feed(&clock, 40, 30, 30, (0, 0));
        wait_frames_stable(&feed);
        let b64 = feed.latest_jpeg_b64(60).expect("应有 JPEG");
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .expect("b64 可解码");
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&jpeg[..]));
        let pixels = decoder.decode().expect("JPEG 可解码");
        let info = decoder.info().expect("有 JPEG 头");
        assert_eq!((info.width, info.height), (40, 30));
        assert_eq!(info.pixel_format, jpeg_decoder::PixelFormat::RGB24);
        assert_eq!(pixels.len(), 40 * 30 * 3, "RGB888 输出");
        // 编码缓存路径再走一遍
        assert_eq!(feed.latest_jpeg_b64(60).unwrap(), b64);
        assert_eq!(feed.frames_encoded(), 1, "第二次命中缓存不再编码");
    }

    #[test]
    fn jpeg_roundtrip_pixel_accuracy_on_solid_frame() {
        // 纯色帧：JPEG 有损但平滑区误差应很小（q60 + 4:2:0 对灰场几乎无损）
        let clock = Arc::new(MockClock::new());
        let timing = Arc::new(Timing::new());
        let feed = CameraFeed::with_source(
            Box::new(SolidSource {
                w: 48,
                h: 36,
                v: 180,
            }),
            (0, 0),
            Arc::clone(&timing),
            Arc::clone(&clock) as Arc<dyn Clock>,
        );
        wait_frames_at_least(&feed, 1);
        let b64 = feed.latest_jpeg_b64(60).expect("应有 JPEG");
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .unwrap();
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&jpeg[..]));
        let pixels = decoder.decode().unwrap();
        let info = decoder.info().unwrap();
        assert_eq!((info.width, info.height), (48, 36));
        assert_eq!(pixels.len(), 48 * 36 * 3);
        assert!(
            pixels
                .chunks_exact(3)
                .all(|px| { px.iter().all(|&c| c.abs_diff(180) <= 16) }),
            "纯色帧往返误差应 ≤16"
        );
        // timing.jpeg 已埋点：jpg_kb = 编码字节数 / 1024
        let avgs = timing.avgs();
        let kb = avgs.jpg_kb.expect("jpeg 埋点应有值");
        assert!((kb - jpeg.len() as f64 / 1024.0).abs() < 0.01);
        assert!(avgs.jpeg_ms.is_some());
        assert!(avgs.b64_ms.is_some());
    }

    #[test]
    fn jpeg_quality_param_changes_output() {
        let clock = Arc::new(MockClock::new());
        let feed = mock_feed(&clock, 40, 30, 30, (0, 0));
        wait_frames_stable(&feed);
        let q60 = feed.latest_jpeg_b64(60).unwrap();
        let q90 = feed.latest_jpeg_b64(90).unwrap();
        assert_ne!(q60, q90, "质量参数应影响编码输出");
        assert_eq!(feed.frames_encoded(), 2);
    }

    #[test]
    fn bad_frames_skipped_good_frames_flow() {
        let clock = Arc::new(MockClock::new());
        let timing = Arc::new(Timing::new());
        let script: Vec<Option<(u32, u32, Vec<u8>)>> = vec![
            Some((8, 6, vec![0u8; 17])),        // 坏帧：长度不符
            Some((8, 6, vec![7u8; 8 * 6 * 3])), // 好帧
            Some((8, 6, vec![9u8; 5])),         // 坏帧
            Some((8, 6, vec![9u8; 8 * 6 * 3])), // 好帧
        ];
        let feed = CameraFeed::with_source(
            Box::new(ScriptedSource { script, pos: 0 }),
            (16, 12),
            timing,
            Arc::clone(&clock) as Arc<dyn Clock>,
        );
        // 脚本耗尽 = EOF，线程退出；等它消化完
        let deadline = Instant::now() + Duration::from_secs(3);
        while feed.is_alive() {
            assert!(Instant::now() < deadline, "EOF 后线程该退出");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(feed.frames_skipped(), 2);
        assert_eq!(feed.frames_read(), 2);
        // 上行槽是最后一个好帧（9 填充），编码可用
        let b64 = feed.latest_jpeg_b64(60).expect("好帧应入槽");
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .unwrap();
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&jpeg[..]));
        let _pixels = decoder.decode().unwrap();
        assert_eq!(
            (
                decoder.info().unwrap().width,
                decoder.info().unwrap().height
            ),
            (8, 6)
        );
        // 预览槽：16x12（从 8x6 放大）
        let pv = feed.latest_preview().expect("好帧应有预览");
        assert_eq!((pv.width, pv.height), (16, 12));
    }

    #[test]
    fn eof_stops_thread_and_last_frame_stays() {
        let clock = Arc::new(MockClock::new());
        let timing = Arc::new(Timing::new());
        let script: Vec<Option<(u32, u32, Vec<u8>)>> = vec![
            Some((4, 4, vec![1u8; 4 * 4 * 3])),
            Some((4, 4, vec![2u8; 4 * 4 * 3])),
        ];
        let mut feed = CameraFeed::with_source(
            Box::new(ScriptedSource { script, pos: 0 }),
            (0, 0),
            timing,
            Arc::clone(&clock) as Arc<dyn Clock>,
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while feed.is_alive() {
            assert!(Instant::now() < deadline, "EOF 后线程该退出");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!feed.is_alive());
        assert_eq!(feed.frames_read(), 2);
        // EOF 不清槽（Python 语义：stop 才清）；最后一帧仍可取
        assert!(feed.latest_jpeg_b64(60).is_some());
        // stop 幂等且不挂
        feed.stop();
        assert!(!feed.is_alive());
        assert!(feed.latest_jpeg_b64(60).is_none(), "stop 后槽被清空");
        assert!(feed.latest_preview().is_none());
    }

    #[test]
    fn stop_is_idempotent_and_clears_slots() {
        let clock = Arc::new(MockClock::new());
        let mut feed = mock_feed(&clock, 8, 6, 100, (16, 12));
        clock.advance(Duration::from_millis(50));
        wait_frames_stable(&feed);
        assert!(feed.latest_jpeg_b64(60).is_some());
        assert!(feed.latest_preview().is_some());
        let t0 = Instant::now();
        feed.stop();
        assert!(t0.elapsed() < Duration::from_millis(2500), "stop 不该超时");
        assert!(!feed.is_alive());
        assert!(feed.latest_jpeg_b64(60).is_none());
        assert!(feed.latest_preview().is_none());
        // 幂等：再来一次立即返回
        let t1 = Instant::now();
        feed.stop();
        assert!(t1.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn cam_gap_recorded_from_mock_clock_beats() {
        let clock = Arc::new(MockClock::new());
        let timing = Arc::new(Timing::new());
        let source = MockFrameSource::new(8, 6, 10, Arc::clone(&clock) as Arc<dyn Clock>)
            .with_pattern(MockPattern::MovingBlock);
        let feed = CameraFeed::with_source(
            Box::new(source),
            (0, 0),
            Arc::clone(&timing),
            Arc::clone(&clock) as Arc<dyn Clock>,
        );
        // 每 100ms 推一次假时钟 → 每次一帧，帧间间隔（假时钟口径）= 100ms
        for _ in 0..4 {
            clock.advance(Duration::from_millis(100));
            wait_frames_stable(&feed);
        }
        assert!(feed.frames_read() >= 4);
        let w = timing.perf_window(1.0);
        assert!(
            (w.cam_gap_ms - 100.0).abs() < 1.0,
            "cam_gap 应为假时钟节拍 100ms，实测 {}",
            w.cam_gap_ms
        );
    }

    #[test]
    fn drop_without_stop_kills_thread() {
        // Drop 兜底：不显式 stop，线程也应被收掉（防 feeder 孤儿进程的语义）
        let clock = Arc::new(MockClock::new());
        let feed = mock_feed(&clock, 8, 6, 50, (0, 0));
        clock.advance(Duration::from_millis(20));
        wait_frames_stable(&feed);
        assert!(feed.is_alive());
        drop(feed);
        // （线程退出由 Drop 内的 stop 完成；这里只验证不 panic / 不挂死）
    }

    #[test]
    fn no_preview_size_means_no_preview() {
        let clock = Arc::new(MockClock::new());
        let feed = mock_feed(&clock, 8, 6, 30, (0, 0));
        clock.advance(Duration::from_millis(50));
        wait_frames_stable(&feed);
        assert!(feed.latest_preview().is_none());
        assert!(feed.latest_jpeg_b64(60).is_some(), "上行不受影响");
    }
}

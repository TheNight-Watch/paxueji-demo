//! 帧源抽象：把"从哪儿读 RGB888 帧"从 [`crate::CameraFeed`] 里解耦出来。
//!
//! - 设备（linux）：`feeder.rs` 的 MaixPy 喂流子进程源
//! - PC / 测试 / `--test-pc`：本文件的 [`MockFrameSource`]（合成帧：彩条/移动方块，
//!   可配置帧率，节奏走注入时钟）

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::clock::Clock;

/// RGB888 帧源：返回 `(width, height, data)`，`data.len() == w*h*3`。
///
/// - `read_frame` 返回 `None` 表示"这一刻没有新帧"（节流型源回 None，
///   相机线程小睡后重试；设备源在 stdout 上阻塞读，不会频繁回 None）。
/// - 永久结束（子进程退出 / EOF）用 [`FrameSource::is_finished`] 表达，
///   相机线程看到它就退出。
pub trait FrameSource: Send {
    /// 读下一帧；无新帧返回 `None`（非永久错误）。
    fn read_frame(&mut self) -> Option<(u32, u32, Vec<u8>)>;

    /// 源已永久结束（EOF/子进程退出）。默认 `false`（节流型源永不结束）。
    fn is_finished(&self) -> bool {
        false
    }
}

/// 合成帧图案。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MockPattern {
    /// 移动方块：水平灰度渐变背景 + 白色方块逐帧右移（帧与帧必然不同，
    /// 供"新帧后变化"类断言使用）。
    #[default]
    MovingBlock,
    /// 滚动彩条：8 色竖条（白黄青绿品红蓝黑）随帧号整体滚动。
    ColorBars,
}

/// 合成帧源（PC/测试）：按注入时钟的节拍产帧，图案随帧号变化。
///
/// `fps = 0` 表示不限速（每次 `read_frame` 立即产一帧，用于压力测试）。
/// 依赖注入的时钟决定节拍：与 [`crate::CameraFeed`] 共享同一个
/// [`MockClock`](crate::clock::MockClock) 即可用假时钟驱动整套节流逻辑。
pub struct MockFrameSource {
    clock: Arc<dyn Clock>,
    width: u32,
    height: u32,
    period: Option<Duration>,
    next_due: Instant,
    frame_idx: u64,
    pattern: MockPattern,
}

impl MockFrameSource {
    /// `fps` 帧率的合成源（`fps = 0` 不限速）。
    pub fn new(width: u32, height: u32, fps: u32, clock: Arc<dyn Clock>) -> Self {
        let period = if fps > 0 {
            Some(Duration::from_secs_f64(1.0 / fps as f64))
        } else {
            None
        };
        let next_due = clock.now();
        Self {
            clock,
            width,
            height,
            period,
            next_due,
            frame_idx: 0,
            pattern: MockPattern::default(),
        }
    }

    /// 换合成图案（默认 [`MockPattern::MovingBlock`]）。
    pub fn with_pattern(mut self, pattern: MockPattern) -> Self {
        self.pattern = pattern;
        self
    }

    /// 已产帧数（含被跳过的畸形帧之前的正常帧；测试同步用）。
    pub fn frames_generated(&self) -> u64 {
        self.frame_idx
    }

    fn render(&self, idx: u64) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut data = vec![0u8; w * h * 3];
        match self.pattern {
            MockPattern::MovingBlock => {
                // 背景：水平灰度渐变（行间一致，缩放/断言都好算）
                for x in 0..w {
                    let g = (x * 255 / w.max(1)) as u8;
                    for y in 0..h {
                        let i = (y * w + x) * 3;
                        data[i] = g;
                        data[i + 1] = g;
                        data[i + 2] = g;
                    }
                }
                // 白色方块逐帧右移（尺寸取帧的 1/4，保证任意尺寸下都能移动；
                // 帧号决定位置 → 帧间必然不同）
                let bw = (w / 4).max(1);
                let bh = (h / 4).max(1);
                let span = w.saturating_sub(bw).max(1);
                let bx = idx as usize % span;
                let by = (h / 2).saturating_sub(bh / 2);
                for y in by..(by + bh).min(h) {
                    for x in bx..(bx + bw).min(w) {
                        let i = (y * w + x) * 3;
                        data[i] = 255;
                        data[i + 1] = 255;
                        data[i + 2] = 255;
                    }
                }
            }
            MockPattern::ColorBars => {
                const BARS: [(u8, u8, u8); 8] = [
                    (255, 255, 255), // 白
                    (255, 255, 0),   // 黄
                    (0, 255, 255),   // 青
                    (0, 255, 0),     // 绿
                    (255, 0, 255),   // 品红
                    (255, 0, 0),     // 红
                    (0, 0, 255),     // 蓝
                    (0, 0, 0),       // 黑
                ];
                let shift = idx as usize % BARS.len();
                for x in 0..w {
                    let bar = (x * BARS.len() / w.max(1) + shift) % BARS.len();
                    let (r, g, b) = BARS[bar];
                    for y in 0..h {
                        let i = (y * w + x) * 3;
                        data[i] = r;
                        data[i + 1] = g;
                        data[i + 2] = b;
                    }
                }
            }
        }
        data
    }
}

impl FrameSource for MockFrameSource {
    fn read_frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        if let Some(period) = self.period {
            let now = self.clock.now();
            if now < self.next_due {
                return None; // 还没到下一帧时刻（小睡重试）
            }
            // 固定节拍推进；落后时钟太多（测试一次性快进很久）则重同步，
            // 避免一口气补生成海量帧。
            self.next_due += period;
            if now >= self.next_due + Duration::from_secs(1) {
                self.next_due = now + period;
            }
        }
        self.frame_idx += 1;
        Some((self.width, self.height, self.render(self.frame_idx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{MockClock, SystemClock};

    #[test]
    fn mock_paces_by_injected_clock() {
        let clock = Arc::new(MockClock::new());
        let mut src = MockFrameSource::new(4, 4, 10, clock.clone()); // 100ms/帧
        assert!(src.read_frame().is_some()); // 首帧立即
        assert!(src.read_frame().is_none(), "时钟未推进，不该有第二帧");
        clock.advance(Duration::from_millis(50));
        assert!(src.read_frame().is_none(), "半周期仍不该有帧");
        clock.advance(Duration::from_millis(60));
        assert!(src.read_frame().is_some());
        assert!(!src.is_finished());
    }

    #[test]
    fn mock_frames_differ_between_renders() {
        // 移动方块 / 滚动彩条都要保证帧与帧不同（上行"新帧后变化"测试的前提）
        let clock: Arc<dyn Clock> = Arc::new(MockClock::new());
        let mut src = MockFrameSource::new(64, 48, 0, Arc::clone(&clock))
            .with_pattern(MockPattern::MovingBlock);
        let (_, _, a) = src.read_frame().unwrap();
        let (_, _, b) = src.read_frame().unwrap();
        assert_ne!(a, b);
        let mut src = MockFrameSource::new(64, 48, 0, clock).with_pattern(MockPattern::ColorBars);
        let (_, _, a) = src.read_frame().unwrap();
        let (_, _, b) = src.read_frame().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn mock_output_dimensions_and_len() {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut src = MockFrameSource::new(7, 5, 0, clock);
        let (w, h, data) = src.read_frame().unwrap();
        assert_eq!((w, h), (7, 5));
        assert_eq!(data.len(), 7 * 5 * 3);
    }

    #[test]
    fn default_pattern_is_moving_block_and_fps_zero_is_unlimited() {
        let clock = Arc::new(MockClock::new());
        let mut src = MockFrameSource::new(8, 8, 0, clock);
        assert_eq!(src.pattern, MockPattern::MovingBlock);
        assert!(src.period.is_none());
        // 不限速：连续读都有帧
        for _ in 0..5 {
            assert!(src.read_frame().is_some());
        }
        assert_eq!(src.frames_generated(), 5);
    }

    #[test]
    fn moving_block_pattern_is_deterministic_per_index() {
        // 同一帧号渲染结果一致（b64 缓存断言的前提）
        let clock = Arc::new(MockClock::new());
        let a = {
            let mut s = MockFrameSource::new(32, 32, 0, clock.clone());
            for _ in 0..3 {
                s.read_frame();
            }
            s.render(3)
        };
        let b = {
            let mut s = MockFrameSource::new(32, 32, 0, clock);
            for _ in 0..7 {
                s.read_frame();
            }
            s.render(3)
        };
        assert_eq!(a, b);
        assert_eq!(a.len(), 32 * 32 * 3);
    }

    #[test]
    fn tiny_frame_renders_without_panic() {
        // 1x1 / 极小尺寸（方块小于 16x16 时退化为整帧）
        let clock = Arc::new(MockClock::new());
        let mut src = MockFrameSource::new(1, 1, 0, clock);
        let (w, h, data) = src.read_frame().unwrap();
        assert_eq!((w, h), (1, 1));
        assert_eq!(data.len(), 3);
    }
}

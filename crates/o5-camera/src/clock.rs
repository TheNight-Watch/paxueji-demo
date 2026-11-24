//! 可注入时钟：把 `Instant` 包一层 trait，让相机线程的节拍（1s 上行 / 0.125s 预览）
//! 在单测里可以用假时钟快进，避免真 sleep 拖慢测试（小 sleep 仍可接受）。
//!
//! Python 版直接用 `time.monotonic()`；Rust 版语义相同（单调时钟），只是多了注入口。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 单调时钟抽象（`Instant` 的包一层）。
///
/// 相机线程内的所有节拍判断（帧间间隔、1s 上行节流、0.125s 预览节流）都走它；
/// 生产路径用 [`SystemClock`]，测试注入 [`MockClock`]。
pub trait Clock: Send + Sync {
    /// 当前单调时刻。
    fn now(&self) -> Instant;
}

/// 真实时钟（`Instant::now()`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// 假时钟：内部计数器只增不减（保持单调），测试线程用 [`MockClock::advance`] 快进。
///
/// 多线程共享：计数器是原子量，相机线程与测试线程可并发读/推进，
/// 真实时间上仍有毫秒级的调度延迟，但"逻辑时刻"完全由测试控制。
#[derive(Debug)]
pub struct MockClock {
    origin: Instant,
    offset_ms: AtomicU64,
}

impl MockClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            offset_ms: AtomicU64::new(0),
        }
    }

    /// 逻辑时间前移 `dur`（只能向前推；跨线程立即可见）。
    pub fn advance(&self, dur: Duration) {
        let add = dur.as_millis() as u64; // 亚毫秒截断足够测试用
        self.offset_ms.fetch_add(add, Ordering::Release);
    }
}

impl Default for MockClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MockClock {
    fn now(&self) -> Instant {
        let ms = self.offset_ms.load(Ordering::Acquire);
        // 测试里推的量级是秒；checked_add 防御极端溢出（溢出时钉在 origin）
        self.origin
            .checked_add(Duration::from_millis(ms))
            .unwrap_or(self.origin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_clock_is_monotonic_and_advanceable() {
        let c = MockClock::new();
        let t0 = c.now();
        assert_eq!(t0, c.now());
        c.advance(Duration::from_millis(1500));
        let t1 = c.now();
        assert!(t1 > t0);
        assert!((t1 - t0).as_secs_f64() >= 1.5);
        c.advance(Duration::from_millis(1));
        assert!(c.now() >= t1);
    }

    #[test]
    fn system_clock_moves() {
        let c = SystemClock;
        let t0 = c.now();
        std::thread::sleep(Duration::from_millis(2));
        assert!(c.now() >= t0);
    }
}

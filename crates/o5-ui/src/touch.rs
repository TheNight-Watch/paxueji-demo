//! 触摸输入：evdev 直读（设备）+ 竖屏→横屏 8 变体映射 + mock（PC 测试）。
//!
//! 设备事实（device-findings）：
//! - `/dev/input/event1`（hyn_ts，Hynitron 电容屏），MT type-B 协议；
//! - `input_event` 为 64-bit 时间戳结构（`qqHHi` 24 字节，16 字节格式会 EINVAL）；
//! - EVIOCGABS 实测 **MT_POSITION_X: 0..480，MT_POSITION_Y: 0..640——
//!   原始坐标是竖屏空间（宽 480 × 高 640）**，映射到 640x480 横屏需要
//!   swap/flip 组合（8 变体之一，config `device.touchVariant`，默认 2，
//!   变体 2 为 tap_test.py 在 device 02 上实测校准的结果）。
//!
//! 按下→抬起 = 一次点击（沿检测在 UI 主循环做，这里只报告当前状态）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use o5_config::Config;

/// 触摸屏原始坐标范围（实测：X 0..480，Y 0..640，竖屏空间）。
pub const RAW_X_MAX: f64 = 480.0;
pub const RAW_Y_MAX: f64 = 640.0;

/// 一次触摸采样（已映射到 640x480 屏幕坐标）。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TouchSample {
    pub x: i32,
    pub y: i32,
    pub pressed: bool,
}

impl TouchSample {
    pub fn idle() -> TouchSample {
        TouchSample {
            x: 0,
            y: 0,
            pressed: false,
        }
    }
}

/// 触摸源（设备 = evdev，测试 = mock 可注入）。
pub trait TouchSource: Send {
    /// 非阻塞：返回当前触摸状态（屏幕坐标）。50Hz 轮询。
    fn poll(&mut self) -> TouchSample;
}

// ---------------------------------------------------------------------------
// 竖屏 → 横屏 8 变体映射表（照 tools/screen_tool.py `map_screen_to_raw`
// 的变体枚举补全成完整 swap/flip 立方；v0..v5 与 screen_tool 逐条一致，
// v6/v7 是补上的直接 + 单翻转两个组合）。
//
// 归一化：nx = raw_x / 480，ny = raw_y / 640（原始竖屏轴），
//         ux = sx / 639，uy = sy / 479（横屏轴，照 tap_test.py 的归一化）。
//
// 变体表（raw → screen 方向，即 (ux, uy) 关于 (nx, ny) 的表达式）：
//   v0 (直接)        : (nx,     ny)
//   v1 (交换)        : (ny,     nx)
//   v2 (交换+Y翻转)  : (ny,     1-nx)   ← 默认；tap_test.py 实测校准
//   v3 (交换+X翻转)  : (1-ny,   nx)
//   v4 (双翻转)      : (1-nx,   1-ny)
//   v5 (交换+双翻转) : (1-ny,   1-nx)
//   v6 (直接+Y翻转)  : (nx,     1-ny)
//   v7 (直接+X翻转)  : (1-nx,   ny)
// ---------------------------------------------------------------------------

/// 原始竖屏坐标 → 横屏屏幕坐标（variant 取自 config `device.touchVariant`）。
/// 归一化口径与 tap_test.py 一致：raw 轴除 480/640，屏幕轴乘 639/479。
pub fn map_raw_to_screen(raw_x: i32, raw_y: i32, variant: u8) -> (i32, i32) {
    let nx = (raw_x as f64 / RAW_X_MAX).clamp(0.0, 1.0);
    let ny = (raw_y as f64 / RAW_Y_MAX).clamp(0.0, 1.0);
    let (ux, uy) = variant_units(variant, nx, ny);
    (
        (ux * 639.0).round() as i32,
        (uy * 479.0).round() as i32,
    )
}

/// 屏幕坐标 → 原始竖屏坐标（设备自检/注入用，screen_tool.py 的方向：
/// `ux, uy = sx / 639.0, sy / 479.0` 的归一化）。
pub fn map_screen_to_raw(sx: i32, sy: i32, variant: u8) -> (i32, i32) {
    let ux = (sx as f64 / 639.0).clamp(0.0, 1.0);
    let uy = (sy as f64 / 479.0).clamp(0.0, 1.0);
    // 求逆：把 (ux, uy) 代回变体的原始方向公式
    let (nx, ny) = match variant % 8 {
        0 => (ux, uy),
        1 => (uy, ux),
        2 => (1.0 - uy, ux),
        3 => (uy, 1.0 - ux),
        4 => (1.0 - ux, 1.0 - uy),
        5 => (1.0 - uy, 1.0 - ux),
        6 => (ux, 1.0 - uy),
        7 => (1.0 - ux, uy),
        _ => unreachable!(),
    };
    (
        (nx * RAW_X_MAX).round() as i32,
        (ny * RAW_Y_MAX).round() as i32,
    )
}

/// raw → screen 方向的变体表（(ux, uy) = f(nx, ny)）。
fn variant_units(variant: u8, nx: f64, ny: f64) -> (f64, f64) {
    match variant % 8 {
        0 => (nx, ny),
        1 => (ny, nx),
        2 => (ny, 1.0 - nx),
        3 => (1.0 - ny, nx),
        4 => (1.0 - nx, 1.0 - ny),
        5 => (1.0 - ny, 1.0 - nx),
        6 => (nx, 1.0 - ny),
        7 => (1.0 - nx, ny),
        _ => unreachable!(),
    }
}

/// 8 变体四角映射一览（设备自检模式用，写 /tmp 供标定参考）。
pub fn variant_selftest_report() -> String {
    let mut s = String::from("touch variant table (raw 480x640 -> screen 640x480):\n");
    for v in 0..8u8 {
        s.push_str(&format!(
            "v{}: (0,0)->{:?} (480,640)->{:?} (480,0)->{:?} (0,640)->{:?}\n",
            v,
            map_raw_to_screen(0, 0, v),
            map_raw_to_screen(480, 640, v),
            map_raw_to_screen(480, 0, v),
            map_raw_to_screen(0, 640, v),
        ));
    }
    s
}

/// 打开默认触摸源：设备上 evdev（/dev/input/event1），失败回退空闲 mock；
/// PC 上恒为空闲 mock（Windows 无 /dev/input）。
pub fn open_default(cfg: &Config) -> Box<dyn TouchSource> {
    #[cfg(target_os = "linux")]
    {
        match evdev::EvdevTouch::open("/dev/input/event1", cfg.device_touch_variant) {
            Ok(t) => Box::new(t),
            Err(e) => {
                log::warn!("打开触摸设备失败（{}），触摸不可用", e);
                Box::new(MockTouch::idle())
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // PC：无 evdev；返回可注入的 mock（默认空闲）
        let _ = cfg;
        Box::new(MockTouch::idle())
    }
}

/// 可注入的 mock 触摸源（测试 / --test-pc 用）。
pub struct MockTouch {
    queue: Arc<Mutex<VecDeque<TouchSample>>>,
    current: Arc<Mutex<TouchSample>>,
}

impl MockTouch {
    pub fn idle() -> MockTouch {
        MockTouch {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            current: Arc::new(Mutex::new(TouchSample::idle())),
        }
    }

    /// 注入一个采样（队列形式；poll 逐个弹出，空后保持最后状态）。
    pub fn push(&self, s: TouchSample) {
        self.queue.lock().unwrap().push_back(s);
    }

    /// 共享句柄（跨线程注入用）。
    pub fn handle(&self) -> MockTouchHandle {
        MockTouchHandle {
            queue: Arc::clone(&self.queue),
            current: Arc::clone(&self.current),
        }
    }
}

impl TouchSource for MockTouch {
    fn poll(&mut self) -> TouchSample {
        let next = self.queue.lock().unwrap().pop_front();
        if let Some(s) = next {
            *self.current.lock().unwrap() = s;
        }
        *self.current.lock().unwrap()
    }
}

/// [`MockTouch`] 的跨线程注入句柄。
pub struct MockTouchHandle {
    queue: Arc<Mutex<VecDeque<TouchSample>>>,
    current: Arc<Mutex<TouchSample>>,
}

impl MockTouchHandle {
    pub fn push(&self, s: TouchSample) {
        self.queue.lock().unwrap().push_back(s);
    }

    pub fn set_current(&self, s: TouchSample) {
        *self.current.lock().unwrap() = s;
    }
}

// ---------------------------------------------------------------------------
// evdev 直读（仅 Linux）
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub mod evdev {
    use super::{map_raw_to_screen, TouchSample, TouchSource};
    use std::fs::{File, OpenOptions};
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    // evdev 常量（linux/input-event-codes.h）
    const EV_SYN: u16 = 0x00;
    const EV_KEY: u16 = 0x01;
    const EV_ABS: u16 = 0x03;
    const SYN_REPORT: u16 = 0x00;
    const BTN_TOOL_FINGER: u16 = 0x145;
    const BTN_TOUCH: u16 = 0x14A;
    const ABS_X: u16 = 0x00;
    const ABS_Y: u16 = 0x01;
    const ABS_MT_POSITION_X: u16 = 0x35;
    const ABS_MT_POSITION_Y: u16 = 0x36;
    const ABS_MT_TRACKING_ID: u16 = 0x39;

    /// `struct input_event` 的字节长度（64-bit 时间戳布局：
    /// sec:i64, usec:i64, type:u16, code:u16, value:i32；16 字节格式会 EINVAL）。
    pub const INPUT_EVENT_SIZE: usize = 24;

    /// 事件流解码后的中间状态（驱动只在变化时上报，跨 read 保持）。
    #[derive(Debug)]
    struct EvState {
        raw_x: i32,
        raw_y: i32,
        tracking: i32,
        btn: bool,
        /// 本轮 poll 内已见过按下（drain 内完整点击的合成用）。
        seen_press_in_poll: bool,
    }

    /// evdev 触摸读取器（MT type-B 单点跟踪）。
    pub struct EvdevTouch {
        file: File,
        variant: u8,
        /// 半条记录的接续缓冲（read 常按整块来，防御性处理）。
        carry: Vec<u8>,
        st: EvState,
        /// 当前（SYN 提交后的）状态。
        cur: TouchSample,
        /// 一轮 drain 内出现完整按下→抬起时，下一拍补发释放沿。
        pending_release: bool,
    }

    impl EvdevTouch {
        pub fn open(path: &str, variant: u8) -> std::io::Result<EvdevTouch> {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)?;
            Ok(EvdevTouch {
                file,
                variant,
                carry: Vec::new(),
                st: EvState {
                    raw_x: 0,
                    raw_y: 0,
                    tracking: -1,
                    btn: false,
                    seen_press_in_poll: false,
                },
                cur: TouchSample::idle(),
                pending_release: false,
            })
        }

        fn apply_event(&mut self, ev_type: u16, code: u16, value: i32) {
            let st = &mut self.st;
            match (ev_type, code) {
                (EV_ABS, ABS_MT_POSITION_X) | (EV_ABS, ABS_X) => st.raw_x = value,
                (EV_ABS, ABS_MT_POSITION_Y) | (EV_ABS, ABS_Y) => st.raw_y = value,
                (EV_ABS, ABS_MT_TRACKING_ID) => st.tracking = value,
                (EV_KEY, BTN_TOUCH) | (EV_KEY, BTN_TOOL_FINGER) => st.btn = value != 0,
                (EV_SYN, SYN_REPORT) => {
                    // 一次完整采样报告：按下判定 = tracking 有效或 BTN 按下
                    let pressed = st.tracking >= 0 || st.btn;
                    let (sx, sy) = map_raw_to_screen(st.raw_x, st.raw_y, self.variant);
                    if pressed {
                        st.seen_press_in_poll = true;
                        self.cur = TouchSample {
                            x: sx,
                            y: sy,
                            pressed: true,
                        };
                    } else if st.seen_press_in_poll {
                        // 本轮 drain 内出现了完整按下→抬起（tap 快于一个轮询周期）：
                        // 当前拍报按下、下一拍补释放，UI 沿检测不丢这次点击
                        self.pending_release = true;
                    } else {
                        self.cur = TouchSample {
                            x: sx,
                            y: sy,
                            pressed: false,
                        };
                    }
                }
                _ => {}
            }
        }
    }

    impl TouchSource for EvdevTouch {
        fn poll(&mut self) -> TouchSample {
            if self.pending_release {
                self.pending_release = false;
                self.cur.pressed = false;
                return self.cur;
            }
            self.st.seen_press_in_poll = false;
            // 非阻塞 drain：读到 EAGAIN/EOF 为止
            let mut buf = [0u8; 4096];
            loop {
                match self.file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        self.carry.extend_from_slice(&buf[..n]);
                        let whole = self.carry.len() / INPUT_EVENT_SIZE * INPUT_EVENT_SIZE;
                        let events: Vec<u8> = self.carry.drain(..whole).collect();
                        for rec in events.chunks_exact(INPUT_EVENT_SIZE) {
                            let ev_type = u16::from_ne_bytes([rec[16], rec[17]]);
                            let code = u16::from_ne_bytes([rec[18], rec[19]]);
                            let value = i32::from_ne_bytes([rec[20], rec[21], rec[22], rec[23]]);
                            self.apply_event(ev_type, code, value);
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            self.cur
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant2_matches_tap_test_calibration() {
        // tap_test.py（device 02 实测校准）：nx, ny = 1-uy, ux。
        // 逆映射：screen_x = raw_y，screen_y = 480 - raw_x。
        assert_eq!(map_raw_to_screen(0, 0, 2), (0, 479));
        assert_eq!(map_raw_to_screen(480, 640, 2), (639, 0));
        // raw (240, 320) → 屏幕 (320, 240)
        assert_eq!(map_raw_to_screen(240, 320, 2), (320, 240));
        // 四分之一点：ux=0.25*639=159.75→160, uy=0.75*479=359.25→359
        assert_eq!(map_raw_to_screen(120, 160, 2), (160, 359));
    }

    #[test]
    fn all_variants_roundtrip() {
        // screen_tool.py 正向表（v0..v5 与其逐条一致）+ v6/v7 补全：
        // 屏幕 → raw → 屏幕 应回到原坐标（±1px 归一化误差）。
        let spots = [(0, 0), (639, 479), (320, 240), (100, 400), (600, 20)];
        for v in 0..8u8 {
            for &(sx, sy) in &spots {
                let (rx, ry) = map_screen_to_raw(sx, sy, v);
                assert!(
                    (0..=480).contains(&rx) && (0..=640).contains(&ry),
                    "raw 越界 v{}: {:?}",
                    v,
                    (rx, ry)
                );
                let (bx, by) = map_raw_to_screen(rx, ry, v);
                assert!(
                    (bx - sx).abs() <= 1 && (by - sy).abs() <= 1,
                    "v{} roundtrip 失败: ({}, {}) -> ({}, {}) -> ({}, {})",
                    v,
                    sx,
                    sy,
                    rx,
                    ry,
                    bx,
                    by
                );
            }
        }
    }

    #[test]
    fn variant_table_corners() {
        // v0（直接）：raw 轴与屏幕轴同向（仅适用于触摸横向报告的面板）
        assert_eq!(map_raw_to_screen(0, 0, 0), (0, 0));
        assert_eq!(map_raw_to_screen(480, 640, 0), (639, 479));
        // v1（交换）
        assert_eq!(map_raw_to_screen(0, 640, 1), (639, 0));
        assert_eq!(map_raw_to_screen(480, 0, 1), (0, 479));
        // v3（交换+X翻转）
        assert_eq!(map_raw_to_screen(480, 0, 3), (639, 479));
        // v4（双翻转）
        assert_eq!(map_raw_to_screen(0, 0, 4), (639, 479));
        assert_eq!(map_raw_to_screen(480, 640, 4), (0, 0));
        // v5（交换+双翻转）：(nx,ny)=(0,0) → (ux,uy)=(1,1) → (639,479)
        assert_eq!(map_raw_to_screen(0, 0, 5), (639, 479));
        // v6（直接+Y翻转）/ v7（直接+X翻转）
        assert_eq!(map_raw_to_screen(480, 0, 6), (639, 479));
        assert_eq!(map_raw_to_screen(0, 0, 7), (639, 0));
        // v2 与 v6 在两角同值、四分点不同（表不重复）
        assert_eq!(map_raw_to_screen(480, 0, 2), (0, 0));
        assert_ne!(map_raw_to_screen(480, 0, 2), map_raw_to_screen(480, 0, 6));
    }

    #[test]
    fn screen_tool_forward_formula_v2() {
        // screen_tool.py variant 2 正向公式：nx, ny = 1.0 - uy, ux
        let (rx, ry) = map_screen_to_raw(320, 240, 2);
        let ux: f64 = 320.0 / 639.0;
        let uy: f64 = 240.0 / 479.0;
        let ex = ((1.0 - uy) * 480.0).round() as i32;
        let ey = (ux * 640.0).round() as i32;
        assert_eq!((rx, ry), (ex, ey));
    }

    #[test]
    fn mock_poll_semantics() {
        let mut t = MockTouch::idle();
        assert!(!t.poll().pressed);
        t.push(TouchSample {
            x: 10,
            y: 20,
            pressed: true,
        });
        assert_eq!(t.poll(), TouchSample { x: 10, y: 20, pressed: true });
        // 队列空后保持最后状态
        assert_eq!(t.poll(), TouchSample { x: 10, y: 20, pressed: true });
        t.push(TouchSample {
            x: 10,
            y: 20,
            pressed: false,
        });
        assert!(!t.poll().pressed);
        assert!(!t.poll().pressed);
    }

    #[test]
    fn mock_handle_cross_thread() {
        let mut t = MockTouch::idle();
        let h = t.handle();
        h.push(TouchSample {
            x: 5,
            y: 6,
            pressed: true,
        });
        let s = t.poll();
        assert_eq!((s.x, s.y, s.pressed), (5, 6, true));
    }

    #[test]
    fn selftest_report_writes_all_variants() {
        let r = variant_selftest_report();
        assert!(r.contains("v0:") && r.contains("v7:"));
        assert!(r.contains("screen"));
    }
}

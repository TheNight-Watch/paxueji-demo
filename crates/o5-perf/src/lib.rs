//! o5-perf：实时性埋点、`[perf]` 周期采样与网络模式。
//!
//! Python 版 `o5/timing.py` + `o5/perf.py` + `o5/netmode.py` 的忠实移植，拆成三个模块：
//!
//! # timing（o5/timing.py）
//!
//! 实时性埋点：四个关键时刻 + 滚动统计。
//!
//! 对应测试报告 TODO 的四个时间点：
//! 1. `unit_recorded`  录到每秒音频（录音线程凑满 1s）
//! 2. `unit_sent`      发出 unit（WebSocket send 完成）
//! 3. `audio_received` 收到第一段模型音频（response.spoken.delta.audio，按“回合”口径）
//! 4. `playback_started` 扬声器开始播放（播放线程突发首块）
//!
//! 派生指标：采集->发送滞后、发送->committed ACK RTT、收音->出声滞后、会话建立耗时。
//!
//! # proc_perf（o5/perf.py）
//!
//! `[perf]` 周期行：进程/线程 CPU、WiFi 信号 + 各组件埋点窗口汇总。
//!
//! 卡顿归因用的总账（每 10s 一行，常驻，不依赖会话）：
//! - 进程 CPU%（两核 A53，满载 200%）+ 占用最高的线程名 -> 回答“设备处理能力差在哪”
//! - WiFi 信号 dBm + 应用层收发 KB/s -> 回答“是不是带宽/网络问题”
//! - session 事件循环滞后、UI 帧耗时、JPEG 编码/大小、扬声器块间隙 -> 回答“卡在哪个环节”
//!
//! 读 /proc 实现（无 subprocess，开销可忽略）：
//! - `/proc/self/stat`          utime+stime（时钟 tick）
//! - `/proc/self/task/*/stat`   各线程 utime+stime；comm 取线程名（Python 设置过 prname）
//! - `/proc/net/wireless`       信号电平（dBm 取负）
//!
//! /proc 读取仅在 linux 编译；其它平台（PC 联测环境）CPU 显示 `n/a`、
//! 埋点窗口照常汇总，不 panic。
//!
//! # netmode（o5/netmode.py）
//!
//! 网络模式：WiFi 直连（默认）/ USB 中继（经电脑转发）。
//!
//! 配置里的端点永远写标准 443 地址；运行时按 `net_mode` 决定实际连接：
//! - wifi：端点原样使用，并清掉 /etc/hosts 里的中继映射块
//! - usb ：URL 域名命中中继 portMap 时改写为中继端口，同时在
//!   /etc/hosts 写入标记块（域名 -> 电脑 USB 网卡 IP）。TLS 的
//!   SNI / 证书校验仍按真实域名走（PC 中继只透传 TCP）。
//!
//! /etc/hosts 用 BEGIN/END 标记块管理，幂等，切回 wifi 自动移除；
//! 非 root / 文件不可写时降级为提示（不影响进程运行）。

pub mod netmode;
pub mod proc_perf;
pub mod timing;

pub use proc_perf::{PerfMonitor, PerfSnapshot};
pub use timing::{Metric, PerfWindow, Rolling, Timing, TimingAvgs, TimingSnapshot};

pub(crate) mod util {
    use std::sync::{Mutex, MutexGuard};

    /// 加锁（毒化也不中断统计——GIL 语义的近似）。
    pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Python `round(v, n)`（对二进制精确值做十进制最近舍入、平局取偶）——快照/日志的数值口径对齐。
    /// 借道 Rust 定点格式化（与 Python 一样按精确十进制展开舍入，先乘 10^n 会丢掉平局信息）。
    pub(crate) fn py_round(v: f64, digits: i32) -> f64 {
        if !v.is_finite() {
            return v;
        }
        let digits = digits.clamp(0, 20) as usize;
        match format!("{v:.digits$}").parse::<f64>() {
            Ok(r) => r,
            Err(_) => v,
        }
    }

    /// Python `str(float)`：整数值补 `.0`，其余取最短往返十进制。
    pub(crate) fn py_f64_str(v: f64) -> String {
        let s = format!("{v}");
        if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
            s
        } else {
            format!("{s}.0")
        }
    }

    /// 周期值消毒：钳到 [0.05s, 24h]；NaN/无穷回退 10s
    ///（`f64::clamp` 对 NaN 仍返回 NaN，而 `Duration::from_secs_f64` 会 panic）。
    pub(crate) fn sanitize_interval_s(v: f64) -> f64 {
        if v.is_finite() {
            v.clamp(0.05, 86_400.0)
        } else {
            10.0
        }
    }

    /// 设置当前线程名（≤15 字符，linux 上 /proc/<pid>/task/*/comm 可见；其它平台 no-op）。
    pub(crate) fn set_thread_name(name: &str) {
        #[cfg(target_os = "linux")]
        {
            if let Ok(c) = std::ffi::CString::new(name) {
                unsafe { libc::pthread_setname_np(libc::pthread_self(), c.as_ptr()) };
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::util::{py_f64_str, py_round};

    #[test]
    fn py_round_matches_python() {
        assert_eq!(py_round(12.25, 1), 12.2); // 精确平局 -> 取偶
        assert_eq!(py_round(12.35, 1), 12.3); // 12.35 的二进制值略小（Python 同）
        assert_eq!(py_round(20.55, 1), 20.6); // 20.55 的二进制值略大（Python 同）
        assert_eq!(py_round(2.5, 0), 2.0);
        assert_eq!(py_round(3.5, 0), 4.0);
        assert_eq!(py_round(-2.5, 0), -2.0);
        assert_eq!(py_round(20.6, 1), 20.6);
        assert_eq!(py_round(2.0, 2), 2.0);
        assert_eq!(py_round(100.0, 1), 100.0);
        assert_eq!(py_round(6.4, 0), 6.0);
    }

    #[test]
    fn py_f64_str_matches_python() {
        assert_eq!(py_f64_str(12.0), "12.0");
        assert_eq!(py_f64_str(12.3), "12.3");
        assert_eq!(py_f64_str(0.5), "0.5");
        assert_eq!(py_f64_str(2.0), "2.0");
        assert_eq!(py_f64_str(3900.0), "3900.0");
    }
}

/// 音频链路 trace（卡顿归因用）：环境变量 PAXUEJI_TRACE=1 时输出
/// `[trace] <unix 毫秒> <事件> k=v ...`，正式运行不输出、零开销分支。
pub mod trace {
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var("PAXUEJI_TRACE").is_ok_and(|v| v == "1"))
    }

    pub fn now_ms() -> u128 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
    }
}

#[macro_export]
macro_rules! trace_ev {
    ($ev:expr $(, $k:ident = $v:expr)* $(,)?) => {
        if $crate::trace::enabled() {
            let mut s = format!("[trace] {} {}", $crate::trace::now_ms(), $ev);
            $( s.push_str(&format!(" {}={}", stringify!($k), $v)); )*
            log::info!(target: "trace", "{}", s);
        }
    };
}

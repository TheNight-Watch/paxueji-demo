//! `[perf]` 周期行：进程/线程 CPU、WiFi 信号 + 各组件埋点窗口汇总。（`o5/perf.py` 的忠实移植）
//!
//! 卡顿归因用的总账（每 10s 一行，常驻，不依赖会话）：
//! - 进程 CPU%（两核 A53，满载 200%）+ 占用最高的线程名 -> 回答“设备处理能力差在哪”
//! - WiFi 信号 dBm + 应用层收发 KB/s -> 回答“是不是带宽/网络问题”
//! - session 事件循环滞后、UI 帧耗时、JPEG 编码/大小、扬声器块间隙 -> 回答“卡在哪个环节”
//!
//! 读 /proc 实现（无 subprocess，开销可忽略）：
//! - `/proc/self/stat`          utime+stime（时钟 tick）
//! - `/proc/self/task/*/stat`   各线程 utime+stime；comm 取线程名（线程创建时可 prname）
//! - `/proc/net/wireless`       信号电平（dBm 取负）
//!
//! /proc 读取仅在 linux 编译（`#[cfg(target_os = "linux")]`）；Windows/PC 联测环境
//! 降级为 “CPU n/a（无 /proc）” 头，埋点窗口照常汇总，不 panic。

use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::timing::{PerfWindow, Timing, TimingAvgs};
use crate::util::{lock, py_round};

/// 纯解析：`/proc/self/stat`（或 `task/*/stat`）一行 -> utime+stime 时钟 tick。
/// 字段口径：comm 的右括号之后第 12/13 个（0 基 11/12）。
pub fn parse_proc_stat_ticks(line: &str) -> Option<u64> {
    let rest = line.get(line.rfind(')')? + 2..)?;
    let mut fields = rest.split_whitespace();
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime + stime)
}

/// 纯解析：`/proc/net/wireless` 内容 -> 信号 dBm 文本（与 Python `_wifi_level_dbm` 同口径）。
/// link level 原始值是负 dBm*10 还是正数视内核而定，统一换算成 dBm。
pub fn wifi_level_dbm(content: &str) -> String {
    for line in content.lines().skip(2) {
        let mut parts = line.split_whitespace();
        let Some(iface) = parts.next() else { continue };
        if iface.starts_with("wlan") {
            // parts[3]：signal 电平；解析失败按 Python 异常口径直接返回 "-"
            if let Some(lvl) = parts.nth(2).and_then(|v| v.parse::<f64>().ok()) {
                let dbm = if lvl > 0.0 { lvl / 10.0 } else { lvl };
                return format!("{dbm:.0}dBm");
            }
            return "-".to_string();
        }
    }
    "-".to_string()
}

/// linux 下的 /proc 采样。
#[cfg(target_os = "linux")]
mod procfs {
    use std::collections::BTreeMap;
    use std::fs;

    use super::parse_proc_stat_ticks;

    /// 一轮 /proc 采样基线（进程 + 各线程 tick）。
    pub(super) struct Baseline {
        pub proc_ticks: u64,
        pub threads: BTreeMap<u32, (String, u64)>,
    }

    /// 构造时采样；/proc 不可用返回 None（此后不再重试，等价 Python `_proc_ok=False`）。
    pub(super) fn sample() -> Option<Baseline> {
        Some(Baseline {
            proc_ticks: read_proc_ticks().ok()?,
            threads: read_thread_ticks(),
        })
    }

    /// `/proc/self/stat`：utime+stime（时钟 tick）。
    pub(super) fn read_proc_ticks() -> std::io::Result<u64> {
        let stat = fs::read_to_string("/proc/self/stat")?;
        parse_proc_stat_ticks(&stat).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "bad /proc/self/stat")
        })
    }

    /// `/proc/self/task/*/stat` + `comm`：tid -> (线程名, utime+stime)。
    /// 读失败的条目跳过（Python 逐 tid try/except 的口径）。
    pub(super) fn read_thread_ticks() -> BTreeMap<u32, (String, u64)> {
        let mut out = BTreeMap::new();
        let Ok(entries) = fs::read_dir("/proc/self/task") else {
            return out;
        };
        for entry in entries.flatten() {
            let Ok(tid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(comm) = fs::read_to_string(format!("/proc/self/task/{tid}/comm")) else {
                continue;
            };
            let trimmed = comm.trim();
            let name = if trimmed.is_empty() {
                tid.to_string()
            } else {
                trimmed.to_string()
            };
            let Ok(stat) = fs::read_to_string(format!("/proc/self/task/{tid}/stat")) else {
                continue;
            };
            if let Some(ticks) = parse_proc_stat_ticks(&stat) {
                out.insert(tid, (name, ticks));
            }
        }
        out
    }

    /// `/proc/net/wireless`：信号电平（原始正值按 dBm*10 换算）。
    pub(super) fn wifi_level() -> String {
        match fs::read_to_string("/proc/net/wireless") {
            Ok(content) => super::wifi_level_dbm(&content),
            Err(_) => "-".to_string(),
        }
    }

    /// SC_CLK_TCK（通常 100）；sysconf 失败按 100。
    pub(super) fn clk_tck() -> f64 {
        let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if v > 0 {
            v as f64
        } else {
            100.0
        }
    }
}

/// 最近一个统计窗口的 CPU/wifi/速率（性能面板消费；无 /proc 的平台相应字段为 None/0）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PerfSnapshot {
    pub cpu_pct: Option<i64>,
    pub wifi_dbm: Option<String>,
    pub tx_kbs: i64,
    pub rx_kbs: i64,
    pub ui_fps: f64,
}

struct MonitorState {
    wall_t0: Instant,
    /// None = /proc 不可用（PC 联测环境），只保留埋点窗口。
    #[cfg(target_os = "linux")]
    baseline: Option<procfs::Baseline>,
}

impl MonitorState {
    fn new() -> Self {
        Self {
            wall_t0: Instant::now(),
            #[cfg(target_os = "linux")]
            baseline: procfs::sample(),
        }
    }
}

/// 10s 周期把埋点账本拼成一行 `[perf]` 日志（Python `PerfMonitor`）。
/// 常驻线程，无需显式停止；Clone 共享快照，供 UI 性能面板读。
#[derive(Clone)]
pub struct PerfMonitor {
    timing: Arc<Timing>,
    interval_s: f64,
    last: Arc<Mutex<PerfSnapshot>>,
    state: Arc<Mutex<MonitorState>>,
}

impl PerfMonitor {
    /// 默认 10s 周期。
    pub fn new(timing: Arc<Timing>) -> Self {
        Self::with_interval(timing, 10.0)
    }

    pub fn with_interval(timing: Arc<Timing>, interval_s: f64) -> Self {
        Self {
            timing,
            interval_s: crate::util::sanitize_interval_s(interval_s),
            last: Arc::new(Mutex::new(PerfSnapshot::default())),
            state: Arc::new(Mutex::new(MonitorState::new())),
        }
    }

    /// 最近一个统计窗口的 CPU/wifi/速率（无则默认值）。
    pub fn snapshot(&self) -> PerfSnapshot {
        lock(&self.last).clone()
    }

    /// 启动常驻采样线程（Python `start()`；线程名 o5-perf）。线程随进程退出，无需 join。
    pub fn spawn(self) -> JoinHandle<()> {
        thread::spawn(move || {
            crate::util::set_thread_name("o5-perf");
            loop {
                thread::sleep(Duration::from_secs_f64(self.interval_s));
                self.report();
            }
        })
    }

    /// 立即汇总一轮（手动触发/测试用；常驻线程每 interval_s 调的就是它）。
    pub fn report_once(&self) {
        self.report();
    }

    fn report(&self) {
        let mut st = lock(&self.state);
        let wall = st.wall_t0.elapsed().as_secs_f64();

        // /proc 只在设备（Linux）上有：PC 联测环境下禁用，只保留埋点窗口
        #[cfg(target_os = "linux")]
        let Some((head, cpu_pct, wifi)) = Self::sample_linux(&mut st, wall) else {
            return; // /proc 读失败：跳过本轮（基线保留，下一窗口并入）
        };
        #[cfg(not(target_os = "linux"))]
        let (head, cpu_pct, wifi): (String, Option<f64>, String) =
            ("CPU n/a（无 /proc）".to_string(), None, String::new());

        st.wall_t0 = Instant::now();
        drop(st);

        let win = self.timing.perf_window(wall);
        // 供 UI 性能面板读：CPU/wifi/收发速率（窗口值，~10s 刷新一次）
        *lock(&self.last) = PerfSnapshot {
            cpu_pct: cpu_pct.map(|v| py_round(v, 0) as i64),
            wifi_dbm: if cpu_pct.is_some() {
                Some(wifi)
            } else {
                None
            },
            tx_kbs: py_round(win.tx_kbs, 0) as i64,
            rx_kbs: py_round(win.rx_kbs, 0) as i64,
            ui_fps: py_round(win.ui_fps, 1),
        };
        let avgs = self.timing.avgs();
        log::info!("[perf] {}", build_perf_line(&head, &win, &avgs));
    }

    /// linux：进程/线程 CPU + wifi 头。None = /proc 读失败（本轮放弃）。
    #[cfg(target_os = "linux")]
    fn sample_linux(
        st: &mut MonitorState,
        wall: f64,
    ) -> Option<(String, Option<f64>, String)> {
        let Some(base) = st.baseline.as_mut() else {
            return Some(("CPU n/a（无 /proc）".to_string(), None, String::new()));
        };
        // 进程 CPU%（单核=100%，两核满载 200%）
        let cur_proc = match procfs::read_proc_ticks() {
            Ok(v) => v,
            Err(e) => {
                log::error!("[perf] 汇总失败: {e}");
                return None;
            }
        };
        let cur_threads = procfs::read_thread_ticks();
        let clk = procfs::clk_tck();
        let cpu_pct = (cur_proc - base.proc_ticks) as f64 / (wall * clk) * 100.0;

        // 线程级 CPU：窗口内增量最大的前 3 个（≥5% 才展示）
        let mut parts: Vec<(f64, String)> = Vec::new();
        for (tid, (name, ticks)) in &cur_threads {
            if let Some((_, prev)) = base.threads.get(tid) {
                let pct = (*ticks - prev) as f64 / (wall * clk) * 100.0;
                if pct >= 5.0 {
                    parts.push((pct, name.clone()));
                }
            }
        }
        base.proc_ticks = cur_proc;
        base.threads = cur_threads;
        parts.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let top: Vec<String> = parts
            .iter()
            .take(3)
            .map(|(p, n)| format!("{n} {p:.0}%"))
            .collect();
        let threads_s = if top.is_empty() {
            "-".to_string()
        } else {
            top.join(",")
        };
        let wifi = procfs::wifi_level();
        Some((
            format!("CPU {cpu_pct:.0}%（{threads_s}） wifi {wifi}"),
            Some(cpu_pct),
            wifi,
        ))
    }
}

/// `avg()` 帮助函数：空窗口显示 "-"（Python 口径）。
fn avg_str(v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{v:.0}"),
        None => "-".to_string(),
    }
}

/// 拼一行 `[perf]` 消息（不含前缀）——格式与 perf.py 逐字一致。
fn build_perf_line(head: &str, win: &PerfWindow, a: &TimingAvgs) -> String {
    let PerfWindow {
        tx_kbs,
        rx_kbs,
        loop_lag_ms,
        ui_fps,
        ui_loop_ms,
        cam_gap_ms,
        play_gap_n,
        play_gap_ms,
    } = *win;
    let underruns = a.underruns;
    let (send, json, uid, uis, kb, je, b6, cc, pl) = (
        avg_str(a.send_ms),
        avg_str(a.json_ms),
        avg_str(a.ui_draw_ms),
        avg_str(a.ui_show_ms),
        avg_str(a.jpg_kb),
        avg_str(a.jpeg_ms),
        avg_str(a.b64_ms),
        avg_str(a.cam_copy_ms),
        avg_str(a.play_ms),
    );
    format!(
        "{head} | 上行 {tx_kbs:.0}KB/s 下行 {rx_kbs:.0}KB/s send {send}ms(序 {json}) \
         事件循环滞后 {loop_lag_ms:.0}ms | UI {uid}ms(show {uis}) {ui_fps:.1}fps \
         节拍 {ui_loop_ms:.0}ms | JPEG {kb}KB 编 {je}ms b64 {b6}ms copy {cc}ms \
         帧间隔 {cam_gap_ms:.0}ms | 播放 {pl}ms 块间隙 {play_gap_n}次/\
         最大 {play_gap_ms:.0}ms 欠载 {underruns}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_proc_stat() {
        // pid (comm 可含空格/括号) state ppid ... utime(11) stime(12)
        let line = "1234 (o5 worker) S 1 2 3 4 5 6 7 8 9 10 200 300 0 0 0 0";
        assert_eq!(parse_proc_stat_ticks(line), Some(500));
        let line2 = "42 ((x)y) R 1 2 3 4 5 6 7 8 9 10 5 7";
        assert_eq!(parse_proc_stat_ticks(line2), Some(12));
        assert_eq!(parse_proc_stat_ticks("no paren"), None);
        assert_eq!(parse_proc_stat_ticks("1 (a) S"), None);
    }

    #[test]
    fn wifi_level_parse() {
        let content = "Inter-| sta-| Quality\niface status link signal\n\
                       wlan0: 0000  42.  -47.  -256.  0000  0000\n";
        assert_eq!(wifi_level_dbm(content), "-47dBm");
        // 正值按 dBm*10 换算（15.3 -> 15）
        let pos = "a | b\nx | y\nwlan0: 0000  42.  153.  -256.\n";
        assert_eq!(wifi_level_dbm(pos), "15dBm");
        assert_eq!(wifi_level_dbm("a\nb\neth0: 1 2 3 4\n"), "-");
        assert_eq!(wifi_level_dbm("a\nb\nwlan0: 0000  42.  bad -1\n"), "-");
        assert_eq!(wifi_level_dbm(""), "-");
    }

    #[test]
    fn perf_line_format_empty() {
        let line = build_perf_line(
            "CPU n/a（无 /proc）",
            &PerfWindow::default(),
            &TimingAvgs::default(),
        );
        assert_eq!(
            line,
            "CPU n/a（无 /proc） | 上行 0KB/s 下行 0KB/s send -ms(序 -) 事件循环滞后 0ms | \
             UI -ms(show -) 0.0fps 节拍 0ms | JPEG -KB 编 -ms b64 -ms copy -ms 帧间隔 0ms | \
             播放 -ms 块间隙 0次/最大 0ms 欠载 0"
        );
    }

    #[test]
    fn perf_line_format_values() {
        let win = PerfWindow {
            tx_kbs: 12.6,
            rx_kbs: 3.4,
            loop_lag_ms: 7.0,
            ui_fps: 14.85,
            ui_loop_ms: 66.0,
            cam_gap_ms: 90.0,
            play_gap_n: 2,
            play_gap_ms: 41.0,
        };
        let a = TimingAvgs {
            send_ms: Some(3.0),
            json_ms: Some(1.0),
            ui_draw_ms: Some(12.0),
            ui_show_ms: Some(8.0),
            jpg_kb: Some(6.4),
            jpeg_ms: Some(9.0),
            b64_ms: Some(2.0),
            cam_copy_ms: Some(1.0),
            play_ms: Some(5.0),
            underruns: 1,
        };
        let line = build_perf_line("CPU 37%（o5-perf 12%,mic 9%） wifi -47dBm", &win, &a);
        assert_eq!(
            line,
            "CPU 37%（o5-perf 12%,mic 9%） wifi -47dBm | 上行 13KB/s 下行 3KB/s \
             send 3ms(序 1) 事件循环滞后 7ms | UI 12ms(show 8) 14.8fps 节拍 66ms | \
             JPEG 6KB 编 9ms b64 2ms copy 1ms 帧间隔 90ms | \
             播放 5ms 块间隙 2次/最大 41ms 欠载 1"
        );
        // 逐字对齐 Python perf.py 实跑输出（同输入、PC 无 /proc 头）
        assert_eq!(
            build_perf_line("CPU n/a（无 /proc）", &win, &a),
            "CPU n/a（无 /proc） | 上行 13KB/s 下行 3KB/s send 3ms(序 1) 事件循环滞后 7ms | \
             UI 12ms(show 8) 14.8fps 节拍 66ms | JPEG 6KB 编 9ms b64 2ms copy 1ms 帧间隔 90ms | \
             播放 5ms 块间隙 2次/最大 41ms 欠载 1"
        );
    }

    #[test]
    fn monitor_report_rounds() {
        let timing = Arc::new(Timing::new());
        timing.tx_bytes(2048);
        timing.rx_bytes(1024);
        let m = PerfMonitor::with_interval(timing.clone(), 10.0);
        // 注入 1s 窗口基线（速率按 Timing 自己的 win_t 真实流逝算），可精确断言
        {
            let mut st = lock(&m.state);
            st.wall_t0 = Instant::now() - Duration::from_secs(1);
        }
        timing.window_reset_at(Instant::now() - Duration::from_secs(1));
        m.report_once();
        let s = m.snapshot();
        assert_eq!(s.tx_kbs, 2); // 2048B / ~1s / 1024
        assert_eq!(s.rx_kbs, 1);
        // 无 /proc 的平台 CPU/wifi 为 None；linux 上应为 Some
        #[cfg(not(target_os = "linux"))]
        {
            assert_eq!(s.cpu_pct, None);
            assert_eq!(s.wifi_dbm, None);
        }
        // 第二轮：窗口基线已更新，无新数据 -> 0
        m.report_once();
        assert_eq!(m.snapshot().tx_kbs, 0);
        assert_eq!(m.snapshot().ui_fps, 0.0);
    }

    #[test]
    fn monitor_spawn_smoke() {
        // 常驻线程（daemon 语义）：短周期跑几轮不 panic 即可
        let timing = Arc::new(Timing::new());
        let m = PerfMonitor::with_interval(timing, 0.05);
        let _handle = m.clone().spawn();
        thread::sleep(Duration::from_millis(250));
    }
}

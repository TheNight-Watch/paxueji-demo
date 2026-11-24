//! 退出看门狗（main.py `_fork_exit_watchdog` 的线程版）。
//!
//! Python fork 一个盯梢子进程，父进程 40s 内没退就 SIGTERM（再 10s SIGKILL）；
//! Rust 无需子进程——起一个盯梢线程，主流程 40s 内没走到 [`finish`]（进程
//! 仍未退出）就 `std::process::exit(1)` 强制终结。背景同 Python：清理路径上
//! 有少量无超时的调用（cam feeder kill / aplay 管道写入），极端情况下能把
//! 主线程永久卡住——进程不死，launcher/daemon 就永远等不到退出。
//!
//! 只在真正退出应用的路径上布防（run_ui / run_headless 的收尾，在
//! stop_session 之前）——会话开关 / 模式切换绝不能挂它。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// 看门狗时限（正常退出各 join 合计最坏 ~22.5s，40s 只在真卡死时才触发）。
const WATCHDOG_TIMEOUT_S: f64 = 40.0;

static ARMED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

/// 布防（幂等；每进程一次）。
pub fn arm() {
    if ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("o5-exit-watchdog".to_string())
        .spawn(|| {
            let deadline = Instant::now() + Duration::from_secs_f64(WATCHDOG_TIMEOUT_S);
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(500));
                if DONE.load(Ordering::SeqCst) {
                    return; // 主流程已收尾
                }
            }
            if !DONE.load(Ordering::SeqCst) {
                eprintln!("[watchdog] 退出清理 {WATCHDOG_TIMEOUT_S:.0}s 未完成，强制结束进程");
                std::process::exit(1);
            }
        });
    if let Err(e) = spawned {
        // 线程都起不来：不阻塞正常退出
        eprintln!("[watchdog] 盯梢线程启动失败：{e}");
    }
}

/// 主流程收尾完成，撤销看门狗。
pub fn finish() {
    DONE.store(true, Ordering::SeqCst);
}

//! `--test-pc` 端到端集成测试：拉起 Python 桩服务端（stub_server.py），
//! 再以子进程跑 `o5-harness-rs --test-pc`，断言五项验收口径全过。
//!
//! 环境约定：
//! - `O5_PY_HARNESS_DIR`：Python 版仓库根目录（默认
//!   `D:/code/MaixCAM2_Cpm_O5_Harness`），其下找 `tools/stub_server.py`；
//! - 端口自动分配（空闲临时端口，经 `O5_TEST_WS_PORT` / `O5_TEST_HTTP_PORT`
//!   传给 app）；
//! - 本机无 python / 缺 websockets+aiohttp+numpy / 找不到脚本时**跳过**
//!   （打印原因后直接返回，不算失败——桩环境只装在联测机上）。

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 找一个空闲 TCP 端口。
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("绑定临时端口")
        .local_addr()
        .expect("读取临时端口")
        .port()
}

/// 可用的 Python 解释器（能跑 + 桩依赖齐全）。
fn find_python() -> Option<String> {
    for candidate in ["python", "python3", "py"] {
        let deps_ok = Command::new(candidate)
            .args(["-c", "import websockets, aiohttp, numpy"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if deps_ok {
            return Some(candidate.to_string());
        }
    }
    None
}

/// 等 127.0.0.1:port 可连接（桩监听就绪）。
fn wait_listening(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// 后台读子进程 stdout（app 的日志也走 stdout，不排空会塞满管道）。
fn spawn_reader(mut pipe: std::process::ChildStdout) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut out = String::new();
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    out.push_str(&String::from_utf8_lossy(&buf[..n]));
                    // 只留尾部（防长跑膨胀），发当前快照
                    if tx.send(out.clone()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

#[test]
fn test_pc_full_session_roundtrip() {
    let py_dir = std::env::var("O5_PY_HARNESS_DIR")
        .unwrap_or_else(|_| "D:/code/MaixCAM2_Cpm_O5_Harness".to_string());
    let stub_path = format!("{py_dir}/tools/stub_server.py");
    if !std::path::Path::new(&stub_path).is_file() {
        eprintln!("[test_pc.rs] 跳过：找不到桩脚本 {stub_path}");
        return;
    }
    let Some(python) = find_python() else {
        eprintln!("[test_pc.rs] 跳过：本机无 python 或缺 websockets/aiohttp/numpy");
        return;
    };

    let ws_port = free_port();
    let http_port = free_port();

    // ---- 桩服务端 ----
    let mut stub = Command::new(&python)
        .args([stub_path.as_str(), &ws_port.to_string(), &http_port.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动桩服务端");
    let stub_rx = stub.stdout.take().map(spawn_reader);
    // 桩退出（崩溃）时 reader 断开，无妨——监听探测为准
    if !wait_listening(ws_port, Duration::from_secs(20))
        || !wait_listening(http_port, Duration::from_secs(20))
    {
        let mut tail = String::from("<无输出>");
        if let Some(rx) = stub_rx.as_ref() {
            if let Ok(s) = rx.try_recv() {
                tail = s;
            }
        }
        let _ = stub.kill();
        let _ = stub.wait();
        panic!("桩服务端 20s 内未监听（ws={ws_port} http={http_port}）:\n{tail}");
    }

    // ---- app --test-pc ----
    let mut app = Command::new(env!("CARGO_BIN_EXE_o5-harness-rs"))
        .arg("--test-pc")
        .env("O5_TEST_WS_PORT", ws_port.to_string())
        .env("O5_TEST_HTTP_PORT", http_port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 o5-harness-rs --test-pc");
    let app_rx = app.stdout.take().map(spawn_reader);

    // 等退出（会话 ~10s + 启动；宽限 120s 防慢机误杀）
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        match app.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = stub.kill();
                panic!("等待 app 退出失败: {e}");
            }
        }
        if Instant::now() > deadline {
            let _ = app.kill();
            let _ = app.wait();
            let _ = stub.kill();
            let _ = stub.wait();
            panic!("--test-pc 120s 未退出");
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let _ = stub.kill();
    let _ = stub.wait();

    // 汇总 app 输出（reader 线程在管道关闭后终止，最后一条即全量）
    let mut output = String::new();
    if let Some(rx) = app_rx {
        while let Ok(chunk) = rx.try_recv() {
            output = chunk;
        }
    }

    assert!(
        status.success(),
        "--test-pc 退出码 {:?}（期望 0）\n--- 输出尾部 ---\n{}",
        status.code(),
        output
    );
    assert!(
        output.contains("[test-pc] PASS 全部 5 项通过"),
        "五项口径未全部通过\n--- 输出 ---\n{output}"
    );
}

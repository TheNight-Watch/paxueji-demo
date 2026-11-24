//! PC 联测模式（`--test-pc`）：无真机时跑通「麦克风 -> 会话 -> 工具/委派 ->
//! 口播 -> 优雅关闭」全链路。
//!
//! 环境约定（对齐 Python `tools/stub_server.py` 联测）：
//! - `O5_TEST_WS_PORT`（默认 8765）：桩服务端 WebSocket 端口；
//! - `O5_TEST_HTTP_PORT`（默认 8791）：桩后台大脑 HTTP 端口；
//! - 需要先启动 `python tools/stub_server.py <ws_port> <http_port>`。
//!
//! 音频走 `platform_capture()` 的 PC mock（静音时钟，1 unit/s），相机注入
//! `MockFrameSource`（160x120 @2fps），扬声器走空 sink——端到端时序与真机
//! 相同，只是数据是合成的。跑一个完整会话后按五项口径断言：
//! 1. 状态机 connecting -> ready -> closed；
//! 2. 口播 >=2 块、采样率 24k；
//! 3. delegate_task 往返（工具调用已发出且口播含后台结果 "1081"）；
//! 4. 优雅关闭（收到 session.closed，清理有界完成）；
//! 5. 时序统计产出（units/音频块/ACK RTT）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use o5_camera::CameraFeed;
use o5_config::Config;
use o5_session::SessionState;
use serde_json::json;

use crate::app::{App, CameraFactory};

fn env_port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// 联测配置：本地桩端点 + omni 视频 + 30s 超时（PC 上留足余量）。
fn stub_config(ws_port: u16, http_port: u16) -> Config {
    let mut cfg = Config::defaults();
    // 前台：桩 WebSocket
    cfg.fg_endpoint = format!("ws://127.0.0.1:{ws_port}");
    cfg.fg_api_key = "stub-key".into();
    cfg.fg_protocol_version = 4;
    cfg.fg_mode = "omni".into();
    cfg.fg_queue_gate = true;
    cfg.fg_progress_timeout_s = 30.0;
    cfg.fg_ack_timeout_s = 30.0;
    cfg.fg_connect_timeout_s = 10.0;
    cfg.fg_voice = String::new();
    // 过 generate_audio 音色门槛（桩不校验内容）
    cfg.fg_init_payload = json!({
        "tts_prompt_audio": {"source": "base64", "format": "wav", "data": ""}
    });
    // 后台：桩大脑（…/v1 由 delegation 补 /chat/completions）
    cfg.bg_enabled = true;
    cfg.bg_endpoint = format!("http://127.0.0.1:{http_port}/v1");
    cfg.bg_api_key = "stub-key".into();
    cfg.bg_model = "stub-brain".into();
    cfg.bg_timeout_s = 30.0;
    // PC：关 AEC/闪避，不起转写
    cfg.asr_enabled = false;
    cfg.audio_echo_cancel = false;
    cfg.audio_mic_duck_while_playing = 0.0;
    // 相机：合成帧（omni 强校验每 unit 恰好 1 帧）
    cfg.video_enabled = true;
    cfg.video_width = 160;
    cfg.video_height = 120;
    cfg.video_fps = 2;
    cfg.video_jpeg_quality = 60;
    cfg
}

/// 跑一个完整会话并按五项口径断言。桩服务端需已监听（tests/test_pc.rs
/// 负责拉起；手动联测：`python tools/stub_server.py 8765 8791`）。
pub fn run(log_file_override: Option<&str>) -> i32 {
    let ws_port = env_port("O5_TEST_WS_PORT", 8765);
    let http_port = env_port("O5_TEST_HTTP_PORT", 8791);
    let mut cfg = stub_config(ws_port, http_port);
    // 日志落临时目录（不污染 exe 目录），--log-file 可覆盖
    let log_path = match log_file_override.filter(|p| !p.is_empty()) {
        Some(p) => p.to_string(),
        None => std::env::temp_dir()
            .join(format!("o5_test_pc_{}.log", std::process::id()))
            .to_string_lossy()
            .into_owned(),
    };
    cfg.log_file = log_path.clone();
    let _ = crate::logging::setup(&cfg, None);
    log::info!(
        target: "o5.main",
        "[test-pc] 桩端点 ws://127.0.0.1:{ws_port} + http://127.0.0.1:{http_port}，日志 {log_path}"
    );

    let camera_factory: CameraFactory = Arc::new(|_cfg, timing| {
        let clock: Arc<dyn o5_camera::clock::Clock> = Arc::new(o5_camera::clock::SystemClock);
        let src = o5_camera::source::MockFrameSource::new(160, 120, 2, Arc::clone(&clock));
        Some(CameraFeed::with_source(
            Box::new(src),
            (640, 480),
            timing,
            clock,
        ))
    });
    let app = App::with_camera_factory(cfg, true, None, Some(camera_factory));

    let t0 = Instant::now();
    app.start_session();
    // 跑到第二段口播（桩在 unit 4 转述后台结果、unit 5 问候 + turn_eos）
    while t0.elapsed() < Duration::from_secs(60) {
        app.poll_tick();
        if app.spoken_finals() >= 2 {
            break;
        }
        if matches!(app.last_session_state(), SessionState::Failed | SessionState::Closed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // 优雅关闭：stop_session 后台清理有界收尾
    let close_start = Instant::now();
    app.stop_session();
    let closed = app.wait_stop_complete(Duration::from_secs(12));
    let close_ms = close_start.elapsed().as_millis() as u64;

    // ---- 五项口径 ----
    let mut fail = 0;

    // 1. 状态机：connecting 起步，到过 ready，最终 closed
    let states = app.state_history();
    let first_ok = states.first() == Some(&SessionState::Connecting);
    let ready_ok = states.contains(&SessionState::Ready);
    let last = app.last_session_state();
    let closed_state_ok = last == SessionState::Closed;
    report(
        first_ok && ready_ok && closed_state_ok,
        &format!(
            "状态机: {:?} 起步, ready={}, 终态={:?}",
            states.first(),
            ready_ok,
            last
        ),
        &mut fail,
    );

    // 2. 口播 >=2 块 24k
    let (chunks, rate) = app.audio_stats();
    let finals = app.spoken_finals();
    report(
        chunks >= 2 && rate == 24000 && finals >= 2,
        &format!("口播: {finals} 个回合, {chunks} 块 @ {rate}Hz"),
        &mut fail,
    );

    // 3. delegate_task 往返：调用已发出 + 口播含后台结果
    let calls = app.tool_calls();
    let delegated = calls.iter().any(|c| c.starts_with("delegate_task:"));
    let spoken = app.spoken_text();
    let roundtrip = delegated && spoken.contains("1081");
    report(
        roundtrip,
        &format!(
            "delegate_task 往返: 调用 {}, 口播含 1081={}",
            delegated,
            spoken.contains("1081")
        ),
        &mut fail,
    );

    // 4. 优雅关闭：收到 closed 且清理有界（<=10s）
    report(
        closed && closed_state_ok && close_ms < 10_000,
        &format!("优雅关闭: closed={closed}, 耗时 {close_ms}ms"),
        &mut fail,
    );

    // 5. 时序统计产出
    let snap = app.timing().snapshot();
    let stats_ok = snap.units_sent >= 4 && snap.audio_chunks >= 2 && snap.ack_rtt.is_some();
    report(
        stats_ok,
        &format!(
            "时序统计: units={}, 音频块={}, ack_rtt={:?}",
            snap.units_sent, snap.audio_chunks, snap.ack_rtt
        ),
        &mut fail,
    );

    log::info!(target: "o5.main", "[test-pc] 会话统计: {calls:?}");
    if fail == 0 {
        println!("[test-pc] PASS 全部 5 项通过（{:.1}s）", t0.elapsed().as_secs_f64());
        0
    } else {
        println!("[test-pc] FAIL {fail}/5 项未通过");
        1
    }
}

fn report(ok: bool, detail: &str, fail: &mut i32) {
    if ok {
        println!("[test-pc] PASS {detail}");
    } else {
        println!("[test-pc] FAIL {detail}");
        *fail += 1;
    }
}

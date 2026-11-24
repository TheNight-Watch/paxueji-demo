//! 设备硬件自检（`--selftest`；Python `tools/selftest_device.py` 的移植）。
//!
//! 不依赖云端，验证 harness 用到的全部设备侧能力：
//!  1. 配置加载（对应 Python 的模块导入检查——Rust 单二进制，模块即配置）；
//!  2. 麦克风：3 秒采集，统计字节数与 RMS 电平（对着说话应 > -35dB）；
//!  3. 扬声器：24k 正弦两声（失败自动回退 48k + 重采样）——现场应能听到；
//!  4. 摄像头：3 帧 JPEG 编码，输出尺寸分布；
//!  5. 屏幕 + 触摸：绘制测试画面 5 秒，点击屏幕应打印坐标。
//!
//! 汇总 `结果: X/Y 通过`，退出码 0/1。

use std::time::{Duration, Instant};

use o5_audio::device::{platform_capture, platform_sink_factory};
use o5_config::Config;
use o5_protocol::{resample_pcm16, rms_db};

/// 自检结果累积（Python 模块级 `RESULTS` 列表）。
struct Results {
    total: usize,
    failed: Vec<String>,
}

fn report(res: &mut Results, name: &str, ok: bool, detail: &str) {
    res.total += 1;
    if !ok {
        res.failed.push(name.to_string());
    }
    let tag = if ok { "PASS" } else { "FAIL" };
    if detail.is_empty() {
        println!("[{tag}] {name}");
    } else {
        println!("[{tag}] {name} — {detail}");
    }
}

/// 自检入口。返回进程退出码（0=全过 / 1=有失败项）。
pub fn run() -> i32 {
    println!("{}", "=".repeat(56));
    println!("MaixCAM2 O5 Harness 硬件自检");
    println!("{}", "=".repeat(56));
    let mut res = Results { total: 0, failed: Vec::new() };

    test_modules(&mut res);
    // 麦克风电平口径 = 运行时同款（配置音量的软增益之后），说话阈值才有意义
    let input_volume =
        Config::load(None).map(|c| c.audio_input_volume).unwrap_or(100);
    test_mic(&mut res, input_volume);
    test_speaker(&mut res);
    test_camera(&mut res);
    test_display_touch(&mut res);

    println!("{}", "-".repeat(56));
    let passed = res.total - res.failed.len();
    println!("结果: {passed}/{} 通过", res.total);
    if !res.failed.is_empty() {
        println!("失败项: {}", res.failed.join(", "));
    }
    if res.failed.is_empty() {
        0
    } else {
        1
    }
}

/// 配置加载（Python 的 maix/websockets/numpy/o5 模块导入检查——Rust 单
/// 二进制不存在导入失败，等价物是配置能否加载出可用端点）。
fn test_modules(res: &mut Results) {
    report(
        res,
        "o5-harness-rs",
        true,
        &format!("v{} 全部 crate 已链接", env!("CARGO_PKG_VERSION")),
    );
    match Config::load(None) {
        Ok(cfg) => report(
            res,
            "配置加载",
            !cfg.fg_endpoint.is_empty(),
            &format!("端点 {} 模式 {}", cfg.fg_endpoint, cfg.fg_mode),
        ),
        Err(e) => report(res, "配置加载", false, &format!("{e}")),
    }
}

/// 麦克风：3 秒采集（16k mono S16_LE，期望 96000 字节，容忍 70%）。
fn test_mic(res: &mut Results, input_volume: u8) {
    let mut cap = platform_capture(input_volume);
    let mut total: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match cap.read_pcm16(&mut buf) {
            Ok(0) | Err(_) => std::thread::sleep(Duration::from_millis(20)),
            Ok(n) => total.extend_from_slice(&buf[..n]),
        }
    }
    let expect = 16000usize * 2 * 3;
    let ok = total.len() as f64 > expect as f64 * 0.7;
    let db = if total.is_empty() {
        -90.0
    } else {
        f64::from(rms_db(&total[..total.len().min(32000)]))
    };
    report(
        res,
        "麦克风采集",
        ok,
        &format!(
            "3s 采到 {:.1}s 数据，电平 {db:.1} dBFS（说话应 > -35）",
            total.len() as f64 / 32000.0
        ),
    );
}

/// 扬声器：880Hz 0.6s 正弦两声；24k 失败回退 48k + 重采样。
fn test_speaker(res: &mut Results) {
    let rate: u32 = 24000;
    let n = (f64::from(rate) * 0.6) as usize;
    let mut pcm: Vec<u8> = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f64 / f64::from(rate);
        let v = 0.4 * (2.0 * std::f64::consts::PI * 880.0 * t).sin();
        pcm.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes());
    }
    let mut played = false;
    let mut note = String::new();
    let mut factory = platform_sink_factory();
    for target in [24000u32, 48000u32] {
        let data = if target == 24000 {
            pcm.clone()
        } else {
            resample_pcm16(&pcm, 24000, target)
        };
        match factory(target) {
            Ok(mut sink) => {
                for _ in 0..2 {
                    // 响两声（NullPlaybackSink 按实时速率睡，节奏即播放时长）
                    let _ = sink.write_pcm16(&data);
                    std::thread::sleep(Duration::from_millis(700));
                }
                played = true;
                note = format!("{target} Hz 正常（应听到两声 880Hz 提示音）");
                break;
            }
            Err(e) => {
                note = format!("{target} Hz 失败: {e}");
            }
        }
    }
    report(res, "扬声器播放", played, &note);
}

/// 摄像头：feeder 拉流 + 3 帧 JPEG（设备 640x480，quality 60）。
fn test_camera(res: &mut Results) {
    #[cfg(target_os = "linux")]
    {
        let cfg = Config::load(None).unwrap_or_else(|_| Config::defaults());
        let timing = std::sync::Arc::new(o5_perf::Timing::new());
        let mut feed = o5_camera::CameraFeed::from_config(&cfg, timing);
        // 等至少 3 帧进来（feeder 启动 ~1s）
        let deadline = Instant::now() + Duration::from_secs(5);
        while feed.frames_read() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut sizes: Vec<usize> = Vec::new();
        for _ in 0..3 {
            if let Some(j) = feed.latest_jpeg_b64(60) {
                // base64 长度换回二进制字节数（Python 对 jpg.to_bytes() 计数）
                sizes.push(j.len() * 3 / 4);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        let ok = sizes.len() == 3 && sizes.iter().all(|&s| s > 2000);
        report(res, "摄像头 + JPEG", ok, &format!("3 帧 JPEG 尺寸 {sizes:?}"));
        feed.stop();
    }
    #[cfg(not(target_os = "linux"))]
    {
        report(res, "摄像头 + JPEG", false, "非 linux 平台无设备摄像头");
    }
}

/// 屏幕 + 触摸：绘制测试画面 5 秒（进度条），计点击次数并打印坐标。
fn test_display_touch(res: &mut Results) {
    #[cfg(target_os = "linux")]
    {
        use o5_ui::DisplaySink as _;
        let cfg = Config::load(None).unwrap_or_else(|_| Config::defaults());
        let (sw, sh) = (
            o5_ui::layout::SCREEN_W as u32,
            o5_ui::layout::SCREEN_H as u32,
        );
        match o5_ui::FeederSink::spawn(&cfg.display_feeder_path(), sw, sh)
        {
            Ok(sink) => {
                let mut ts = o5_ui::touch::open_default(&cfg);
                let mut touches = 0usize;
                let mut pressed_prev = false;
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    let left = (deadline - Instant::now()).as_secs_f64();
                    // 深蓝底 + 亮绿进度条（随剩余时间缩短）
                    let mut img = o5_ui::Image::new(sw, sh);
                    img.fill(o5_ui::Color(20, 40, 90));
                    img.fill_rect(
                        20,
                        200,
                        (600.0 * (1.0 - left / 5.0)) as i32,
                        24,
                        o5_ui::Color(72, 199, 118),
                    );
                    sink.push_frame(&img.data);
                    let s = ts.poll();
                    if s.pressed && !pressed_prev {
                        touches += 1;
                        println!("    触摸: ({}, {})", s.x, s.y);
                    }
                    pressed_prev = s.pressed;
                    std::thread::sleep(Duration::from_millis(20));
                }
                report(
                    res,
                    "屏幕显示",
                    true,
                    &format!("{sw}x{sh} feeder 输出正常"),
                );
                report(
                    res,
                    "触摸屏",
                    touches > 0,
                    &format!(
                        "{}",
                        if touches == 0 {
                            "5 秒内 0 次点击（现场没点屏幕，不算失败可复测）"
                        } else {
                            "捕获点击"
                        }
                    ),
                );
            }
            Err(e) => {
                report(
                    res,
                    "屏幕显示 + 触摸",
                    false,
                    &format!("无法打开（launcher 占用属正常）: {e}"),
                );
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        report(res, "屏幕显示 + 触摸", false, "非 linux 平台无设备屏幕/触摸");
    }
}

//! `AudioIo` 集成测试：mock 采集/播放通道上验证 1s unit 凑包、静音、
//! 门控/闪避、far 保活、播放速率回退与停止/重启语义。
//!
//! mock 采集队列一次吐一块（25ms~200ms 级），采集循环按 25ms 节流消费，
//! 所以每个测试的墙钟时间与音频时长同量级（~0.3-1.2s）。

use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use o5_audio::device::{MockCaptureSource, PlaybackSink};
use o5_audio::{AudioIo, INPUT_RATE, UNIT_BYTES};
use o5_config::Config;
use o5_perf::Timing;

// ---------------------------------------------------------------------------

fn i16s_to_bytes(v: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 2);
    for &s in v {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn bytes_to_i16s(v: &[u8]) -> Vec<i16> {
    v.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// 已写块记录：(设备速率, 数据)。
type RecLog = Arc<Mutex<Vec<(u32, Vec<u8>)>>>;

/// 立即返回的假播放通道：记录 (rate, data)。
struct RecSink {
    rate: u32,
    log: RecLog,
}

impl PlaybackSink for RecSink {
    fn write_pcm16(&mut self, pcm: &[u8]) -> io::Result<()> {
        self.log.lock().unwrap().push((self.rate, pcm.to_vec()));
        Ok(())
    }
}

/// 注入用 sink 工厂：全部成功、写入即时返回。
fn rec_factory() -> (RecLog, o5_audio::SinkFactory) {
    let log: RecLog = Arc::new(Mutex::new(Vec::new()));
    let l2 = Arc::clone(&log);
    (
        log,
        Box::new(move |rate| {
            let l = Arc::clone(&l2);
            Ok(Box::new(RecSink { rate, log: l }) as Box<dyn PlaybackSink>)
        }),
    )
}

fn cfg_off() -> Config {
    let mut c = Config::default();
    c.audio_echo_mode = "off".to_string();
    // 关 near AGC：这些测试用高幅信号验证凑包/门控/静音语义，不测 AGC
    c.audio_near_agc = false;
    c
}

fn wait_until(cond: impl Fn() -> bool, timeout_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while !cond() {
        assert!(
            Instant::now() < deadline,
            "条件等待超时（{timeout_ms}ms）"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 推 `seconds` 秒常幅 PCM（`samples_per_chunk` 一块），立即全部入队。
fn push_mic(h: &o5_audio::device::MockCaptureHandle, amp: i16, seconds: f64) {
    let total = (seconds * f64::from(INPUT_RATE)).round() as usize;
    let chunk = 1600; // 200ms 一块
    let mut left = total;
    while left > 0 {
        let n = left.min(chunk);
        h.push(&i16s_to_bytes(&vec![amp; n]));
        left -= n;
    }
}

// ---------------------------------------------------------------------------

/// 凑包/直通/静音/停止：1.05s 数据 -> 恰一个 32000B unit；静音后全零；
/// 停止后不再产出。
#[test]
fn mic_unit_mute_and_stop() {
    let (cap, h) = MockCaptureSource::new();
    let mut io = AudioIo::new(&cfg_off(), Arc::new(Timing::new()), Box::new(cap));
    assert!(io.aec_stats().is_none());
    assert_eq!(io.aec_mode(), "off");
    let (tx, rx) = mpsc::channel();
    io.start_mic(Box::new(move |unit, t_recorded| {
        tx.send((unit, t_recorded)).unwrap();
    }));
    assert!(io.mic_healthy());

    // 1.05s：凑满恰好一个 unit，余 0.05s 丢弃（幅度 8192 -> -12dBFS）
    push_mic(&h, 8192, 1.05);
    let (unit, t) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("应凑满一个 unit");
    assert_eq!(unit.len(), UNIT_BYTES);
    assert_eq!(bytes_to_i16s(&unit[..64]), vec![8192i16; 32]);
    assert!(t.elapsed() < Duration::from_secs(2), "t_recorded 应刚产生");
    assert!(io.level().db() > -20.0, "电平应反映输入幅度");
    assert!((io.level().rms() - 8192.0 / 32768.0).abs() < 1e-3);

    // 静音 = 全零片段照发（音频时钟不停）
    io.set_muted(true);
    assert!(io.muted());
    push_mic(&h, 1000, 1.0);
    let (unit, _) = rx.recv_timeout(Duration::from_secs(5)).expect("静音也要发 unit");
    assert_eq!(unit.len(), UNIT_BYTES);
    assert!(bytes_to_i16s(&unit).iter().all(|&s| s == 0), "静音应全零");

    // 停止：余量丢弃，不再产出
    io.set_muted(false);
    push_mic(&h, 1000, 0.5);
    io.stop_mic();
    assert!(
        rx.recv_timeout(Duration::from_millis(400)).is_err(),
        "停止后不应再产 unit"
    );
    assert!(!io.mic_healthy());
}

/// 播放期间硬门控：unit1 全零（播放 + 尾沿窗口内），窗口过后直通。
#[test]
fn gate_zeros_while_playing() {
    let mut cfg = cfg_off();
    cfg.audio_mic_gate_while_playing = true;
    cfg.audio_mic_duck_while_playing = 1.0; // 隔离门控（默认 0.4 会叠加闪避）
    let (cap, h) = MockCaptureSource::new();
    let mut io = AudioIo::new(&cfg, Arc::new(Timing::new()), Box::new(cap));
    let (_log, f) = rec_factory();
    io.set_playback_sink_factory(f);
    io.start();
    let (tx, rx) = mpsc::channel();
    io.start_mic(Box::new(move |unit, _| tx.send(unit).unwrap()));

    // 0.5s 播放块：门控窗口 = play_until(≈+0.5s) + 尾沿 0.2s
    io.put_speaker(i16s_to_bytes(&vec![3000i16; 8000]), INPUT_RATE);
    wait_until(|| io.playing(0.0), 1000);
    // 窗口内推 1s：unit 全零
    push_mic(&h, 1000, 1.0);
    let unit = rx.recv_timeout(Duration::from_secs(5)).expect("unit1");
    assert!(bytes_to_i16s(&unit).iter().all(|&s| s == 0), "门控期应全零");

    // 窗口过后（>0.7s）：直通
    wait_until(|| !io.playing(0.25), 2000);
    push_mic(&h, 1000, 1.0);
    let unit = rx.recv_timeout(Duration::from_secs(5)).expect("unit2");
    assert!(bytes_to_i16s(&unit).iter().all(|&s| s == 1000), "窗口后应直通");

    io.stop_mic();
    io.stop_speaker();
}

/// 播放期间闪避（duck=0.4，尾沿 2s）：unit 全部压到 0.4 倍。
#[test]
fn duck_scales_while_playing() {
    let mut cfg = cfg_off();
    cfg.audio_mic_duck_while_playing = 0.4;
    let (cap, h) = MockCaptureSource::new();
    let mut io = AudioIo::new(&cfg, Arc::new(Timing::new()), Box::new(cap));
    let (_log, f) = rec_factory();
    io.set_playback_sink_factory(f);
    io.start();
    let (tx, rx) = mpsc::channel();
    io.start_mic(Box::new(move |unit, _| tx.send(unit).unwrap()));

    io.put_speaker(i16s_to_bytes(&vec![3000i16; 8000]), INPUT_RATE);
    wait_until(|| io.playing(0.0), 1000);
    push_mic(&h, 1000, 1.0);
    let unit = rx.recv_timeout(Duration::from_secs(5)).expect("unit");
    assert!(
        bytes_to_i16s(&unit).iter().all(|&s| s == 400),
        "duck 0.4 后应全为 400（1000*0.4）"
    );

    io.stop_mic();
    io.stop_speaker();
}

/// far 保活：speaker 空闲时按墙钟给 AEC 推静音（far_s 前进）；
/// 停麦后冻结。
#[test]
fn far_keepalive_advances_and_freezes() {
    let mut cfg = Config::default();
    cfg.audio_echo_mode = "nlms".to_string();
    let (cap, _h) = MockCaptureSource::new();
    let mut io = AudioIo::new(&cfg, Arc::new(Timing::new()), Box::new(cap));
    assert_eq!(io.aec_mode(), "nlms");
    let (_log, f) = rec_factory();
    io.set_playback_sink_factory(f);
    io.start();
    io.start_mic(Box::new(|_, _| {}));
    let far_s = |v: &serde_json::Value| v["far_s"].as_f64().unwrap_or(0.0);

    // 空闲 0.6s：keepalive 按 0.04s 粒度推静音
    let base = far_s(&io.aec_stats().unwrap());
    std::thread::sleep(Duration::from_millis(600));
    let after = far_s(&io.aec_stats().unwrap());
    assert!(
        after - base >= 0.15,
        "空闲期 far_s 应推进（base={base:.3} after={after:.3}）"
    );

    // 停麦：far 时钟同步冻结
    io.stop_mic();
    let frozen = far_s(&io.aec_stats().unwrap());
    std::thread::sleep(Duration::from_millis(300));
    let still = far_s(&io.aec_stats().unwrap());
    assert!(still - frozen < 0.01, "停麦后 far_s 不应推进（frozen={frozen:.3} still={still:.3}）");

    io.stop_speaker();
}

/// 播放实际写出的流喂给 AEC far（写出成功才喂）。
#[test]
fn played_stream_feeds_far() {
    let mut cfg = Config::default();
    cfg.audio_echo_mode = "nlms".to_string();
    let (cap, _h) = MockCaptureSource::new();
    let mut io = AudioIo::new(&cfg, Arc::new(Timing::new()), Box::new(cap));
    let (_log, f) = rec_factory();
    io.set_playback_sink_factory(f);
    io.start();
    io.start_mic(Box::new(|_, _| {}));
    // 24000Hz：写通道按 24000 建，far 重采样回 16k
    io.put_speaker(i16s_to_bytes(&vec![5000i16; 2400]), 24000); // 0.1s
    wait_until(
        || io.aec_stats().unwrap()["far_s"].as_f64().unwrap_or(0.0) > 0.05,
        2000,
    );
    let near_s = io.aec_stats().unwrap()["near_s"].as_f64().unwrap_or(0.0);
    assert!(near_s < 0.01, "未喂近端数据时 near_s 应为 0");
    io.stop_mic();
    io.stop_speaker();
}

/// 未支持速率回退 48000 + 线性重采样；音量增益在写出流上生效。
#[test]
fn speaker_rate_fallback_and_volume_gain() {
    let (cap, _h) = MockCaptureSource::new();
    let mut io = AudioIo::new(&cfg_off(), Arc::new(Timing::new()), Box::new(cap));
    let (log, f) = rec_factory();
    io.set_playback_sink_factory(f);
    io.start();
    io.set_volume(100);

    // 8000Hz 不在 {16k,24k,48k}：走 48000 + resample
    io.put_speaker(i16s_to_bytes(&vec![1000i16; 800]), 8000); // 0.1s
    wait_until(|| !log.lock().unwrap().is_empty(), 1500);
    let writes = log.lock().unwrap().clone();
    assert!(!writes.is_empty(), "应有写出");
    let (rate, data) = &writes[0];
    assert_eq!(*rate, 48000, "未支持速率回退 48000");
    // 800 样本 @8k -> 4800 样本 @48k；首块头部淡入（起播 click 消除）
    assert_eq!(data.len(), 4800 * 2, "重采样后样本数");
    let v48 = bytes_to_i16s(data);
    assert_eq!(v48[500], 1000, "淡入区外样本 = 原样（增益 1.0）");
    assert!(v48[0] < 100, "首样本被淡入（got={}）", v48[0]);

    // 音量 50：增益 (0.5)^1.6 ≈ 0.33，16k 直通无重采样
    let (log2, f2) = rec_factory();
    io.set_playback_sink_factory(f2);
    io.set_volume(50);
    io.put_speaker(i16s_to_bytes(&vec![10000i16; 1600]), INPUT_RATE); // 0.1s
    wait_until(|| !log2.lock().unwrap().is_empty(), 1500);
    let v = bytes_to_i16s(&log2.lock().unwrap()[0].1);
    let expect = (10000.0f32 * (0.5f64.powf(1.6) as f32)).round_ties_even();
    // 速率切换重建通道，首块同样淡入：查淡入区外样本
    assert!((v[100] as f32 - expect).abs() <= 1.0, "音量增益（got={}）", v[100]);
    assert!((v[0] as f32).abs() < expect / 10.0, "首样本淡入（got={}）", v[0]);

    io.stop_speaker();
}

/// 停止清队/播窗复位；停止后 put 丢弃；可重启。
#[test]
fn speaker_stop_clears_and_restarts() {
    let (cap, _h) = MockCaptureSource::new();
    let mut io = AudioIo::new(&cfg_off(), Arc::new(Timing::new()), Box::new(cap));
    let (log, f) = rec_factory();
    io.set_playback_sink_factory(f);
    io.start();

    io.put_speaker(i16s_to_bytes(&vec![1000i16; 3200]), INPUT_RATE); // 0.2s
    wait_until(|| io.playing(0.0), 1000);
    io.stop_speaker();
    assert!(!io.playing(5.0), "停止后 playing 应为 false");
    let n_after_stop = log.lock().unwrap().len();
    assert!(n_after_stop <= 1, "清队后不应继续写");

    // 停止后 put 直接丢弃
    io.put_speaker(i16s_to_bytes(&vec![1000i16; 1600]), INPUT_RATE);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        log.lock().unwrap().len(),
        n_after_stop,
        "停止后的 put 应被丢弃"
    );

    // 重启：新线程 + 新队列，put 恢复工作
    io.start();
    let (log2, f2) = rec_factory();
    io.set_playback_sink_factory(f2);
    io.put_speaker(i16s_to_bytes(&vec![1000i16; 1600]), INPUT_RATE);
    wait_until(
        || io.playing(0.0) && !log2.lock().unwrap().is_empty(),
        1500,
    );
    io.stop_speaker();
    assert!(!io.playing(5.0));
}

//! 实时性埋点：四个关键时刻 + 滚动统计。（`o5/timing.py` 的忠实移植）
//!
//! 对应测试报告 TODO 的四个时间点：
//! 1. unit_recorded  录到每秒音频（录音线程凑满 1s）
//! 2. unit_sent      发出 unit（WebSocket send 完成）
//! 3. audio_received 收到第一段模型音频（response.spoken.delta.audio，按“回合”口径）
//! 4. playback_started 扬声器开始播放（播放线程突发首块）
//!
//! 派生指标：采集->发送滞后、发送->committed ACK RTT、收音->出声滞后、会话建立耗时。
//!
//! [`Timing`] 内部是 `Arc<Mutex<..>>`，Clone 即跨线程共享（GIL 保护下小操作的近似）。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::util::{lock, py_f64_str, py_round};

/// 滚动窗口（Python `Rolling`）：cap 容量、`last`/`mean` 查询。
/// `push(&self, ..)` 内部加锁，可直接跨线程共享。
#[derive(Debug)]
pub struct Rolling {
    cap: usize,
    values: Mutex<Vec<f64>>,
}

impl Rolling {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            values: Mutex::new(Vec::new()),
        }
    }

    /// 追加一个值；超过 cap 淘汰最老的。
    pub fn push(&self, v: f64) {
        let mut values = lock(&self.values);
        values.push(v);
        let len = values.len();
        if len > self.cap {
            values.drain(..len - self.cap);
        }
    }

    /// 最近一个值；空窗口为 `None`。
    pub fn last(&self) -> Option<f64> {
        lock(&self.values).last().copied()
    }

    /// 窗口均值；空窗口为 0.0（判空请用 `last().is_none()`）。
    pub fn mean(&self) -> f64 {
        let values = lock(&self.values);
        if values.is_empty() {
            return 0.0;
        }
        values.iter().sum::<f64>() / values.len() as f64
    }
}

/// 快照里的一条毫秒级指标（Python snapshot() 的 `{"last_ms": .., "avg_ms": ..}`）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub last_ms: f64,
    pub avg_ms: f64,
}

/// `perf_window()` 读走的一个统计窗口：速率 + 窗口内最大值（读后清零）。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct PerfWindow {
    pub tx_kbs: f64,
    pub rx_kbs: f64,
    pub loop_lag_ms: f64,
    pub ui_fps: f64,
    pub ui_loop_ms: f64,
    pub cam_gap_ms: f64,
    pub play_gap_n: u64,
    pub play_gap_ms: f64,
}

/// 快照（给 UI 与日志；字段与 timing.py `snapshot()` 一致）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimingSnapshot {
    pub units_sent: u64,
    pub units_dropped: u64,
    pub turns: u64,
    pub audio_chunks: u64,
    pub capture_to_send: Option<Metric>,
    pub ack_rtt: Option<Metric>,
    pub recv_to_play: Option<Metric>,
    pub net_rtt: Option<Metric>,
    pub response: Option<Metric>,
    pub session_setup_s: Option<f64>,
}

/// `[perf]` 行用的各窗口均值（空窗口为 `None` -> 显示 "-"）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TimingAvgs {
    pub send_ms: Option<f64>,
    pub json_ms: Option<f64>,
    pub ui_draw_ms: Option<f64>,
    pub ui_show_ms: Option<f64>,
    pub jpg_kb: Option<f64>,
    pub jpeg_ms: Option<f64>,
    pub b64_ms: Option<f64>,
    pub cam_copy_ms: Option<f64>,
    pub play_ms: Option<f64>,
    pub underruns: u64,
}

/// 非线程安全的滚动窗口（Timing 内部；锁在 Timing 一侧，一次加锁动全部字段）。
#[derive(Debug)]
struct Roll {
    cap: usize,
    values: Vec<f64>,
}

impl Roll {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            values: Vec::new(),
        }
    }

    fn add(&mut self, v: f64) {
        self.values.push(v);
        let len = self.values.len();
        if len > self.cap {
            self.values.drain(..len - self.cap);
        }
    }

    fn last(&self) -> Option<f64> {
        self.values.last().copied()
    }

    fn mean(&self) -> Option<f64> {
        if self.values.is_empty() {
            None
        } else {
            Some(self.values.iter().sum::<f64>() / self.values.len() as f64)
        }
    }

    /// Python snapshot() 的 ms() 帮助函数：None（空）/ {last_ms, avg_ms}（各取 1 位小数）。
    fn ms_metric(&self) -> Option<Metric> {
        let last = self.last()?;
        Some(Metric {
            last_ms: py_round(last, 1),
            avg_ms: py_round(self.mean()?, 1),
        })
    }
}

#[derive(Debug)]
struct TimingState {
    // —— 派生区间（timing.py 口径）——
    capture_to_send: Roll, // ms：凑满 1s -> send 完成
    send_to_ack: Roll,     // ms：send -> response.unit.committed
    recv_to_play: Roll,    // ms：首个音频帧到达 -> 扬声器出声
    session_setup: Roll,   // s：connect 开始 -> session.created
    net_rtt: Roll,         // ms：TCP 握手 RTT（纯网络，NetRttProbe 喂）
    response: Roll,        // ms：末单元 ACK -> 回合首音频（服务端决策+推理+下行）
    units_sent: u64,
    units_dropped: u64,
    audio_chunks: u64,
    turns: u64,
    turn_first_audio_at: Option<Instant>, // 回合内首音频到达时刻，播放首块消费它
    connect_started_at: Option<Instant>,
    last_ack_at: Option<Instant>, // 末单元 ACK 时刻（响应指标的起点）

    // —— 性能埋点（[perf] 周期行；各组件跨线程直接写入）——
    send_ms: Roll,         // unit send 全程（序列化+ws.send）
    json_ms: Roll,         // 其中 JSON 序列化部分
    tx_bytes: u64,         // 应用层上行字节（base64 后口径）
    rx_bytes: u64,         // 应用层下行字节
    loop_lag_max_ms: f64,  // session 事件循环 0.5s 探针：窗口内最大滞后
    ui_draw_ms: Roll,      // UI 整帧绘制（含 show）
    ui_show_ms: Roll,      // 其中 disp.show 部分
    ui_frames: u64,        // 窗口内绘制帧数（算 fps）
    ui_loop_max_ms: f64,   // UI 主循环节拍窗口内最大一轮耗时
    jpeg_ms: Roll,         // to_jpeg 编码耗时
    b64_ms: Roll,          // base64 编码耗时
    jpg_kb: Roll,          // JPEG 大小 KB
    cam_copy_ms: Roll,     // img.copy（屏显副本）
    cam_gap_max_ms: f64,   // 相机线程出帧间隔窗口最大值
    play_ms: Roll,         // speaker.play() 实际耗时
    play_gap_n: u64,       // 突发内块间隙 >30ms 次数（听得见的卡）
    play_gap_max_ms: f64,
    underruns: u64,        // 突发首块前导没攒够（欠载；累计值，不随窗口清零）
    // 窗口基线（perf_window 算速率用）
    win_t: Instant,
    win_tx: u64,
    win_rx: u64,
}

impl TimingState {
    fn new() -> Self {
        Self {
            capture_to_send: Roll::new(30),
            send_to_ack: Roll::new(30),
            recv_to_play: Roll::new(30),
            session_setup: Roll::new(30),
            net_rtt: Roll::new(30),
            response: Roll::new(30),
            units_sent: 0,
            units_dropped: 0,
            audio_chunks: 0,
            turns: 0,
            turn_first_audio_at: None,
            connect_started_at: None,
            last_ack_at: None,
            send_ms: Roll::new(30),
            json_ms: Roll::new(30),
            tx_bytes: 0,
            rx_bytes: 0,
            loop_lag_max_ms: 0.0,
            ui_draw_ms: Roll::new(20),
            ui_show_ms: Roll::new(20),
            ui_frames: 0,
            ui_loop_max_ms: 0.0,
            jpeg_ms: Roll::new(20),
            b64_ms: Roll::new(20),
            jpg_kb: Roll::new(20),
            cam_copy_ms: Roll::new(20),
            cam_gap_max_ms: 0.0,
            play_ms: Roll::new(20),
            play_gap_n: 0,
            play_gap_max_ms: 0.0,
            underruns: 0,
            win_t: Instant::now(),
            win_tx: 0,
            win_rx: 0,
        }
    }
}

fn dt_ms(now: Instant, t: Instant) -> f64 {
    now.saturating_duration_since(t).as_secs_f64() * 1000.0
}

/// 四个关键时刻 + 滚动统计（Python `TimingStats`）。
/// Clone 是 `Arc` 浅拷贝，跨线程共享同一份状态。
#[derive(Clone, Debug)]
pub struct Timing {
    inner: Arc<Mutex<TimingState>>,
}

impl Default for Timing {
    fn default() -> Self {
        Self::new()
    }
}

impl Timing {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(TimingState::new())),
        }
    }

    // —— 事件入口 -------------------------------------------------------

    /// 连接开始（只在首次生效，重连不覆盖）。
    pub fn connect_started(&self) {
        self.connect_started_at(Instant::now());
    }

    fn connect_started_at(&self, now: Instant) {
        let mut st = lock(&self.inner);
        if st.connect_started_at.is_none() {
            st.connect_started_at = Some(now);
        }
    }

    /// session.created 到达：记一次会话建立耗时（s）并清起点。
    pub fn session_ready(&self) {
        self.session_ready_at(Instant::now());
    }

    fn session_ready_at(&self, now: Instant) {
        let mut st = lock(&self.inner);
        if let Some(t0) = st.connect_started_at.take() {
            st.session_setup
                .add(now.saturating_duration_since(t0).as_secs_f64());
        }
    }

    /// 录音线程凑满 1s unit 的时刻（返回值喂给 [`Timing::unit_sent`] /
    /// [`Timing::send_to_ack`]）。
    pub fn unit_recorded(&self) -> Instant {
        Instant::now()
    }

    /// 发出 unit（WebSocket send 完成）：记采集->发送滞后。
    pub fn unit_sent(&self, t_recorded: Instant) {
        self.unit_sent_at(Instant::now(), t_recorded);
    }

    fn unit_sent_at(&self, now: Instant, t_recorded: Instant) {
        let mut st = lock(&self.inner);
        st.units_sent += 1;
        st.capture_to_send.add(dt_ms(now, t_recorded));
    }

    /// response.unit.committed：记 send -> ACK RTT。
    /// `input_id` 只是口径占位（v4 的 started_map 映射在 session 侧完成）。
    pub fn send_to_ack(&self, input_id: &str, t_sent: Instant) {
        self.send_to_ack_at(Instant::now(), input_id, t_sent);
    }

    fn send_to_ack_at(&self, now: Instant, _input_id: &str, t_sent: Instant) {
        let mut st = lock(&self.inner);
        st.send_to_ack.add(dt_ms(now, t_sent));
        st.last_ack_at = Some(now);
    }

    /// 每个回合的第一块音频到达时调用（`audio_chunks` 每块都计）。
    pub fn audio_received(&self) {
        self.audio_received_at(Instant::now());
    }

    fn audio_received_at(&self, now: Instant) {
        let mut st = lock(&self.inner);
        st.audio_chunks += 1;
        if st.turn_first_audio_at.is_none() {
            st.turn_first_audio_at = Some(now);
            // 响应：末单元 ACK -> 模型首音频。用户感知“反应快慢”的主要
            // 成分（服务端决策 + 推理 + 下行），从 ACK 起算已剔掉上行网络。
            if let Some(ack) = st.last_ack_at {
                st.response.add(dt_ms(now, ack));
            }
        }
    }

    /// 新回合（spoken 开始）：清掉上一回合的首音频基准。
    pub fn turn_started(&self) {
        lock(&self.inner).turn_first_audio_at = None;
    }

    /// 回合结束（turn_eos）：计数并清首音频基准。
    pub fn turn_ended(&self) {
        let mut st = lock(&self.inner);
        st.turns += 1;
        st.turn_first_audio_at = None;
    }

    /// 播放线程检测到“突发首块”时调用。
    pub fn playback_started(&self) {
        self.playback_started_at(Instant::now());
    }

    fn playback_started_at(&self, now: Instant) {
        let mut st = lock(&self.inner);
        if let Some(first) = st.turn_first_audio_at {
            st.recv_to_play.add(dt_ms(now, first));
        }
    }

    pub fn unit_dropped(&self) {
        lock(&self.inner).units_dropped += 1;
    }

    // —— 性能埋点入口（各组件线程调用，都是小操作）---------------------

    /// send 完成：t0=开始序列化时刻，t_json=序列化完成时刻。
    pub fn unit_send_done(&self, t0: Instant, t_json: Instant) {
        self.unit_send_done_at(Instant::now(), t0, t_json);
    }

    fn unit_send_done_at(&self, now: Instant, t0: Instant, t_json: Instant) {
        let mut st = lock(&self.inner);
        st.send_ms.add(dt_ms(now, t0));
        st.json_ms.add(dt_ms(t_json, t0));
    }

    pub fn tx_bytes(&self, n: u64) {
        lock(&self.inner).tx_bytes += n;
    }

    pub fn rx_bytes(&self, n: u64) {
        lock(&self.inner).rx_bytes += n;
    }

    pub fn loop_lag(&self, ms: f64) {
        let mut st = lock(&self.inner);
        if ms > st.loop_lag_max_ms {
            st.loop_lag_max_ms = ms;
        }
    }

    /// UI 整帧绘制耗时（每帧一次；同时计一帧用于窗口 fps）。
    pub fn ui_draw(&self, draw_ms: f64) {
        let mut st = lock(&self.inner);
        st.ui_draw_ms.add(draw_ms);
        st.ui_frames += 1;
    }

    /// 其中 disp.show 部分。
    pub fn ui_show(&self, show_ms: f64) {
        lock(&self.inner).ui_show_ms.add(show_ms);
    }

    /// UI 主循环单轮耗时（窗口最大值）。
    pub fn ui_loop(&self, ms: f64) {
        let mut st = lock(&self.inner);
        if ms > st.ui_loop_max_ms {
            st.ui_loop_max_ms = ms;
        }
    }

    /// 相机帧处理：`size`=JPEG 字节数，encode/b64/copy 为各阶段耗时（ms）。
    pub fn jpeg(&self, size: usize, encode_ms: f64, b64_ms: f64, copy_ms: f64) {
        let mut st = lock(&self.inner);
        st.jpeg_ms.add(encode_ms);
        st.jpg_kb.add(size as f64 / 1024.0);
        st.b64_ms.add(b64_ms);
        st.cam_copy_ms.add(copy_ms);
    }

    /// 相机线程出帧间隔（窗口最大值）。
    pub fn cam_gap(&self, ms: f64) {
        let mut st = lock(&self.inner);
        if ms > st.cam_gap_max_ms {
            st.cam_gap_max_ms = ms;
        }
    }

    /// speaker.play() 实际耗时。
    pub fn play(&self, ms: f64) {
        lock(&self.inner).play_ms.add(ms);
    }

    /// 突发内块间隙（>30ms 记一次，听得见的卡）。
    pub fn play_gap(&self, ms: f64) {
        let mut st = lock(&self.inner);
        st.play_gap_n += 1;
        if ms > st.play_gap_max_ms {
            st.play_gap_max_ms = ms;
        }
    }

    /// 突发首块前导没攒够（欠载；累计值）。
    pub fn underrun(&self) {
        lock(&self.inner).underruns += 1;
    }

    /// 直接喂 JSON 序列化耗时（[`Timing::unit_send_done`] 之外的独立埋点）。
    pub fn json_ms(&self, ms: f64) {
        lock(&self.inner).json_ms.add(ms);
    }

    /// 纯网络 RTT（NetRttProbe 喂）：TCP 握手往返。
    pub fn net_rtt(&self, ms: f64) {
        lock(&self.inner).net_rtt.add(ms);
    }

    // —— 窗口与输出 -----------------------------------------------------

    /// 取走一个统计窗口的速率/最大值（读后清零，供 `[perf]` 行消费）。
    /// 速率按真实流逝时间计算（`_interval_s` 与 Python 一样只是口径占位）。
    pub fn perf_window(&self, _interval_s: f64) -> PerfWindow {
        self.perf_window_at(Instant::now())
    }

    fn perf_window_at(&self, now: Instant) -> PerfWindow {
        let mut st = lock(&self.inner);
        let dt = now
            .saturating_duration_since(st.win_t)
            .as_secs_f64()
            .max(0.001);
        let win = PerfWindow {
            tx_kbs: st.tx_bytes.saturating_sub(st.win_tx) as f64 / dt / 1024.0,
            rx_kbs: st.rx_bytes.saturating_sub(st.win_rx) as f64 / dt / 1024.0,
            loop_lag_ms: st.loop_lag_max_ms,
            ui_fps: st.ui_frames as f64 / dt,
            ui_loop_ms: st.ui_loop_max_ms,
            cam_gap_ms: st.cam_gap_max_ms,
            play_gap_n: st.play_gap_n,
            play_gap_ms: st.play_gap_max_ms,
        };
        st.win_t = now;
        st.win_tx = st.tx_bytes;
        st.win_rx = st.rx_bytes;
        st.loop_lag_max_ms = 0.0;
        st.ui_frames = 0;
        st.ui_loop_max_ms = 0.0;
        st.cam_gap_max_ms = 0.0;
        st.play_gap_n = 0;
        st.play_gap_max_ms = 0.0;
        win
    }

    /// 快照（给 UI 与日志；字段与 timing.py snapshot() 一致）。
    pub fn snapshot(&self) -> TimingSnapshot {
        let st = lock(&self.inner);
        TimingSnapshot {
            units_sent: st.units_sent,
            units_dropped: st.units_dropped,
            turns: st.turns,
            audio_chunks: st.audio_chunks,
            capture_to_send: st.capture_to_send.ms_metric(),
            ack_rtt: st.send_to_ack.ms_metric(),
            recv_to_play: st.recv_to_play.ms_metric(),
            net_rtt: st.net_rtt.ms_metric(),
            response: st.response.ms_metric(),
            session_setup_s: st.session_setup.last().map(|v| py_round(v, 2)),
        }
    }

    /// 10s 看门狗日志一行（格式照 timing.py `summary_line`）。
    pub fn summary_line(&self) -> String {
        let s = self.snapshot();
        let lat = |m: &Option<Metric>| match m {
            Some(m) => (py_f64_str(m.last_ms), py_f64_str(m.avg_ms)),
            None => ("-".to_string(), "-".to_string()),
        };
        let (net_l, net_a) = lat(&s.net_rtt);
        let (c2s_l, c2s_a) = lat(&s.capture_to_send);
        let (ack_l, ack_a) = lat(&s.ack_rtt);
        let (r2p_l, r2p_a) = lat(&s.recv_to_play);
        let (resp_l, resp_a) = lat(&s.response);
        let setup = match s.session_setup_s {
            Some(v) => py_f64_str(v),
            None => "-".to_string(),
        };
        format!(
            "units={}(+drop {}) 网 {}ms(均 {}) 采集->发送 {}ms(均 {}) ACK {}ms(均 {}) \
             响应 {}ms(均 {}) 收音->出声 {}ms(均 {}) 建会话 {}s",
            s.units_sent, s.units_dropped, net_l, net_a, c2s_l, c2s_a, ack_l, ack_a, resp_l,
            resp_a, r2p_l, r2p_a, setup
        )
    }

    /// `[perf]` 行用的各窗口均值（空窗口为 `None` -> 显示 "-"）。
    pub fn avgs(&self) -> TimingAvgs {
        let st = lock(&self.inner);
        TimingAvgs {
            send_ms: st.send_ms.mean(),
            json_ms: st.json_ms.mean(),
            ui_draw_ms: st.ui_draw_ms.mean(),
            ui_show_ms: st.ui_show_ms.mean(),
            jpg_kb: st.jpg_kb.mean(),
            jpeg_ms: st.jpeg_ms.mean(),
            b64_ms: st.b64_ms.mean(),
            cam_copy_ms: st.cam_copy_ms.mean(),
            play_ms: st.play_ms.mean(),
            underruns: st.underruns,
        }
    }

    // —— 测试辅助（注入假时钟）——

    #[cfg(test)]
    pub(crate) fn window_reset_at(&self, now: Instant) {
        lock(&self.inner).win_t = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn rolling_semantics() {
        let r = Rolling::new(3);
        assert_eq!(r.last(), None);
        assert_eq!(r.mean(), 0.0);
        for v in [1.0, 2.0, 3.0, 4.0, 5.0] {
            r.push(v);
        }
        assert_eq!(r.last(), Some(5.0));
        assert_eq!(r.mean(), 4.0); // 只留最近 3 个：3,4,5

        let r = Rolling::new(30);
        for i in 0..35 {
            r.push(i as f64);
        }
        assert_eq!(r.last(), Some(34.0));
        let expect: f64 = (5..35).sum::<i64>() as f64 / 30.0;
        assert!((r.mean() - expect).abs() < 1e-9);
    }

    #[test]
    fn rolling_thread_sharing() {
        // cap 放大到装下全部值：均值与推入顺序无关（整数和精确），可精确断言
        let r = Arc::new(Rolling::new(400));
        let handles: Vec<_> = (0..4u64)
            .map(|t| {
                let r = Arc::clone(&r);
                thread::spawn(move || {
                    for i in 0..100 {
                        r.push((t * 100 + i) as f64);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // 400 个值无丢失地全部落窗（有丢失/重复即偏离）；和为整数，求和顺序无关
        assert_eq!(r.mean(), 199.5); // (0+..+399)/400
        let last = r.last().unwrap();
        assert!((0.0..=399.0).contains(&last)); // 调度序不定，只验范围
    }

    #[test]
    fn four_moments_and_derived_intervals() {
        let t = Timing::new();
        let base = Instant::now();
        // 会话建立：connect -> created = 2s
        t.connect_started_at(base);
        t.connect_started_at(base + ms(5000)); // 重连不覆盖起点
        t.session_ready_at(base + ms(2000));
        t.session_ready_at(base + ms(9000)); // 起点已清，不再记
        // 四个时刻（注入假 Instant）
        t.unit_sent_at(base + ms(100), base); // capture_to_send = 100ms
        t.send_to_ack_at(base + ms(250), "u1", base + ms(100)); // ack = 150ms
        t.audio_received_at(base + ms(400)); // response = 400-250 = 150ms
        t.playback_started_at(base + ms(450)); // recv_to_play = 450-400 = 50ms
        t.net_rtt(20.5);
        t.net_rtt(20.7);
        t.unit_dropped();

        let s = t.snapshot();
        assert_eq!(s.units_sent, 1);
        assert_eq!(s.units_dropped, 1);
        assert_eq!(s.audio_chunks, 1);
        assert_eq!(
            s.capture_to_send,
            Some(Metric {
                last_ms: 100.0,
                avg_ms: 100.0
            })
        );
        assert_eq!(
            s.ack_rtt,
            Some(Metric {
                last_ms: 150.0,
                avg_ms: 150.0
            })
        );
        assert_eq!(
            s.response,
            Some(Metric {
                last_ms: 150.0,
                avg_ms: 150.0
            })
        );
        assert_eq!(
            s.recv_to_play,
            Some(Metric {
                last_ms: 50.0,
                avg_ms: 50.0
            })
        );
        assert_eq!(
            s.net_rtt,
            Some(Metric {
                last_ms: 20.7,
                avg_ms: 20.6
            })
        );
        assert_eq!(s.session_setup_s, Some(2.0));
    }

    #[test]
    fn turn_semantics() {
        let t = Timing::new();
        let base = Instant::now();
        t.send_to_ack_at(base + ms(100), "u1", base);
        t.audio_received_at(base + ms(200)); // response = 100ms
        t.playback_started_at(base + ms(260)); // recv_to_play = 60ms
        t.turn_ended(); // turns=1，清首音频基准
        t.audio_received_at(base + ms(4000)); // 新回合首音频（response = 3900ms）
        t.playback_started_at(base + ms(4030)); // recv_to_play = 30ms
        let s = t.snapshot();
        assert_eq!(s.turns, 1);
        assert_eq!(s.audio_chunks, 2);
        assert_eq!(
            s.recv_to_play,
            Some(Metric {
                last_ms: 30.0,
                avg_ms: 45.0
            })
        );
        assert_eq!(
            s.response,
            Some(Metric {
                last_ms: 3900.0,
                avg_ms: 2000.0
            })
        );
    }

    #[test]
    fn unit_send_done_feeds_send_and_json() {
        let t = Timing::new();
        let base = Instant::now();
        t.unit_send_done_at(base + ms(30), base, base + ms(10));
        let a = t.avgs();
        assert_eq!(a.send_ms, Some(30.0));
        assert_eq!(a.json_ms, Some(10.0));
        t.json_ms(5.0); // 独立入口
        assert_eq!(t.avgs().json_ms, Some(7.5));
    }

    #[test]
    fn perf_window_read_and_clear() {
        let t = Timing::new();
        let base = Instant::now();
        t.window_reset_at(base);
        t.tx_bytes(2048);
        t.rx_bytes(1024);
        t.ui_draw(5.0);
        t.ui_draw(5.0);
        t.ui_show(1.0);
        t.ui_loop(40.0);
        t.loop_lag(60.0);
        t.cam_gap(70.0);
        t.play_gap(35.0);
        t.play(3.0);
        t.underrun();
        t.underrun();
        let w = t.perf_window_at(base + ms(1000));
        assert_eq!(w.tx_kbs, 2.0); // 2048B / 1s / 1024
        assert_eq!(w.rx_kbs, 1.0);
        assert_eq!(w.ui_fps, 2.0);
        assert_eq!(w.loop_lag_ms, 60.0);
        assert_eq!(w.ui_loop_ms, 40.0);
        assert_eq!(w.cam_gap_ms, 70.0);
        assert_eq!(w.play_gap_n, 1);
        assert_eq!(w.play_gap_ms, 35.0);
        // 读后清零：无新数据的窗口全为 0
        let w2 = t.perf_window_at(base + ms(2000));
        assert_eq!(w2.tx_kbs, 0.0);
        assert_eq!(w2.rx_kbs, 0.0);
        assert_eq!(w2.ui_fps, 0.0);
        assert_eq!(w2.loop_lag_ms, 0.0);
        assert_eq!(w2.ui_loop_ms, 0.0);
        assert_eq!(w2.cam_gap_ms, 0.0);
        assert_eq!(w2.play_gap_n, 0);
        assert_eq!(w2.play_gap_ms, 0.0);
        // underruns 是累计值，不随窗口清零
        assert_eq!(t.avgs().underruns, 2);
    }

    #[test]
    fn perf_window_public_smoke() {
        let t = Timing::new();
        t.tx_bytes(4096);
        let w = t.perf_window(10.0);
        assert!(w.tx_kbs > 0.0);
        assert_eq!(t.perf_window(10.0).tx_kbs, 0.0); // 基线已更新
    }

    #[test]
    fn summary_line_format() {
        // 期望串已与 Python timing.py 实跑输出逐字核对（同输入 monkeypatch 时钟）
        let t = Timing::new();
        let base = Instant::now();
        t.connect_started_at(base);
        t.session_ready_at(base + ms(2000));
        t.unit_sent_at(base + ms(100), base);
        t.send_to_ack_at(base + ms(250), "u1", base + ms(100));
        t.audio_received_at(base + ms(400));
        t.playback_started_at(base + ms(450));
        t.net_rtt(20.5);
        t.net_rtt(20.7);
        t.unit_dropped();
        assert_eq!(
            t.summary_line(),
            "units=1(+drop 1) 网 20.7ms(均 20.6) 采集->发送 100.0ms(均 100.0) \
             ACK 150.0ms(均 150.0) 响应 150.0ms(均 150.0) 收音->出声 50.0ms(均 50.0) 建会话 2.0s"
        );
    }

    #[test]
    fn summary_line_empty() {
        let t = Timing::new();
        assert_eq!(
            t.summary_line(),
            "units=0(+drop 0) 网 -ms(均 -) 采集->发送 -ms(均 -) ACK -ms(均 -) \
             响应 -ms(均 -) 收音->出声 -ms(均 -) 建会话 -s"
        );
    }

    #[test]
    fn snapshot_json_keys_match_python() {
        let t = Timing::new();
        t.net_rtt(20.5);
        let v = serde_json::to_value(t.snapshot()).unwrap();
        for key in [
            "units_sent",
            "units_dropped",
            "turns",
            "audio_chunks",
            "capture_to_send",
            "ack_rtt",
            "recv_to_play",
            "net_rtt",
            "response",
            "session_setup_s",
        ] {
            assert!(v.get(key).is_some(), "missing key {key}");
        }
        assert_eq!(v["net_rtt"]["last_ms"], serde_json::json!(20.5));
        assert_eq!(v["session_setup_s"], serde_json::json!(null));
    }

    #[test]
    fn public_entry_points_smoke() {
        let t = Timing::new();
        let t0 = t.unit_recorded();
        t.connect_started();
        t.session_ready();
        t.unit_sent(t0);
        t.unit_send_done(t0, t0);
        t.send_to_ack("u1", t0);
        t.audio_received();
        t.playback_started();
        t.turn_started();
        t.turn_ended();
        t.unit_dropped();
        t.ui_draw(1.0);
        t.ui_show(0.5);
        t.ui_loop(60.0);
        t.loop_lag(30.0);
        t.jpeg(1024, 5.0, 1.0, 0.5);
        t.cam_gap(33.0);
        t.play(2.0);
        t.play_gap(31.0);
        t.underrun();
        t.json_ms(0.5);
        t.net_rtt(21.0);
        t.tx_bytes(1);
        t.rx_bytes(2);
        let s = t.snapshot();
        assert_eq!(s.units_sent, 1);
        assert_eq!(s.audio_chunks, 1);
        assert_eq!(s.turns, 1);
        assert!(s.capture_to_send.is_some());
        assert!(s.recv_to_play.is_some());
        assert!(s.session_setup_s.is_some());
        assert_eq!(t.avgs().jpg_kb, Some(1.0));
    }

    #[test]
    fn shared_across_threads() {
        let t = Timing::new();
        let th = {
            let t = t.clone();
            thread::spawn(move || {
                t.tx_bytes(100);
                t.unit_dropped();
            })
        };
        th.join().unwrap();
        t.tx_bytes(50);
        let base = Instant::now();
        t.window_reset_at(base);
        let w = t.perf_window_at(base + ms(1000));
        assert_eq!(w.tx_kbs, 150.0 / 1024.0);
        assert_eq!(t.snapshot().units_dropped, 1);
    }
}

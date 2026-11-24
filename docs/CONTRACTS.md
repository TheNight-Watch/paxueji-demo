# 跨 crate 契约（CONTRACTS.md）

本文件是所有 crate 之间的**公共 API 规范**。各 agent 实现自己 crate 时：
- **必须严格遵循**这里给出的跨 crate 签名（其他 crate 依赖它们）
- crate 内部实现自由发挥，但行为必须与对应 Python 模块一致（忠实移植）
- 所有代码必须 `cargo check` + `cargo test` 在 **Windows host** 上通过（设备相关路径一律 `#[cfg(target_os = "linux")]` 门控并给出 PC no-op/mock 路径）
- 交叉编译目标 aarch64-unknown-linux-gnu 也必须通过 `cargo check`（`cargo zigbuild`，zig 在 `C:\Users\1\AppData\Roaming\Python\Python314\site-packages\ziglang`，需要 `export PATH=$PATH:/c/Users/1/AppData/Roaming/Python/Python314/site-packages/ziglang`）
- 日志统一用 `log` crate（宏 `info!/warn!/error!/debug!`），具体 logger 由 o5-app 安装
- 时间统一 `std::time::Instant`（单调）+ `chrono`（墙钟）

依赖版本见根 `Cargo.toml` `[workspace.dependencies]`（用 `foo.workspace = true` 引用）。

## o5-config（Wave 1）

```rust
pub struct Config { /* 所有字段平铺，与 Python o5/config.py DEFAULTS 一一对应 */ }
impl Config {
    pub fn defaults() -> Config;
    /// 搜索顺序：cli_path > env O5_HARNESS_CONFIG > /root/o5_config.json
    /// > <exe_dir>/config.example.json > defaults；文件不存在用 defaults（不报错）
    pub fn load(cli_path: Option<&str>) -> Result<Config, ConfigError>;
    pub fn from_json_str(s: &str) -> Result<Config, ConfigError>;
    /// 原子保存：写 <path>.o5tmp 后 rename
    pub fn save(&self, path: &str) -> Result<(), ConfigError>;
}

pub struct Voice {
    pub spec: String,        // "" | "builtin:<name>" | 绝对路径
    pub data: Vec<u8>,       // 规范化后的 16k mono PCM16 WAV 字节（合规文件原样直通）
    pub sha256: String,      // data 的 hex sha256
    pub data_b64: String,   // data 的 base64
}
impl Voice { pub fn load(spec: &str, assets_dir: &Path) -> Result<Voice, ConfigError>; }

pub struct ConfigError(pub String);  // impl Display + std::error::Error
```

字段清单（平铺命名与 Python 相同，camelCase JSON key 用 serde rename）：
`fg_endpoint/fg_api_key/fg_protocol_version(u8 3|4|5)/fg_mode/fg_omni_mode/fg_auto_start_session/fg_queue_gate/fg_progress_timeout_s/fg_ack_timeout_s/fg_connect_timeout_s/fg_tls_insecure_skip_verify/fg_shell_prompt/fg_ref_audio_segment/fg_task_prompt/fg_init_payload(serde_json::Value)/fg_voice`；
`bg_enabled/bg_endpoint/bg_api_key/bg_model/bg_timeout_s/bg_max_concurrent/bg_system_prompt/bg_max_result_chars/bg_request_parameters(serde_json::Value)`；
`audio_input_volume(0-100)/audio_output_volume/audio_output_sample_rate/audio_mic_gate_while_playing/audio_gate_tail_ms/audio_echo_cancel/audio_echo_mode/audio_echo_max_delay_ms/audio_echo_debug_dump/audio_mic_duck_while_playing/audio_mic_debug_dump/audio_mic_dump_path/audio_max_session_minutes`；
`video_enabled/video_jpeg_quality/video_width/video_height/video_fps/video_hmirror/video_vflip`；
`log_file/log_level`；
`tools_tavily_api_key/tools_serper_api_key/tools_brave_api_key/tools_amap_api_key/tools_amap_service_host`；
`asr_enabled/asr_endpoint/asr_api_key/asr_service_id/asr_max_reconnects/asr_gate_while_playing`；
`net_mode/net_usb_relay_host/net_usb_relay_port_map(BTreeMap<String,u16>)`；
`device_label`、`device_touch_variant(u8, 默认 2)`、`device_display_feeder_path/device_camera_feeder_path(String, 默认与二进制同目录 feeder_display.py/feeder_camera.py)`。
（提供 `voice()` -> `&Voice` 缓存字段或 getter；taskPrompt 的 LEGACY_TASK_PROMPTS 指纹升级逻辑照 Python 移植。）

## o5-protocol（Wave 1）

```rust
pub const INPUT_SAMPLE_RATE: u32 = 16000;
pub const UNIT_SAMPLES: usize = 16000;
pub const UNIT_BYTES: usize = 32000;
pub const DEFAULT_OUTPUT_SAMPLE_RATE: u32 = 24000;
pub const TOOL_RESULT_MAX_BYTES: usize = 2048;
pub const TOOL_RESULT_DELTA_CHUNK_BYTES: usize = 1024;
pub const BLACK_JPEG_B64: &str;  // 448x448 黑图，从 protocol.py 原样拷贝

// PCM16 <-> base64 float32 LE（与 Python pcm16_to_float32_b64 / float32_b64_to_pcm16 逐位一致：
// int16 / 32768.0；解码 nan_to_num + clip ±1 + rint(*32767)）
pub fn pcm16_to_float32_b64(pcm: &[u8]) -> String;
pub fn decode_audio(b64: &str, default_rate: u32) -> Option<(Vec<u8>, u32)>;  // (pcm16, sample_rate)
pub fn resample_pcm16(data: &[u8], from: u32, to: u32) -> Vec<u8>;            // 线性插值，同 protocol.py
pub fn rms_db(pcm16: &[u8]) -> f32;

// 客户端消息构造（返回完整 JSON 文本帧）
pub fn build_session_init(...) -> String;   // 见 protocol.py 148-186：payload 含 protocol_version/mode:"full_duplex"/fc_duplex:true/generate_audio:true/config.decoding_policy:"constrained_decoding"/system.segments(shell+voice wav+tts_prompt_audio)/system.tools
pub fn build_input_append(input_id: &str, pcm16: &[u8], video_jpeg_b64: Option<&str>, force_listen: bool, version: u8) -> String;
pub fn build_tool_result(tool_call_id: &str, content: &str, version: u8) -> String;  // v4+ 原子；v3 list
pub fn build_tool_result_delta(tool_call_id: &str, content: &str) -> String;
pub fn build_tool_result_done(tool_call_id: &str) -> String;                        // v3 超长分片
pub fn build_session_close() -> String;
pub fn build_standalone_event(text: &str) -> String;

// 服务端帧解析
pub struct ParsedToolCall { pub id: String, pub name: String, pub arguments: String }
pub enum ServerFrame {
    Queued, QueueUpdate { position: i64 }, QueueDone,
    Created { session_id: String, resume_sha256: Option<String> },
    UnitStarted { input_id: Option<String>, unit_index: i64 },
    UnitCommitted { input_id: Option<String>, unit_index: i64 },
    SpokenDelta { text: String, audio: Option<(Vec<u8>, u32)> },  // audio=(pcm16, rate)
    SpokenEnd { reason: String },
    ThinkBegin, ThinkDelta { text: String }, ThinkEnd,
    ToolCallBegin { id: String }, ToolCallDelta { id: Option<String>, arguments: String },
    ToolCallDone { call: Option<ParsedToolCall> }, ToolCallAbort { tool_call_id: String },
    NonSpokenEnd, Closed { reason: String, diagnostic: Option<String> },
    Error { code: String, message: String },
    Unknown { type_: String },
}
pub fn parse_server_frame(text: &str) -> Option<ServerFrame>;
```

v3/v4/v5 行为差异（`audio` vs `audio_base64` 字段、tool_result 形态、v5 占位黑帧）全部照 protocol.py。单测电池移植 `tools/test_session_pc.py:174-239` 的全部断言（f32 往返 ≤1 LSB、v4/v3 字段、session.init payload 结构、spoken_text、parse_tool_call、rms_db 边界）。

## o5-perf（Wave 1）

```rust
pub struct Rolling { /* cap 窗口 */ }
impl Rolling { pub fn new(cap: usize) -> Self; pub fn push(&self, v: f64); pub fn last(&self) -> Option<f64>; pub fn mean(&self) -> f64; }

pub struct Timing { /* 内部 Mutex，跨线程共享 Clone（Arc） */ }
impl Timing {
    pub fn new() -> Self;  // 提供 Default
    // 四个关键时刻（与 timing.py 相同口径）
    pub fn unit_recorded(&self) -> Instant;              // 返回 t0，喂给下面两个
    pub fn unit_send_done(&self, t0: Instant, t_json: Instant);
    pub fn unit_sent(&self, t_recorded: Instant);
    pub fn audio_received(&self);
    pub fn playback_started(&self);
    pub fn send_to_ack(&self, input_id: &str, t_sent: Instant);  // v4: started_map 在 session 侧
    pub fn unit_dropped(&self);
    // UI/相机/播放指标（Rolling(20)）：ui_draw/ui_show/ui_loop/ui_fps、jpeg_*、cam_gap、play/play_gap/underrun、loop_lag、json_ms、b64_ms
    pub fn ui_draw(&self, draw_ms: f64); pub fn ui_show(&self, show_ms: f64); pub fn ui_loop(&self, ms: f64);
    pub fn loop_lag(&self, ms: f64); pub fn jpeg(&self, size: usize, encode_ms: f64, b64_ms: f64, copy_ms: f64);
    pub fn cam_gap(&self, ms: f64); pub fn play(&self, ms: f64); pub fn play_gap(&self, ms: f64); pub fn underrun(&self);
    pub fn json_ms(&self, ms: f64);
    pub fn net_rtt(&self, ms: f64);
    pub fn tx_bytes(&self, n: u64); pub fn rx_bytes(&self, n: u64);
    // 快照（给 UI 与日志）
    pub fn snapshot(&self) -> TimingSnapshot;  // serde::Serialize，字段与 timing.py snapshot() 对齐
    pub fn summary_line(&self) -> String;      // 10s 看门狗日志格式
    pub fn perf_window(&self, interval_s: f64) -> PerfWindow;  // 读清窗口：tx/rx KB/s、maxima
}

pub struct PerfMonitor;  // 10s 采样 /proc/self/stat（进程 CPU%）、/proc/self/task/*（top3 线程）、/proc/net/wireless（dBm）
impl PerfMonitor { pub fn spawn(self) -> std::thread::JoinHandle<()>; pub fn snapshot(&self) -> PerfSnapshot; }
// /proc 读取全部 #[cfg(target_os="linux")]；Windows 下返回 None/0（不 panic）

pub mod netmode {
    pub fn effective_url(url: &str, cfg: &Config) -> String;   // usb 模式按 portMap 改端口
    pub fn sync_hosts(cfg: &Config) -> std::io::Result<()>;    // /etc/hosts BEGIN/END o5-usb-relay 块（linux only）
    pub struct NetRttProbe;  // 5s TCP 连接 RTT 探测，喂 timing.net_rtt
    impl NetRttProbe { pub fn spawn(...) -> JoinHandle<()>; }
}
```

## o5-tools（Wave 1）

```rust
pub struct ToolExecutor { /* keys + amap host */ }
impl ToolExecutor {
    pub fn new(tavily_key: &str, serper_key: &str, brave_key: &str,
               amap_key: &str, amap_host: &str) -> Self;
    /// 阻塞执行；异常转为 "[工具失败] {name}: ..." 文本（永不 panic）
    pub fn execute(&self, name: &str, arguments: &str) -> String;
    pub fn foreground_definitions(&self) -> serde_json::Value;   // web_search+map_search 前台版 schema
    pub fn background_definitions(&self) -> serde_json::Value;
}
pub const TOOL_TRAINING_RANK: &[(&str, u8)];  // show_card:0 get_client_location:1 get_current_time:2 web_search:3 calculate:4 map_search:5 amap_weather:6
pub fn order_for_training(name: &str) -> usize;
/// JSON 优先，失败退 Python 字面量解析（单引号 dict），再退 {}
pub fn parse_arguments(raw: &str) -> serde_json::Value;
```

端点/超时/降级链/错误文案与 tools.py 逐字一致（Tavily→Serper→Brave→AnySearch 免 key 永远垫底、链路预算 30s 首家豁免、429/402 映射限流欠费、高德 key 泄露遮蔽 `***`）。

## o5-audio（Wave 2）

```rust
pub trait CaptureSource: Send { fn read_pcm16(&mut self, buf: &mut [u8]) -> std::io::Result<usize>; }
pub trait PlaybackSink: Send { fn write_pcm16(&mut self, pcm: &[u8]) -> std::io::Result<()>; }

pub enum AecImpl { Off, Nlms(NlmsAec), Subband(SubbandSuppressor) }
impl AecImpl { pub fn push_far(&mut self, far16k: &[i16]); pub fn cancel(&mut self, near: &[i16]) -> Vec<i16>; pub fn reset(&mut self); pub fn stats(&self) -> serde_json::Value; }
pub struct NlmsAec;       // o5/aec.py 1:1 移植（FFT 互相关延迟对齐+分数采样漂移补偿+NLMS 子块）
pub struct SubbandSuppressor;  // o5/subband_aec.py 1:1 移植（STFT 512/256 Hann、20 对数带、中位数 G、包络投票锁）

pub struct MicLevelTracker;  // ui 用的电平条
pub struct AudioIo;          // 组合：mic 线程 + speaker 线程 + AEC + 门控/闪避
impl AudioIo {
    pub fn new(cfg: &Config, timing: Arc<Timing>, capture: Box<dyn CaptureSource>) -> Self;
    /// on_unit 在 mic 线程回调（1s unit=32000B + t_recorded），实现方必须快（只投递）
    pub fn start_mic(&mut self, on_unit: Box<dyn FnMut(Vec<u8>, Instant) + Send>);
    pub fn stop_mic(&mut self);
    /// 播放块入队（drop-oldest，cap 120）；rate ∈ {16000,24000,48000}，48k 由调用方重采样后传入
    pub fn put_speaker(&self, pcm16: Vec<u8>, rate: u32);
    pub fn playing(&self, tail_s: f64) -> bool;   // 网关窗口
    pub fn set_muted(&self, m: bool); pub fn set_volume(&self, v: u8);
    pub fn stop_speaker(&self);  // 清队 + 等 drain
    pub fn start(&mut self);     // 启动 speaker 线程
}
```

设备实现（linux cfg）：arecord/aplay 子进程（参数见 device-findings.md）；PC mock：静音环回。
黄金向量测试：`tests/fixtures/aec/`（near/far/out .s16 + meta.json，25ms 块内容对齐节奏），Rust 实现输出与 out.s16 数值容差比对（收敛段建议 RMS 相对误差 ≤1% 或 max|diff| ≤ 3 LSB，瞬态段放宽 —— agent 依实测定档并写明）。

## o5-session（Wave 2）

```rust
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SessionState { Idle, Connecting, Waiting, Initializing, Ready, Closing, Closed, Failed }

pub trait SessionListener: Send + Sync {
    fn on_state(&self, s: SessionState);
    fn on_queue_position(&self, pos: i64);
    fn on_audio(&self, pcm16: Vec<u8>, rate: u32);
    fn on_spoken_text(&self, text: &str, final_: bool);
    fn on_think_text(&self, text: &str);
    fn on_tool_call(&self, call: &o5_protocol::ParsedToolCall);
    fn on_tool_abort(&self, tool_call_id: &str);
    fn on_error(&self, code: &str, message: &str);
    fn on_closed(&self, reason: &str);
}

pub struct O5Session { /* 内部线程 */ }
impl O5Session {
    /// 启动连接线程（WS: tungstenite + rustls；tlsInsecureSkipVerify 用自定义 verifier）
    pub fn start(&self);
    /// 优雅关闭：session.close → 等 closed ≤5s → 强关；join ≤10s
    pub fn stop(&self);  // 幂等
    pub fn state(&self) -> SessionState;
    /// 投递 1s unit（跨线程无阻塞：channel）；非 ready 丢弃；len!=32000 断言
    pub fn offer_unit(&self, pcm16: Vec<u8>, video_jpeg_b64: Option<String>, t_recorded: Instant);
    pub fn send_tool_result(&self, tool_call_id: &str, content: &str);
    pub fn send_tool_result_raw(&self, tool_call_id: &str, content: &str); // v3 分片在内部处理
    pub fn send_standalone_event(&self, text: &str);
    /// 置 force_listen 待发标志（下个 input.append 携带 force_listen:true）
    pub fn interrupt(&self);
    pub fn set_video_frame(&self, jpeg_b64: String);  // 最新帧槽（omni 用）
}
```

流控常量照抄 session.py：`COLD_INFLIGHT_LIMIT=30 / WARM_INFLIGHT_LIMIT=2 / OUTBOX_CAP=30(drop-oldest) / STARTED_MAP_CAP=64`；看门狗 1s tick：progress 超时（连接期 60s）、ack 超时（最老未确认 30s）；WS 选项 ping 20/20、无压缩、max_size 16MB、URL 加 `?mode=`；一次性会话（失败不自动重连）。发方为独立 writer 线程（tungstenite split）。

## o5-asr / o5-delegation（Wave 2）

```rust
// o5-asr
pub struct AsrClient;
impl AsrClient {
    pub fn new(endpoint: String, api_key: String, service_id: i64, max_reconnects: u32,
               on_text: Box<dyn Fn(String, bool) + Send>, on_status: Box<dyn Fn(&str) + Send>) -> Self;
    pub fn start(&self); pub fn stop(&self);      // commit + close 1000 优雅停
    pub fn feed(&self, pcm16: Vec<u8>);           // 1s unit，非阻塞投递
    pub fn update_endpoint(&self, url: String);   // 触发重连
}
// 协议：open 后 auth 帧 → session.created → session.update{service_id} → session.updated
// 缓冲 ≤10 unit；transcript.text.delta/done；退避 1/2/4/8s 上限 3 次；retryable/overloaded 才重连

// o5-delegation
pub struct DelegationManager;
impl DelegationManager {
    pub fn new(enabled: bool, endpoint: String, api_key: String, model: String,
               timeout_s: f64, max_concurrent: usize, system_prompt: String,
               max_result_chars: usize, request_params: serde_json::Value,
               tools: Option<std::sync::Arc<o5_tools::ToolExecutor>>,
               send_result: Box<dyn Fn(&str, &str) + Send + Sync>) -> Self;
    /// 在 session 读线程调用（快路径）；返回 Some(立即回复文本)
    pub fn handle_tool_call(&self, name: &str, arguments: &str) -> Option<String>;
    pub fn handle_tool_abort(&self, tool_call_id: &str);
}
// delegate_task 语义/文案、任务表 20 条、SPEAK_DIRECTIVE、超时预算 timeout+5(+60 带工具)、
// 400-with-tools 自动去 tools 重试、保留字段不可覆盖、get_current_time 即答 —— 全照 delegation.py
```

## o5-camera / o5-ui（Wave 3）

```rust
// o5-camera（不依赖 o5-ui，用自有 PreviewFrame 解耦）
pub struct PreviewFrame { pub width: u32, pub height: u32, pub data: Vec<u8> }  // RGB888
pub struct CameraFeed;
impl CameraFeed {
    /// 启动 feeder_camera.py 子进程（协议：stdout [u32 LE len][RGB888 w*h*3]，全速推帧）
    pub fn new(width: u32, height: u32, fps: u32, hmirror: bool, vflip: bool,
               feeder_path: &Path, preview_size: (u32, u32)) -> Self;
    pub fn latest_jpeg_b64(&self, quality: u8) -> Option<String>;  // 按需编码（1fps 上行）
    pub fn latest_preview(&self) -> Option<PreviewFrame>;           // 8Hz 缩放后的 640x480 RGB
    pub fn stop(&mut self);
}

// o5-ui
pub struct Image { pub width: u32, pub height: u32, pub data: Vec<u8> }  // RGB888
impl Image { draw_rect/fill_rect/draw_image/blend_strip/glyph 渲染 ... }
pub enum UiEvent { StartStop, Mute, Mode, Menu, PanelToggle, PublicNet, AsrToggle, VolumeChanged(f32), CloseApp, SessionResult(bool) }
pub struct Ui;
impl Ui {
    /// 阻塞运行主循环（15fps 浮点节拍、触摸 50Hz、事件从 channel 出）
    pub fn run(&mut self, state: SharedUiState, timing: Arc<Timing>, perf: ..., events: crossbeam_channel::Sender<UiEvent>);
    pub fn stop(&mut self);
}
pub struct SharedUiState(/* 内部 RwLock 的状态集：字幕/转写/指标/按钮态/音量/网络/相机预览 */);
```

## o5-app（Wave 3）

`main.rs`：CLI（`--headless`、`--config <path>`、`--test-pc`、`--selftest`）；组装全部 crate；speak-nudge（5s×2）；maxSessionMinutes 重启；退出看门狗线程（40s SIGTERM 自杀，linux cfg）；日志后台线程写文件（4MB 轮转）。`--test-pc`：mock 音频/相机/UI，跑完整协议链（对接 Python stub_server.py）。

## 版本与公共细节

- edition 2021、`version.workspace = true`
- `serde` 字段全部 `#[serde(rename_all = "camelCase", default)]` + `Default` impl 对齐 Python DEFAULTS
- 每个 crate 根 lib.rs 写模块级 doc comment（移植自对应 Python 文件头部注释）
- `cargo clippy --all-targets -- -D warnings` 必须干净；测试用 `cargo test -p <crate>`

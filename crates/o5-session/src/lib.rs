//! O5 实时会话客户端：WebSocket 连接 + queue gate + one-in-flight ACK 流控。
//!
//! 架构（对齐 Glass_Omni_Cpm_Harness_Demo 的 O5Session 与 cpm-harness 的
//! ws-client/ack-queue；从 Python `o5/session.py` 忠实移植）：
//!
//! - 线程模型：`start()` 起 2 条线程——reader 与 writer，共享一把
//!   `Arc<Mutex<WebSocket>>`（tungstenite sync 无内建 split，全双工靠
//!   读超时轮询 + 互斥交替收发；语义等价 Python 的单事件循环协程并发）。
//!   对外 API（`offer_unit` / `stop` / `interrupt` / `send_tool_result`）
//!   全部线程安全、非阻塞投递。
//! - 状态机：idle -> connecting -> waiting(排队) -> initializing -> ready
//!   -> closing -> closed；任意阶段出错 -> failed（O5 一次性会话语义，
//!   不自动重连，由上层决定重建）。
//! ```text
//!                queue_done/created(补发 init)        session.created
//!  idle -> connecting -> waiting ----------> initializing --------> ready
//!            |             |                        |                |
//!            |             +-- queue_update ---------+                |
//!            |                                                                      |
//!            +------------------ 任意阶段错误 -> failed <- ack/progress 超时 <-+
//!
//!  ready -> (stop) closing -> closed   （session.close -> 5s -> 强断）
//! ```
//! - queue gate：连接后等 `session.queue_done` 再发 `session.init`
//!   （直连 worker 没有队列帧时，`session.created` 先到也补发 init）。
//! - 音频时钟红线：静音也每秒发一个 unit；ready 之前丢音频；committed ACK
//!   前限制在途数量（冷启动 30 个——真机首 ACK 可达 17.6s；预热后 2 个保实时）。
//! - 无主 tool_result 会杀会话：只回填模型真的调用过的 tool_call_id。
//! - v4 committed 不带 input_id：用 `response.unit.started` 的
//!   unit_index -> input_id 映射补。
//! - keepalive：writer 空闲 20s 发 WS Ping（对齐 Python websockets 的
//!   ping_interval=20 / ping_timeout=20），pong 由 reader 侧 tungstenite
//!   自动应答；server 的 ping 也由 reader 自动回 pong。
//! - WS 选项：max 16MB、无压缩（permessage-deflate 在 A53 上压缩 200KB+ 的
//!   omni unit 帧会同步阻塞收发上百 ms——开摄像头后对话变卡的主因之一；
//!   JPEG/PCM 本就是高熵数据，压不动还白烧 CPU，tungstenite 不启用压缩）。
//!
//! 与 Python 的有意偏差：
//! - Python 单 asyncio 线程 + `call_soon_threadsafe`；这里是 reader/writer
//!   双线程 + crossbeam channel，listener 回调全部从 reader 线程发出
//!   （writer 仅在发送失败的罕见路径上直接触发 `on_error`/`on_state`）。
//! - Python `_lag_probe`（GIL 滞后探针）在无 GIL 的 Rust 线程模型下无意义，
//!   未移植（`Timing::loop_lag` 由 UI 侧使用）。
//! - 契约的 `SessionListener` 没有 `on_turn_end`/`detail` 参数：回合结束以
//!   `on_spoken_text("", final=true)` 表达；失败原因通过 `on_error("", reason)`
//!   补给上层（服务端 error 帧则带真实 code/message）。
//! - `O5Session::new` 直接吃 `&Config`：URL mode 参数、音色、init_payload
//!   等全部从配置取（等价 Python main.py 的装配），tools JSON 由 app 透传。

mod deploy_probe;
mod listener;
mod session;
mod tls;

pub use listener::{SessionListener, SessionState};
pub use session::O5Session;

/// 首个 committed ACK 之前允许的在途 unit（真机首 ACK 可达 17.6s）。
pub const COLD_INFLIGHT_LIMIT: usize = 30;
/// 预热后：单 unit 在途 + 1 个排队。
pub const WARM_INFLIGHT_LIMIT: usize = 2;
/// 发送缓冲上限，超出丢最旧保实时。
pub const OUTBOX_CAP: usize = 30;
/// started_map（unit_index -> input_id）容量上限。
pub const STARTED_MAP_CAP: usize = 64;

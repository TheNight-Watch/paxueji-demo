//! 会话状态与监听器回调接口（契约 `o5-session` 一节）。

/// 会话状态机（Python `STATES` 的强类型版）。
///
/// `idle -> connecting -> waiting -> initializing -> ready -> closing -> closed`，
/// 任意阶段出错 -> `failed`（一次性会话，不自动重连）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum SessionState {
    /// 尚未 `start()`。
    Idle,
    /// TCP/TLS/WS 握手中。
    Connecting,
    /// 服务端排队中（session.queued / session.queue_update）。
    Waiting,
    /// session.init 已发，等 session.created。
    Initializing,
    /// 会话就绪，可投递音频 unit。
    Ready,
    /// 优雅关闭中（session.close 已发或强断进行中）。
    Closing,
    /// 正常关闭完成。
    Closed,
    /// 失败（一次性语义，不重连）。
    Failed,
}

/// 回调接口；全部在 session 的 reader 线程里调用，**必须快**且线程安全
/// （`on_audio` 只做入队投递，绝不能阻塞——音频时钟红线）。
///
/// 默认实现全部为空操作（对齐 Python `SessionListener` 基类）。
pub trait SessionListener: Send + Sync {
    /// 状态机迁移（每次 `_set_state` 都触发，含重复状态，如多条排队更新）。
    fn on_state(&self, _s: SessionState) {}
    /// 排队位置（session.queue_update 的 position）。
    fn on_queue_position(&self, _pos: i64) {}
    /// 下行 TTS 音频（pcm16 LE + 采样率，帧内 sample_rate 优先）。
    fn on_audio(&self, _pcm16: Vec<u8>, _rate: u32) {}
    /// 口播文本增量；`final_=true` 表示回合结束（spoken.end reason=turn_eos，
    /// 对应 Python 的 `on_turn_end`）。
    fn on_spoken_text(&self, _text: &str, _final_: bool) {}
    /// 思考流文本（response.think.delta）。
    fn on_think_text(&self, _text: &str) {}
    /// 用户打断让位（Rust 版新增信号）：口播回合没有 turn_eos 就直接转
    /// `spoken.end reason=listen` = 被插话后让出话轮（cpm-harness 口径）。
    /// 此刻服务器已停发旧口播，调用方应清播放积压实现快停。
    /// 判定条件是「listen **且** 口播中」：空闲期的 listen 只是每 unit 的
    /// 常规收据（每秒一条，不能当打断——曾因此每秒误清导致「说一半就
    /// 没了」）；口播中的 slot_eos/tts_pad 是 unit 内收束，同样不算
    /// （v5 透明池口播中每 unit 一条，曾误当回合结束导致「重复一句话」
    /// + aplay 重建爆音）。
    fn on_interrupted(&self) {}
    /// 完整工具调用（begin/delta 已拼装，done 缺 arguments 时用缓冲补）。
    fn on_tool_call(&self, _call: &o5_protocol::ParsedToolCall) {}
    /// 工具调用被放弃。
    fn on_tool_abort(&self, _tool_call_id: &str) {}
    /// 失败通知：服务端 error 帧带真实 code/message；内部失败（超时/IO）
    /// code 为空串、message 为失败原因（随后必有 `on_state(Failed)`）。
    fn on_error(&self, _code: &str, _message: &str) {}
    /// 会话正常关闭（state -> Closed 时触发；reason 来自 session.closed
    /// 帧，无帧时为「连接结束」）。
    fn on_closed(&self, _reason: &str) {}
}

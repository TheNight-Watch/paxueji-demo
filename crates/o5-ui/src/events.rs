//! UI 事件（跨 crate 契约；从 `Ui::run` 经 crossbeam channel 发出）。
//!
//! Python 版是 `{action: callable}` 回调表，映射关系：
//! - `session` → [`UiEvent::StartStop`]
//! - `mute` → [`UiEvent::Mute`]
//! - `mode` → [`UiEvent::Mode`]
//! - `menu`（设置页打开，仅通知）→ [`UiEvent::Menu`]
//! - `panel`（设置页网络/性能面板开关）→ [`UiEvent::PanelToggle`]
//! - `volume` → [`UiEvent::VolumeChanged`]（0-100）
//! - `quit` → [`UiEvent::CloseApp`]
//!
//! Python 的 `camera` / `network` 回调没有对应的契约变体：
//! 摄像头开关由 UI 直接翻转 [`crate::SharedUiState`] 的 `camera_on`
//! （Python 里也是在 UI 线程同步翻转后由回调停流），app 侧观察状态即可；
//! 网络切换（Python `ui_toggle_network`：改配置/hosts/重启会话）逻辑
//! 属 app，契约 `UiEvent` 无 `Network` 变体，故顶栏芯片保持纯显示；
//! Python `publicnet` 事件（公网调用开关）对应 `UiEvent::PublicNet`；
//! `UiEvent::AsrToggle`（ASR 转写开关）是 Rust 版独有（性能测试用）。

/// UI 事件（CONTRACTS.md 契约枚举；拍学机分叉新增 `Shutter`）。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum UiEvent {
    /// 开始/停止会话按钮。
    StartStop,
    /// 静音切换按钮。
    Mute,
    /// 模式切换按钮（audio ↔ omni/video）。
    Mode,
    /// 设置页打开（主界面「更多」按钮）。
    Menu,
    /// 网络/性能面板开关（设置页）。
    PanelToggle,
    /// 公网调用开关（设置页）：开=前台端点域名换公网网关 o5gw.modelbest.cn。
    /// 状态由 app 翻转后写回 `set_public_net`（改配置/重启会话属 app 职责）。
    PublicNet,
    /// ASR 转写开关（设置页）：关=完全不建 ASR 连接（性能测试用）。
    /// 状态由 app 翻转后写回 `set_asr_on`（启停 AsrClient 属 app 职责）。
    AsrToggle,
    /// 音量变化（0-100；音量条拖动 5% 步进 / ±10 按钮）。
    VolumeChanged(f32),
    /// 退出 app（顶栏「退出」）。
    CloseApp,
    /// 拍学机快门：实时取景时定格并请模型讲解，定格中则回到取景。
    Shutter,
    /// 会话结果（test-pc/无人值守路径上报；UI 自身不产生）。
    SessionResult(bool),
}

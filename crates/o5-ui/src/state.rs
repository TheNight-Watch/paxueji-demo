//! UI 共享状态（Python `main.py` `AppState` 的移植；UI 线程只读、app 线程写）。
//!
//! Python 版靠 GIL 直接读写裸字段；Rust 版收进一把 `RwLock`：
//! app 侧用各 set/add 方法写，UI 线程每帧 [`SharedUiState::snapshot`]
//! 拿一份_owned_视图（相机预览是 `Arc` 浅拷贝，921KB 帧零拷贝）。

use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::image::{Color, Image};
use crate::layout::{COLOR_ACCENT, COLOR_DIM, COLOR_ERR, COLOR_OK, COLOR_WARN};

/// 转写条目类型（Python "model" | "sys" | "user"）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TranscriptKind {
    Model,
    Sys,
    User,
}

impl TranscriptKind {
    /// 字幕颜色：用户=绿（tts-center 转写），系统=蓝，模型=白。
    pub fn color(self) -> Color {
        match self {
            TranscriptKind::Sys => COLOR_ACCENT,
            TranscriptKind::User => COLOR_OK,
            TranscriptKind::Model => crate::layout::COLOR_TEXT,
        }
    }
}

/// 一条转写（Python `(text, kind)` 元组）。
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptEntry {
    pub text: String,
    pub kind: TranscriptKind,
}

/// 会话状态（Python `session_state` 字符串的强类型化；取值与文案一一对应）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SessionPhase {
    #[default]
    Idle,
    Connecting,
    Waiting,
    Initializing,
    Ready,
    Closing,
    Closed,
    Failed,
}

impl SessionPhase {
    /// Python 字符串原值（`STATE_ZH` 的键）。
    pub fn as_str(self) -> &'static str {
        match self {
            SessionPhase::Idle => "idle",
            SessionPhase::Connecting => "connecting",
            SessionPhase::Waiting => "waiting",
            SessionPhase::Initializing => "initializing",
            SessionPhase::Ready => "ready",
            SessionPhase::Closing => "closing",
            SessionPhase::Closed => "closed",
            SessionPhase::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> SessionPhase {
        match s {
            "connecting" => SessionPhase::Connecting,
            "waiting" => SessionPhase::Waiting,
            "initializing" => SessionPhase::Initializing,
            "ready" => SessionPhase::Ready,
            "closing" => SessionPhase::Closing,
            "closed" => SessionPhase::Closed,
            "failed" => SessionPhase::Failed,
            _ => SessionPhase::Idle,
        }
    }

    /// 是否处于「会话进行中」（Python 按钮/标签判定集合：
    /// connecting/waiting/initializing/ready/closing）。
    pub fn running(self) -> bool {
        matches!(
            self,
            SessionPhase::Connecting
                | SessionPhase::Waiting
                | SessionPhase::Initializing
                | SessionPhase::Ready
                | SessionPhase::Closing
        )
    }

    /// 中文文案（STATE_ZH）。
    pub fn zh(self) -> &'static str {
        match self {
            SessionPhase::Idle => "未连接",
            SessionPhase::Connecting => "连接中",
            SessionPhase::Waiting => "排队中",
            SessionPhase::Initializing => "建立会话",
            SessionPhase::Ready => "在听",
            SessionPhase::Closing => "关闭中",
            SessionPhase::Closed => "点我重连",
            SessionPhase::Failed => "点我重连",
        }
    }

    /// 状态章颜色（STATE_ZH）。
    pub fn color(self) -> Color {
        match self {
            SessionPhase::Idle => COLOR_DIM,
            SessionPhase::Connecting
            | SessionPhase::Waiting
            | SessionPhase::Initializing
            | SessionPhase::Closing => COLOR_WARN,
            SessionPhase::Ready => COLOR_OK,
            SessionPhase::Closed => COLOR_DIM,
            SessionPhase::Failed => COLOR_ERR,
        }
    }
}

/// 会话状态文案兜底（未知状态时 Python 用原串 + WARN 色）。
pub fn state_label(phase: SessionPhase) -> (&'static str, Color) {
    (phase.zh(), phase.color())
}

/// 转写窗口容量（Python `_trim`：> 80 截到 80）。
pub const TRANSCRIPT_CAP: usize = 80;

/// UI 渲染一份快照看到的全部状态（owned；相机预览为 `Arc` 浅拷贝）。
#[derive(Clone, Debug)]
pub struct UiView {
    pub session_state: SessionPhase,
    pub session_detail: String,
    pub transcript: Vec<TranscriptEntry>,
    pub think_active: bool,
    pub background_line: String,
    /// "audio" | "omni" | "video"（照 Python mode 字符串）。
    pub mode: String,
    pub camera_on: bool,
    pub muted: bool,
    /// 右上角网络/性能面板开关。
    pub perf_panel: bool,
    pub mic_level_db: f64,
    /// 线性 RMS（0~1），指标区展示原始值。
    pub mic_rms: f64,
    /// UI 直接画的摄像头预览（640x480 RGB888）。
    pub cam_preview: Option<Arc<Image>>,
    /// "wifi" 直连 | "usb" 中继。
    pub network_mode: String,
    /// 公网调用开关（前台端点域名换公网网关 o5gw.modelbest.cn）。
    pub public_net: bool,
    /// ASR 转写开关（关=完全不建连接，性能测试用）。
    pub asr_on: bool,
    /// 设备编号徽章（config device.label，如 "03"）。
    pub device_label: String,
    /// 扬声器音量 0-100（二级菜单调节）。
    pub volume: u8,
    /// 拍学机：快门定格的画面（Some 时代替实时预览铺满屏幕）。
    pub frozen: Option<Arc<Image>>,
    /// 拍学机：模型通过 show_card 推上屏的知识卡片。
    pub card: Option<KnowledgeCard>,
    /// 拍学机：联网查询状态芯片（文案 + 截止时刻；None 不显示）。
    pub tool_chip: Option<(String, Instant)>,
}

/// 拍学机知识卡片（show_card 工具参数）。
#[derive(Clone, Debug, PartialEq)]
pub struct KnowledgeCard {
    pub title: String,
    pub points: Vec<String>,
    pub source: Option<String>,
}

#[derive(Debug)]
struct UiStateInner {
    session_state: SessionPhase,
    session_detail: String,
    transcript: VecDeque<TranscriptEntry>,
    line_open: bool,       // 当前口播行是否未收束
    user_line_open: bool,  // 当前用户转写行是否未收束
    think_active: bool,
    background_line: String,
    mode: String,
    camera_on: bool,
    muted: bool,
    perf_panel: bool,
    mic_level_db: f64,
    mic_rms: f64,
    cam_preview: Option<Arc<Image>>,
    network_mode: String,
    public_net: bool,
    asr_on: bool,
    device_label: String,
    volume: u8,
    frozen: Option<Arc<Image>>,
    card: Option<KnowledgeCard>,
    tool_chip: Option<(String, Instant)>,
}

impl Default for UiStateInner {
    fn default() -> Self {
        UiStateInner {
            session_state: SessionPhase::Idle,
            session_detail: String::new(),
            transcript: VecDeque::new(),
            line_open: false,
            user_line_open: false,
            think_active: false,
            background_line: "后台: 未配置".to_string(),
            mode: "audio".to_string(),
            camera_on: true,
            muted: false,
            perf_panel: false,
            mic_level_db: -90.0,
            mic_rms: 0.0,
            cam_preview: None,
            network_mode: "wifi".to_string(),
            public_net: false,
            asr_on: true,
            device_label: String::new(),
            volume: 80,
            frozen: None,
            card: None,
            tool_chip: None,
        }
    }
}

/// 跨线程共享的 UI 状态集（`RwLock<UiStateInner>` 的句柄，`Clone` 即共享）。
#[derive(Clone, Default)]
pub struct SharedUiState {
    inner: Arc<RwLock<UiStateInner>>,
}

fn lock_read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    // 锁中毒不致命：沿用持锁者崩溃前的状态
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn lock_write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

impl SharedUiState {
    pub fn new() -> SharedUiState {
        SharedUiState::default()
    }

    // —— app 侧写 --------------------------------------------------------

    pub fn set_session(&self, s: SessionPhase, detail: &str) {
        let mut st = lock_write(&self.inner);
        st.session_state = s;
        if !detail.is_empty() {
            st.session_detail = detail.to_string();
        }
    }

    pub fn set_session_state(&self, s: SessionPhase) {
        lock_write(&self.inner).session_state = s;
    }

    pub fn set_session_detail(&self, d: &str) {
        lock_write(&self.inner).session_detail = d.to_string();
    }

    /// 系统消息（模式切换提示等）：关闭上一条口播行后新增 sys 条目。
    pub fn add_sys(&self, text: &str) {
        let mut st = lock_write(&self.inner);
        st.line_open = false;
        st.user_line_open = false;
        st.transcript.push_back(TranscriptEntry {
            text: text.to_string(),
            kind: TranscriptKind::Sys,
        });
        trim(&mut st.transcript);
    }

    /// 模型口播增量：上一条未收束时原位拼接。
    pub fn add_model(&self, text: &str) {
        let mut st = lock_write(&self.inner);
        if st.line_open {
            if let Some(last) = st.transcript.back_mut() {
                last.text.push_str(text);
                trim(&mut st.transcript);
                return;
            }
        }
        st.transcript.push_back(TranscriptEntry {
            text: text.to_string(),
            kind: TranscriptKind::Model,
        });
        st.line_open = true;
        trim(&mut st.transcript);
    }

    pub fn close_model_line(&self) {
        lock_write(&self.inner).line_open = false;
    }

    /// tts-center ASR 转写：delta 是整句（覆盖式），done 是定稿。
    pub fn add_user(&self, text: &str, final_: bool) {
        if text.is_empty() {
            return;
        }
        let mut st = lock_write(&self.inner);
        if st.user_line_open && st.transcript.back().map(|e| e.kind) == Some(TranscriptKind::User) {
            st.transcript.back_mut().unwrap().text = text.to_string();
        } else {
            st.transcript.push_back(TranscriptEntry {
                text: text.to_string(),
                kind: TranscriptKind::User,
            });
        }
        st.user_line_open = !final_;
        trim(&mut st.transcript);
    }

    pub fn set_think_active(&self, b: bool) {
        lock_write(&self.inner).think_active = b;
    }

    pub fn set_background_line(&self, s: &str) {
        lock_write(&self.inner).background_line = s.to_string();
    }

    pub fn set_mode(&self, m: &str) {
        lock_write(&self.inner).mode = m.to_string();
    }

    pub fn set_camera_on(&self, b: bool) {
        lock_write(&self.inner).camera_on = b;
    }

    /// 翻转摄像头开关（设置页 omni 项；UI 侧直接改，返回新值）。
    pub fn toggle_camera_on(&self) -> bool {
        let mut st = lock_write(&self.inner);
        st.camera_on = !st.camera_on;
        st.camera_on
    }

    /// 摄像头开关当前值（app 侧观察启停；相比 snapshot 不拷贝转写）。
    pub fn camera_on(&self) -> bool {
        lock_read(&self.inner).camera_on
    }

    pub fn set_muted(&self, b: bool) {
        lock_write(&self.inner).muted = b;
    }

    pub fn set_mic_level(&self, db: f64, rms: f64) {
        let mut st = lock_write(&self.inner);
        st.mic_level_db = db;
        st.mic_rms = rms;
    }

    pub fn set_cam_preview(&self, img: Option<Image>) {
        let mut st = lock_write(&self.inner);
        st.cam_preview = img.map(Arc::new);
    }

    pub fn set_network_mode(&self, m: &str) {
        lock_write(&self.inner).network_mode = m.to_string();
    }

    /// 公网调用开关（app 在 `UiEvent::PublicNet` 处理器里翻转后写回）。
    pub fn set_public_net(&self, on: bool) {
        lock_write(&self.inner).public_net = on;
    }

    /// ASR 转写开关（app 在 `UiEvent::AsrToggle` 处理器里翻转后写回）。
    pub fn set_asr_on(&self, on: bool) {
        lock_write(&self.inner).asr_on = on;
    }

    pub fn set_device_label(&self, s: &str) {
        lock_write(&self.inner).device_label = s.to_string();
    }

    pub fn set_volume(&self, v: u8) {
        lock_write(&self.inner).volume = v;
    }

    /// 设置页/拖动条读当前音量。
    pub fn volume(&self) -> u8 {
        lock_read(&self.inner).volume
    }

    /// 拍学机：定格 / 解除定格（解除时一并收起卡片）。
    pub fn set_frozen(&self, img: Option<Arc<Image>>) {
        let mut st = lock_write(&self.inner);
        st.frozen = img;
        st.card = None;
    }

    pub fn is_frozen(&self) -> bool {
        lock_read(&self.inner).frozen.is_some()
    }

    pub fn set_card(&self, card: Option<KnowledgeCard>) {
        lock_write(&self.inner).card = card;
    }

    /// 查询状态芯片：显示 `secs` 秒后自动消失。
    pub fn set_tool_chip(&self, text: &str, secs: f64) {
        lock_write(&self.inner).tool_chip =
            Some((text.to_string(), Instant::now() + Duration::from_secs_f64(secs)));
    }

    /// 相机预览是否活跃（15fps 节拍判定；只读不拷贝）。
    pub fn has_cam_preview(&self) -> bool {
        lock_read(&self.inner).cam_preview.is_some()
    }

    // —— UI 侧读 ---------------------------------------------------------

    /// 一帧渲染所需的完整快照（owned；transcript 克隆、预览浅拷贝）。
    pub fn snapshot(&self) -> UiView {
        let st = lock_read(&self.inner);
        UiView {
            session_state: st.session_state,
            session_detail: st.session_detail.clone(),
            transcript: st.transcript.iter().cloned().collect(),
            think_active: st.think_active,
            background_line: st.background_line.clone(),
            mode: st.mode.clone(),
            camera_on: st.camera_on,
            muted: st.muted,
            perf_panel: st.perf_panel,
            mic_level_db: st.mic_level_db,
            mic_rms: st.mic_rms,
            cam_preview: st.cam_preview.clone(),
            network_mode: st.network_mode.clone(),
            public_net: st.public_net,
            asr_on: st.asr_on,
            device_label: st.device_label.clone(),
            volume: st.volume,
            frozen: st.frozen.clone(),
            card: st.card.clone(),
            tool_chip: st.tool_chip.clone(),
        }
    }

    /// 翻转网络/性能面板开关（设置页 perf 项；返回新值）。
    pub fn toggle_perf_panel(&self) -> bool {
        let mut st = lock_write(&self.inner);
        st.perf_panel = !st.perf_panel;
        st.perf_panel
    }
}

fn trim(q: &mut VecDeque<TranscriptEntry>) {
    while q.len() > TRANSCRIPT_CAP {
        q.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_phase_roundtrip() {
        for p in [
            SessionPhase::Idle,
            SessionPhase::Connecting,
            SessionPhase::Waiting,
            SessionPhase::Initializing,
            SessionPhase::Ready,
            SessionPhase::Closing,
            SessionPhase::Closed,
            SessionPhase::Failed,
        ] {
            assert_eq!(SessionPhase::parse(p.as_str()), p);
        }
        assert_eq!(SessionPhase::parse("junk"), SessionPhase::Idle);
        assert!(SessionPhase::Ready.running());
        assert!(SessionPhase::Closing.running());
        assert!(!SessionPhase::Idle.running());
        assert!(!SessionPhase::Closed.running());
        assert_eq!(SessionPhase::Ready.zh(), "在听");
        assert_eq!(SessionPhase::Failed.zh(), "点我重连");
        assert_eq!(state_label(SessionPhase::Idle).0, "未连接");
    }

    #[test]
    fn transcript_model_merging() {
        let s = SharedUiState::new();
        s.add_model("你");
        s.add_model("好");
        let v = s.snapshot();
        assert_eq!(v.transcript.len(), 1);
        assert_eq!(v.transcript[0].text, "你好");
        assert_eq!(v.transcript[0].kind, TranscriptKind::Model);
        s.close_model_line();
        s.add_model("下句");
        assert_eq!(s.snapshot().transcript.len(), 2);
        // add_sys 关闭未收束行并新增 sys 条目
        s.add_sys("── 切换 ──");
        let v = s.snapshot();
        assert_eq!(v.transcript.len(), 3);
        assert_eq!(v.transcript[2].kind, TranscriptKind::Sys);
        s.add_model("再接");
        assert_eq!(s.snapshot().transcript.len(), 4);
    }

    #[test]
    fn transcript_user_override() {
        let s = SharedUiState::new();
        s.add_user("你在", false);
        s.add_user("你在说什", false);
        let v = s.snapshot();
        assert_eq!(v.transcript.len(), 1);
        assert_eq!(v.transcript[0].text, "你在说什");
        s.add_user("你在说什么", true);
        let v = s.snapshot();
        assert_eq!(v.transcript.len(), 1);
        assert_eq!(v.transcript[0].text, "你在说什么");
        // 定稿后新起一条
        s.add_user("新问题", false);
        assert_eq!(s.snapshot().transcript.len(), 2);
        // 空文本忽略
        s.add_user("", true);
        assert_eq!(s.snapshot().transcript.len(), 2);
        // model 打开中的行后 user 另起
        let s2 = SharedUiState::new();
        s2.add_model("回答");
        s2.add_user("插话", false);
        let v = s2.snapshot();
        assert_eq!(v.transcript.len(), 2);
        assert_eq!(v.transcript[1].kind, TranscriptKind::User);
    }

    #[test]
    fn transcript_cap_80() {
        let s = SharedUiState::new();
        for i in 0..100 {
            s.add_sys(&format!("行 {}", i));
        }
        let v = s.snapshot();
        assert_eq!(v.transcript.len(), 80);
        assert_eq!(v.transcript[0].text, "行 20");
        assert_eq!(v.transcript[79].text, "行 99");
    }

    #[test]
    fn defaults_match_python() {
        let v = SharedUiState::new().snapshot();
        assert_eq!(v.mode, "audio");
        assert!(v.camera_on);
        assert!(!v.muted);
        assert!(!v.perf_panel);
        assert_eq!(v.mic_level_db, -90.0);
        assert_eq!(v.mic_rms, 0.0);
        assert_eq!(v.network_mode, "wifi");
        assert_eq!(v.background_line, "后台: 未配置");
        assert_eq!(v.volume, 80);
        assert!(v.cam_preview.is_none());
        assert_eq!(v.device_label, "");
    }

    #[test]
    fn setters_and_toggles() {
        let s = SharedUiState::new();
        s.set_muted(true);
        s.set_mode("omni");
        s.toggle_camera_on();
        s.toggle_perf_panel();
        s.set_volume(42);
        s.set_device_label("03");
        s.set_mic_level(-32.5, 0.017);
        let v = s.snapshot();
        assert!(v.muted);
        assert_eq!(v.mode, "omni");
        assert!(!v.camera_on);
        assert!(v.perf_panel);
        assert_eq!(v.volume, 42);
        assert_eq!(v.device_label, "03");
        assert_eq!(v.mic_level_db, -32.5);
        assert_eq!(s.volume(), 42);
        let img = Image::new(2, 2);
        s.set_cam_preview(Some(img));
        assert!(s.has_cam_preview());
        assert!(s.snapshot().cam_preview.is_some());
        s.set_cam_preview(None);
        assert!(!s.has_cam_preview());
    }
}

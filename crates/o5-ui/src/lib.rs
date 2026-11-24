//! `o5-ui`：MaixCAM2 触屏 UI（全软件渲染）。
//!
//! Python `o5/ui.py`（830 行）的 Rust 移植。布局、颜色、交互热区、
//! 绘制配方与 Python 逐项对齐；差异集中在基础设施（显示走 feeder
//! 子进程、触摸走 evdev、字形走 ab_glyph 光栅化 + FIFO 缓存）。
//!
//! # 结构
//! - [`image`]：RGB888 画布与绘制原语（矩形/贴图/压暗带/掩膜字形）
//! - [`text`]：显示宽度估算与 CJK 宽度贪婪折行
//! - [`font`]：字体加载（设备 otf → exe assets → ASCII 点阵回退）+ 字形缓存
//! - [`layout`]：常量（与 ui.py 39-49 逐项对齐）、按钮/热区几何、配色
//! - [`state`]：[`SharedUiState`]（app 线程写 / UI 线程读）
//! - [`events`]：[`UiEvent`]（契约枚举，经 crossbeam channel 出）
//! - [`touch`]：触摸源抽象（linux evdev MT type-B + 8 变体坐标映射；PC mock）
//! - [`display`]：显示后端（设备 feeder 子进程 / Null / PNG 转储）
//! - [`render`]：页面绘制（主界面/设置页/信息卡/性能面板）
//! - [`ui`]：[`Ui`] 主循环（50Hz 触摸轮询 + 15fps 浮点节拍重绘）
//!
//! # 用法
//! ```no_run
//! use o5_ui::{Ui, SharedUiState, UiEvent};
//! use o5_config::Config;
//! use o5_perf::Timing;
//! use std::sync::Arc;
//!
//! let cfg = Config::defaults();
//! let state = SharedUiState::new();
//! let timing = Arc::new(Timing::new());
//! let (tx, rx) = crossbeam_channel::unbounded::<UiEvent>();
//! std::thread::spawn(move || {
//!     let mut ui = Ui::new(&cfg);
//!     ui.run(state.clone(), timing, None, tx);
//! });
//! // app 线程：收 rx 事件、写 state
//! ```

pub mod display;
pub mod events;
pub mod font;
pub mod image;
pub mod key;
pub mod layout;
pub mod render;
pub mod state;
pub mod text;
pub mod touch;
pub mod ui;

mod png; // PNG 编码仅内部使用（PngDumpSink / 画布转储）

pub use display::{DisplaySink, NullSink, PngDumpSink};
#[cfg(target_os = "linux")]
pub use display::FeederSink;
pub use events::UiEvent;
pub use font::{font_candidates, FontSet};
pub use image::{ClipRect, Color, GlyphBitmap, Image};
pub use layout::{
    Button, ButtonAction, BTN_H, BTN_TOP, FONT_BODY, FONT_TITLE, METRIC1_Y, METRIC2_Y, SCREEN_H,
    SCREEN_W, TOPBAR_H, TRANSCRIPT_BOTTOM,
};
pub use render::{DeviceInfo, Interaction, Renderer, TouchTarget};
pub use state::{
    KnowledgeCard, SessionPhase, SharedUiState, TranscriptEntry, TranscriptKind, UiView,
    TRANSCRIPT_CAP,
};
pub use touch::{TouchSample, TouchSource};
pub use ui::Ui;

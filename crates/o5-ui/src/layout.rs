//! 布局常量与命中区（Python `ui.py` 39-68 逐字移植）。
//!
//! 布局（640x480）：
//!
//! ```text
//! +----------------------------------------------------+
//! | O5 语音助手   [●就绪]                       [退出] |  顶栏 52
//! |----------------------------------------------------|
//! |                                                    |
//! |   对话字幕（自动换行、滚动显示最近几行）             |  56..300
//! |                                                    |
//! |----------------------------------------------------|
//! | 状态: 就绪 / 排队 #2 / 正在关闭                     |  302..336
//! | ACK 1024ms  单元 42  麦克风 ▓▓▓░░  后台 x1           |  336..372
//! |----------------------------------------------------|
//! | [ 开始 ] [静音:关] [模式:语音] [ 更多 ]           |  376..460 按钮区
//! +----------------------------------------------------+
//! ```

use crate::image::Color;

/// 屏幕逻辑分辨率（横屏）。
pub const SCREEN_W: i32 = 640;
pub const SCREEN_H: i32 = 480;

/// 布局常量（640x480 屏幕）。
pub const TOPBAR_H: i32 = 52;
pub const TRANSCRIPT_TOP: i32 = TOPBAR_H + 4;
pub const TRANSCRIPT_BOTTOM: i32 = 300;
pub const METRIC1_Y: i32 = 302;
pub const METRIC2_Y: i32 = 336;
pub const BTN_TOP: i32 = 376;
pub const BTN_H: i32 = 84;
pub const BTN_MARGIN_X: i32 = 14;
pub const BTN_GAP: i32 = 12;
pub const FONT_BODY: u16 = 20;
pub const FONT_TITLE: u16 = 26;

// 二级菜单（设置页）布局：全屏覆盖主界面
pub const MENU_BACK: (i32, i32, i32, i32) = (14, 8, 126, 36); // 返回
pub const MENU_MINUS: (i32, i32, i32, i32) = (20, 140, 80, 70); // 音量 -
pub const MENU_BAR_X: i32 = 120;
pub const MENU_BAR_Y: i32 = 160;
pub const MENU_BAR_W: i32 = 340;
pub const MENU_BAR_H: i32 = 30;
/// 音量条命中区（比可视区域大）。
pub const MENU_BAR_HIT: (i32, i32, i32, i32) = (110, 135, 360, 80);
pub const MENU_PLUS: (i32, i32, i32, i32) = (480, 140, 80, 70); // 音量 +
pub const MENU_OMNICAM: (i32, i32, i32, i32) = (20, 236, 600, 58); // omni模式关闭摄像头开关
pub const MENU_PERF: (i32, i32, i32, i32) = (20, 302, 600, 58); // 网络/性能面板开关
pub const MENU_PUBNET: (i32, i32, i32, i32) = (20, 368, 600, 58); // 公网调用开关（前台端点域名换公网网关）
pub const MENU_ASR: (i32, i32, i32, i32) = (20, 434, 600, 44); // ASR 转写开关（关=不建连接，性能测试用）
/// 右上角网络/性能面板（开启时盖住指标条）。
pub const PANEL: (i32, i32, i32, i32) = (340, 58, 300, 296);

/// 面板超标阈值（ms）：last 或 均值任一超限即标红。
pub fn panel_threshold(key: &str) -> f64 {
    match key {
        // 纯网络 >120ms = 网络差（正常 20~45）
        "net_rtt" => 120.0,
        // 服务端确认飙高（正常 ~400）
        "ack_rtt" => 900.0,
        // 上行发送阻塞（正常 12~26）
        "capture_to_send" => 120.0,
        // 模型响应 >2s = 反应慢
        "response" => 2000.0,
        // 播放缓冲（正常 150~400）
        "recv_to_play" => 600.0,
        _ => f64::INFINITY,
    }
}

/// 顶栏设备编号徽章 x（标题右侧，高度同其他芯片）。
pub const DEV_BADGE_X: i32 = 168;
/// 徽章命中区（含余量；点击弹设备信息卡）。
pub const DEV_BADGE_HIT: (i32, i32, i32, i32) = (160, 0, 120, 40);

/// 拍学机配色与区域：橙色主色、奶油卡片、字幕带在按钮区上方。
pub const PAXUEJI_ORANGE: Color = Color(240, 140, 40);
pub const CARD_BG: Color = Color(255, 248, 232);
pub const CARD_TEXT: Color = Color(52, 44, 36);
pub const CARD_RECT: (i32, i32, i32) = (336, TOPBAR_H + 8, 292);
pub const SUBTITLE_TOP: i32 = 282;
pub const SUBTITLE_BOTTOM: i32 = BTN_TOP - 6;
pub const SHUTTER_FONT: u16 = 34;

pub const COLOR_BG: Color = Color(24, 24, 28);
pub const COLOR_PANEL: Color = Color(34, 36, 44);
pub const COLOR_TEXT: Color = Color(235, 235, 240);
pub const COLOR_DIM: Color = Color(150, 152, 160);
pub const COLOR_ACCENT: Color = Color(64, 156, 255);
pub const COLOR_OK: Color = Color(72, 199, 118);
pub const COLOR_WARN: Color = Color(240, 180, 60);
pub const COLOR_ERR: Color = Color(235, 92, 92);
pub const COLOR_MUTED: Color = Color(110, 112, 120);
/// 指标条压暗色（半透明底条用）。
pub const COLOR_STRIP: Color = Color(12, 12, 16);

/// 按钮描述（几何 + 动作名）。
#[derive(Clone, Copy, Debug)]
pub struct Button {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub action: ButtonAction,
}

/// 拍学机主界面按钮动作：更多 / 快门 / 静音（会话自动建立，模式固定全模态）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonAction {
    Menu,
    Shutter,
    Mute,
}

impl Button {
    pub fn hit(&self, x: i32, y: i32) -> bool {
        self.x <= x && x < self.x + self.w && self.y <= y && y < self.y + self.h
    }
}

/// 拍学机按钮几何：左「更多」、中间加宽的快门、右「静音」。
pub const SIDE_BTN_W: i32 = 120;
pub const SHUTTER_W: i32 = 220;

pub fn buttons(screen_w: i32) -> [Button; 3] {
    let side = |x, action| Button { x, y: BTN_TOP, w: SIDE_BTN_W, h: BTN_H, action };
    [
        side(BTN_MARGIN_X, ButtonAction::Menu),
        Button {
            x: (screen_w - SHUTTER_W) / 2,
            y: BTN_TOP,
            w: SHUTTER_W,
            h: BTN_H,
            action: ButtonAction::Shutter,
        },
        side(screen_w - BTN_MARGIN_X - SIDE_BTN_W, ButtonAction::Mute),
    ]
}

/// 顶栏状态章 x（104 宽；会话没在跑时点它重连）。
pub fn status_chip_x(screen_w: i32) -> i32 {
    screen_w - 192
}

pub fn in_status_zone(x: i32, y: i32, screen_w: i32) -> bool {
    let sx = status_chip_x(screen_w);
    (sx..=sx + 104).contains(&x) && (8..=TOPBAR_H - 8).contains(&y)
}

/// 坐标是否落在 (x, y, w, h) 矩形内（Python `_in`）。
pub fn in_zone(x: i32, y: i32, zone: (i32, i32, i32, i32)) -> bool {
    let (zx, zy, zw, zh) = zone;
    zx <= x && x < zx + zw && zy <= y && y < zy + zh
}

/// 顶栏退出热区（Python `_on_press`：`w-70 <= x <= w-8` 且 `8 <= y <= TOPBAR_H-8`）。
pub fn in_quit_zone(x: i32, y: i32, screen_w: i32) -> bool {
    (screen_w - 70..=screen_w - 8).contains(&x) && (8..=TOPBAR_H - 8).contains(&y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_geometry() {
        let [menu, shutter, mute] = buttons(SCREEN_W);
        assert_eq!((menu.x, menu.w, menu.action), (14, 120, ButtonAction::Menu));
        assert_eq!((shutter.x, shutter.w, shutter.action), (210, 220, ButtonAction::Shutter));
        assert_eq!((mute.x, mute.w, mute.action), (506, 120, ButtonAction::Mute));
        assert!(shutter.hit(320, 400));
        assert!(!shutter.hit(209, 400));
        assert!(!menu.hit(320, 400));
    }

    #[test]
    fn zones() {
        assert!(in_zone(14, 8, MENU_BACK));
        assert!(!in_zone(13, 8, MENU_BACK));
        assert!(!in_zone(140, 8, MENU_BACK));
        // 音量条命中区大于可视区域
        assert!(in_zone(115, 140, MENU_BAR_HIT));
        assert!(!in_zone(115, 140, (MENU_BAR_X, MENU_BAR_Y, MENU_BAR_W, MENU_BAR_H)));
        // 顶栏热区（闭区间语义）
        assert!(in_quit_zone(570, 8, SCREEN_W));
        assert!(in_quit_zone(632, 44, SCREEN_W));
        assert!(!in_quit_zone(569, 8, SCREEN_W));
        assert!(!in_quit_zone(633, 8, SCREEN_W));
        assert!(!in_quit_zone(600, 7, SCREEN_W));
        assert!(!in_quit_zone(600, 45, SCREEN_W));
        assert!(in_status_zone(448, 8, SCREEN_W));
        assert!(in_status_zone(552, 44, SCREEN_W));
        assert!(!in_status_zone(447, 8, SCREEN_W));
        // 设备徽章命中区
        assert!(in_zone(160, 0, DEV_BADGE_HIT));
        assert!(in_zone(279, 39, DEV_BADGE_HIT));
        assert!(!in_zone(280, 20, DEV_BADGE_HIT));
    }

    #[test]
    fn panel_thresholds() {
        assert_eq!(panel_threshold("net_rtt"), 120.0);
        assert_eq!(panel_threshold("ack_rtt"), 900.0);
        assert_eq!(panel_threshold("response"), 2000.0);
        assert_eq!(panel_threshold("unknown"), f64::INFINITY);
    }

    #[test]
    fn layout_constants_unchanged() {
        // 与 ui.py 39-49 逐字对齐（防止意外改动布局）
        assert_eq!(TOPBAR_H, 52);
        assert_eq!(TRANSCRIPT_TOP, 56);
        assert_eq!(TRANSCRIPT_BOTTOM, 300);
        assert_eq!(METRIC1_Y, 302);
        assert_eq!(METRIC2_Y, 336);
        assert_eq!(BTN_TOP, 376);
        assert_eq!(BTN_H, 84);
        assert_eq!(BTN_MARGIN_X, 14);
        assert_eq!(BTN_GAP, 12);
        assert_eq!(FONT_BODY, 20);
        assert_eq!(FONT_TITLE, 26);
    }
}

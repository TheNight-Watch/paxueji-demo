//! 整帧渲染器（Python `ui.py` `_draw_*` 全系的逐行移植，纯软件渲染）。
//!
//! 画布跨帧复用（Python 版同样不每帧清屏：主页面整屏铺摄像头或底色、
//! 设置页整屏底色）。文本不再走 Python 的「芯片缓存」（draw_string 光栅化
//! 每帧 ~35ms 才需要的优化）——Rust 光栅化单字形后缓存，blit 亚毫秒。

use std::time::Instant;

use o5_perf::{Metric, PerfSnapshot, TimingSnapshot};

use crate::image::{ClipRect, Color, Image};
use crate::layout::*;
use crate::state::{state_label, KnowledgeCard, TranscriptKind, UiView};
use crate::text::{display_units, fit_units, wrap_text};
use crate::font::FontSet;

/// 设备标识（点徽章弹信息卡）：wlan MAC + 主机名（设备侧读，无需配置）。
#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    pub mac: String,
    pub host: String,
}

/// 读设备标识（文件不存在/PC 上为空串，不报错）。
pub fn read_device_info() -> DeviceInfo {
    DeviceInfo {
        mac: std::fs::read_to_string("/sys/class/net/wlan0/address")
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
        host: std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
    }
}

/// 触摸按压目标（沿检测期间的高亮状态 + 释放判定）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TouchTarget {
    /// 主界面第 n 个按钮。
    Btn(usize),
    MenuBack,
    MenuMinus,
    MenuBar,
    MenuPlus,
    MenuOmnicam,
    MenuPerf,
    MenuAsr,
    Quit,
    Status,
    DevInfo,
}

/// 交互态（触摸按压高亮 + 设置页开关 + 信息卡计时）。
#[derive(Default)]
pub struct Interaction {
    /// 二级菜单（设置页）打开时主界面不响应触摸。
    pub menu_open: bool,
    pub press_target: Option<TouchTarget>,
    /// 设备信息卡显示截止时刻。
    pub info_until: Option<Instant>,
}

/// 整帧渲染器。
pub struct Renderer {
    pub canvas: Image,
    pub fonts: FontSet,
    pub screen_w: i32,
    pub screen_h: i32,
    buttons: Vec<Button>,
    pub device: DeviceInfo,
}

impl Renderer {
    pub fn new(screen_w: i32, screen_h: i32, fonts: FontSet, device: DeviceInfo) -> Renderer {
        Renderer {
            canvas: Image::new(screen_w as u32, screen_h as u32),
            fonts,
            screen_w,
            screen_h,
            buttons: buttons(screen_w).to_vec(),
            device,
        }
    }

    // ------------------------------------------------------------------
    // 文本（`_blit_text` 的直接绘制版）
    // ------------------------------------------------------------------

    /// 画文本：`bg=Some` 时先铺实底芯片矩形（按钮/设置页等不透光控件），
    /// `bg=None` 为透明底文字（掩膜透明写，底透出被压暗的摄像头画面）。
    /// 芯片矩形尺寸 = Python 版的 est_w（显示宽度估算 + 12）× (字号 + 8)，
    /// 字形超出芯片部分丢弃（对齐 Python 芯片/掩膜裁剪语义）。
    fn blit_text(&mut self, x: i32, y: i32, text: &str, fg: Color, size: u16, bg: Option<Color>) {
        let est_w = (display_units(text) * size as f64) as i32 + 12;
        let w = est_w.min(self.screen_w - x);
        let hh = (size as i32 + 8).min(self.screen_h - y);
        if w <= 0 || hh <= 0 {
            return;
        }
        if let Some(bg) = bg {
            self.canvas.fill_rect(x, y, w, hh, bg);
        }
        let clip = ClipRect::new(x, y, w, hh);
        self.fonts
            .draw_string(&mut self.canvas, x + 6, y + 4, text, fg, size, Some(clip));
    }

    // ------------------------------------------------------------------
    // 主页面
    // ------------------------------------------------------------------

    pub fn draw_main(&mut self, view: &UiView, inter: &Interaction, snap: &TimingSnapshot, perf: &PerfSnapshot) {
        // 背景：定格画面 > 实时取景 > 底色
        let full = |img: &Option<std::sync::Arc<Image>>| {
            img.as_ref()
                .filter(|c| c.width as i32 == self.screen_w && c.height as i32 == self.screen_h)
                .cloned()
        };
        match full(&view.frozen).or_else(|| full(&view.cam_preview)) {
            Some(bg) => self.canvas.draw_image(0, 0, &bg),
            None => self.canvas.fill_rect(0, 0, self.screen_w, self.screen_h, COLOR_BG),
        }

        self.canvas.blend_strip(0, 0, self.screen_w, TOPBAR_H, COLOR_PANEL);
        self.blit_text(12, 0, "拍学机", PAXUEJI_ORANGE, FONT_TITLE, None);
        let hint = if view.think_active {
            "小松果老师在想…"
        } else if view.frozen.is_some() {
            "可以随时问我问题哦"
        } else {
            "对准想认识的东西，按「拍」"
        };
        self.blit_text(12, 28, hint, COLOR_TEXT, 16, None);
        self.draw_dev_badge(view, inter);
        let (s_zh, s_color) = state_label(view.session_state);
        let sx = status_chip_x(self.screen_w);
        let s_bg = if inter.press_target == Some(TouchTarget::Status) {
            s_color.brighten(40)
        } else {
            s_color
        };
        self.canvas.fill_rect(sx, 12, 104, 30, s_bg);
        self.blit_text(sx + 10, 14, s_zh, Color(20, 20, 24), FONT_BODY, Some(s_bg));
        self.canvas
            .draw_rect(self.screen_w - 66, 8, 58, 36, COLOR_MUTED, 1);
        self.blit_text(self.screen_w - 54, 14, "退出", COLOR_TEXT, FONT_BODY, None);

        if let Some((text, until)) = &view.tool_chip {
            if Instant::now() < *until {
                self.draw_tool_chip(text);
            }
        }
        if view.perf_panel {
            self.draw_perf_panel(view, snap, perf);
        } else if let Some(card) = &view.card {
            self.draw_knowledge_card(card);
        }
        self.draw_transcript(SUBTITLE_TOP, SUBTITLE_BOTTOM, view);

        let labels = self.button_labels(view);
        let colors = self.button_colors(view, inter);
        let buttons: Vec<Button> = self.buttons.to_vec();
        for (i, b) in buttons.iter().enumerate() {
            let color = colors[i];
            let bg = if inter.press_target == Some(TouchTarget::Btn(i)) {
                color.brighten(40)
            } else {
                color
            };
            let size = if b.action == ButtonAction::Shutter { SHUTTER_FONT } else { FONT_TITLE };
            self.canvas.fill_rect(b.x, b.y, b.w, b.h, bg);
            self.canvas.draw_rect(b.x, b.y, b.w, b.h, color.brighten(60), 2);
            let tw = (display_units(labels[i]) * size as f64) as i32 + 12;
            self.blit_text(
                b.x + ((b.w - tw) / 2).max(4),
                b.y + (b.h - size as i32 - 8) / 2,
                labels[i],
                COLOR_TEXT,
                size,
                Some(bg),
            );
        }
    }

    /// 联网查询状态芯片（顶栏下方靠左，避开右侧知识卡片）。
    fn draw_tool_chip(&mut self, text: &str) {
        let w = (display_units(text) * FONT_BODY as f64) as i32 + 28;
        let x = 12;
        self.canvas.fill_rect(x, TOPBAR_H + 8, w, 32, COLOR_ACCENT);
        self.blit_text(x + 8, TOPBAR_H + 10, text, COLOR_TEXT, FONT_BODY, Some(COLOR_ACCENT));
    }

    /// 知识卡片（右侧奶油色面板）：标题 + 要点 + 来源，高度随内容、不越过字幕区。
    pub fn draw_knowledge_card(&mut self, card: &KnowledgeCard) {
        let (x, y, w) = CARD_RECT;
        let max_bottom = SUBTITLE_TOP - 8;
        let inner = w - 28;
        let title_lines = wrap_text(&card.title, inner as f64 / FONT_TITLE as f64);
        let mut rows: Vec<(String, u16, Color)> = title_lines
            .into_iter()
            .take(2)
            .map(|l| (l, FONT_TITLE, PAXUEJI_ORANGE))
            .collect();
        for p in card.points.iter().take(4) {
            for (i, l) in wrap_text(p, (inner - 16) as f64 / FONT_BODY as f64)
                .into_iter()
                .enumerate()
            {
                let prefix = if i == 0 { "· " } else { "  " };
                rows.push((format!("{prefix}{l}"), FONT_BODY, CARD_TEXT));
            }
        }
        if let Some(src) = card.source.as_deref().filter(|s| !s.is_empty()) {
            rows.push((fit_units(&format!("来源：{src}"), inner as f64 / 16.0), 16, COLOR_MUTED));
        }
        let row_h = |size: u16| size as i32 + 10;
        let mut h = 20;
        let mut fit = 0;
        for (_, size, _) in &rows {
            if y + h + row_h(*size) > max_bottom {
                break;
            }
            h += row_h(*size);
            fit += 1;
        }
        self.canvas.fill_rect(x, y, w, h, CARD_BG);
        self.canvas.draw_rect(x, y, w, h, PAXUEJI_ORANGE, 3);
        let mut ty = y + 10;
        for (text, size, color) in rows.into_iter().take(fit) {
            self.blit_text(x + 8, ty, &text, color, size, None);
            ty += row_h(size);
        }
    }

    /// 字幕区（`_draw_transcript`）：按显示宽度折行，滚动保留最近几行。
    fn draw_transcript(&mut self, top: i32, bottom: i32, view: &UiView) {
        let avail_w = self.screen_w - 28;
        let max_units = avail_w as f64 / FONT_BODY as f64;
        let line_h = FONT_BODY as i32 + 8;
        let max_lines = ((bottom - top - 6) / line_h).max(1);

        let entries: Vec<&crate::state::TranscriptEntry> =
            view.transcript.iter().rev().take(60).rev().collect();
        let mut lines: Vec<(String, TranscriptKind)> = Vec::new();
        for e in &entries {
            for l in wrap_text(&e.text, max_units) {
                lines.push((l, e.kind));
            }
        }
        let shown = &lines[lines.len().saturating_sub(max_lines as usize)..];
        let mut y = bottom - shown.len() as i32 * line_h;
        // 字幕带整体半透明压暗一次（替代逐行全黑芯片底），文字逐行掩膜写入
        if !shown.is_empty() {
            self.canvas.blend_strip(
                0,
                y - 3,
                self.screen_w,
                shown.len() as i32 * line_h + 5,
                COLOR_STRIP,
            );
        }
        for (text, kind) in shown {
            // 用户=绿（tts-center 转写），系统=蓝，模型=白
            self.blit_text(8, y - 2, text, kind.color(), FONT_BODY, None);
            y += line_h;
        }
    }

    fn button_labels(&self, view: &UiView) -> [&'static str; 3] {
        let shutter = if view.frozen.is_some() { "再拍一张" } else { "拍" };
        let mute = if view.muted { "已静音" } else { "静音" };
        ["更多", shutter, mute]
    }

    fn button_colors(&self, view: &UiView, inter: &Interaction) -> [Color; 3] {
        let menu = if inter.menu_open { COLOR_WARN } else { COLOR_MUTED };
        let shutter = if view.frozen.is_some() { COLOR_ACCENT } else { PAXUEJI_ORANGE };
        let mute = if view.muted { COLOR_WARN } else { COLOR_MUTED };
        [menu, shutter, mute]
    }

    // ------------------------------------------------------------------
    // 网络/性能面板（右上角，`_draw_perf_panel`）
    // ------------------------------------------------------------------

    fn draw_perf_panel(&mut self, view: &UiView, snap: &TimingSnapshot, perf: &PerfSnapshot) {
        self.canvas
            .blend_strip(PANEL.0, PANEL.1, PANEL.2, PANEL.3, COLOR_STRIP);
        self.canvas
            .draw_rect(PANEL.0, PANEL.1, PANEL.2, PANEL.3, COLOR_PANEL, 1);

        // (文本, 是否超标)：last(均avg) 取整 + ' ms'；超阈值标红
        let val = |m: &Option<Metric>, key: &str| -> (String, bool) {
            match m {
                None => ("-".to_string(), false),
                Some(m) => {
                    let txt = format!("{:.0}(均{:.0}) ms", m.last_ms, m.avg_ms);
                    let th = panel_threshold(key);
                    (txt, m.last_ms > th || m.avg_ms > th)
                }
            }
        };
        let net = val(&snap.net_rtt, "net_rtt");
        let ack = val(&snap.ack_rtt, "ack_rtt");
        let cts = val(&snap.capture_to_send, "capture_to_send");
        let resp = val(&snap.response, "response");
        let rtp = val(&snap.recv_to_play, "recv_to_play");
        let setup = snap.session_setup_s.filter(|&s| s != 0.0);
        let setup_txt = setup.map(|s| format!("{}s", s)).unwrap_or_else(|| "-".into());
        let setup_bad = setup.map(|s| s > 10.0).unwrap_or(false);
        let drop = snap.units_dropped;
        let unit_txt = format!(
            "{}{}",
            snap.units_sent,
            if drop > 0 { format!(" 丢{}", drop) } else { String::new() }
        );
        let cpu_txt = match perf.cpu_pct {
            Some(cpu) => format!("{}% wifi {}", cpu, perf.wifi_dbm.as_deref().unwrap_or("None")),
            None => "-".to_string(),
        };
        let cpu_bad = perf.cpu_pct.map(|c| c > 80).unwrap_or(false);
        let rows: [(&str, String, bool); 10] = [
            ("网RTT", net.0, net.1),
            ("ACK", ack.0, ack.1),
            ("采发", cts.0, cts.1),
            ("响应", resp.0, resp.1),
            ("出声", rtp.0, rtp.1),
            ("建会话", setup_txt, setup_bad),
            ("单元", unit_txt, drop > 0),
            ("RMS", format!("{:.3} ({:.0}dB)", view.mic_rms, view.mic_level_db), view.mic_rms > 0.5),
            ("CPU", cpu_txt, cpu_bad),
            (
                "速率",
                format!("↑{} ↓{} KB/s", perf.tx_kbs, perf.rx_kbs),
                false,
            ),
        ];
        let mut y = PANEL.1 + 4;
        for (label, txt, bad) in rows {
            self.blit_text(PANEL.0 + 10, y, label, COLOR_WARN, FONT_BODY, None);
            self.blit_text(
                PANEL.0 + 78,
                y,
                &txt,
                if bad { COLOR_ERR } else { COLOR_TEXT },
                FONT_BODY,
                None,
            );
            y += 26;
        }
        // 底部行：后台状态（思考中也在这提示）
        let mut bg = if view.background_line.is_empty() {
            "后台: 未配置".to_string()
        } else {
            view.background_line.clone()
        };
        if view.think_active {
            bg += "  思考…";
        }
        self.blit_text(
            PANEL.0 + 10,
            y,
            &fit_units(&bg, 11.5),
            COLOR_DIM,
            FONT_BODY,
            None,
        );
    }

    // ------------------------------------------------------------------
    // 设置页 / 徽章 / 信息卡
    // ------------------------------------------------------------------

    /// 二级菜单（设置页）：全屏覆盖，主界面与摄像头预览让位（`_draw_menu`）。
    pub fn draw_menu(&mut self, view: &UiView, inter: &Interaction) {
        let vol = view.volume as i32;

        self.canvas
            .fill_rect(0, 0, self.screen_w, self.screen_h, COLOR_BG);

        // 顶栏 + 返回
        self.canvas
            .fill_rect(0, 0, self.screen_w, TOPBAR_H, COLOR_PANEL);
        let back_bg = if inter.press_target == Some(TouchTarget::MenuBack) {
            COLOR_MUTED.brighten(40)
        } else {
            COLOR_MUTED
        };
        self.canvas.fill_rect(MENU_BACK.0, MENU_BACK.1, MENU_BACK.2, MENU_BACK.3, back_bg);
        self.blit_text(48, 13, "返回", COLOR_TEXT, FONT_BODY, Some(back_bg));
        self.blit_text(300, 10, "设置", COLOR_TEXT, FONT_TITLE, Some(COLOR_PANEL));

        // 扬声器音量
        self.blit_text(20, 76, "扬声器音量", COLOR_TEXT, FONT_TITLE, Some(COLOR_BG));
        for (zone, label, target) in [
            (MENU_MINUS, "-", TouchTarget::MenuMinus),
            (MENU_PLUS, "+", TouchTarget::MenuPlus),
        ] {
            let bg = if inter.press_target == Some(target) {
                COLOR_ACCENT.brighten(40)
            } else {
                COLOR_ACCENT
            };
            self.canvas.fill_rect(zone.0, zone.1, zone.2, zone.3, bg);
            let tw = (display_units(label) * FONT_TITLE as f64) as i32 + 12;
            self.blit_text(
                zone.0 + ((zone.2 - tw) / 2).max(4),
                zone.1 + 20,
                label,
                COLOR_TEXT,
                FONT_TITLE,
                Some(bg),
            );
        }
        // 音量条（拖动时高亮为暖色）
        let dragging = inter.press_target == Some(TouchTarget::MenuBar);
        self.canvas
            .fill_rect(MENU_BAR_X, MENU_BAR_Y, MENU_BAR_W, MENU_BAR_H, COLOR_PANEL);
        let fill = (MENU_BAR_W as f64 * vol as f64 / 100.0) as i32;
        if fill > 0 {
            let col = if dragging { COLOR_WARN } else { COLOR_ACCENT };
            self.canvas.fill_rect(MENU_BAR_X, MENU_BAR_Y, fill, MENU_BAR_H, col);
        }
        self.canvas
            .draw_rect(MENU_BAR_X, MENU_BAR_Y, MENU_BAR_W, MENU_BAR_H, COLOR_MUTED, 1);
        // 手柄（超出两端夹住）
        let hx = (MENU_BAR_X + fill).clamp(MENU_BAR_X + 4, MENU_BAR_X + MENU_BAR_W - 4);
        self.canvas
            .fill_rect(hx - 4, MENU_BAR_Y - 6, 8, MENU_BAR_H + 12, COLOR_TEXT);
        let pct = format!("{}%", vol);
        let ptw = (display_units(&pct) * FONT_TITLE as f64) as i32 + 12;
        self.blit_text(
            MENU_BAR_X + (MENU_BAR_W - ptw) / 2,
            200,
            &pct,
            COLOR_TEXT,
            FONT_TITLE,
            Some(COLOR_BG),
        );

        // omni模式关闭摄像头开关（静音按钮已移回主界面）
        let cam_off = !view.camera_on;
        let m_color = if cam_off { COLOR_WARN } else { COLOR_MUTED };
        let m_bg = if inter.press_target == Some(TouchTarget::MenuOmnicam) {
            m_color.brighten(40)
        } else {
            m_color
        };
        self.canvas.fill_rect(MENU_OMNICAM.0, MENU_OMNICAM.1, MENU_OMNICAM.2, MENU_OMNICAM.3, m_bg);
        self.canvas
            .draw_rect(MENU_OMNICAM.0, MENU_OMNICAM.1, MENU_OMNICAM.2, MENU_OMNICAM.3, m_color, 1);
        let m_label = if cam_off {
            "omni模式关闭摄像头: 开"
        } else {
            "omni模式关闭摄像头: 关"
        };
        let mtw = (display_units(m_label) * FONT_TITLE as f64) as i32 + 12;
        self.blit_text(
            MENU_OMNICAM.0 + (MENU_OMNICAM.2 - mtw) / 2,
            MENU_OMNICAM.1 + 24,
            m_label,
            COLOR_TEXT,
            FONT_TITLE,
            Some(m_bg),
        );

        // 网络/性能面板开关（右上角指标面板，开=全量替代底部指标条）
        let p_on = view.perf_panel;
        let p_color = if p_on { COLOR_WARN } else { COLOR_MUTED };
        let p_bg = if inter.press_target == Some(TouchTarget::MenuPerf) {
            p_color.brighten(40)
        } else {
            p_color
        };
        self.canvas.fill_rect(MENU_PERF.0, MENU_PERF.1, MENU_PERF.2, MENU_PERF.3, p_bg);
        self.canvas
            .draw_rect(MENU_PERF.0, MENU_PERF.1, MENU_PERF.2, MENU_PERF.3, p_color, 1);
        let p_label = if p_on { "网络/性能面板: 开" } else { "网络/性能面板: 关" };
        let ptw = (display_units(p_label) * FONT_TITLE as f64) as i32 + 12;
        self.blit_text(
            MENU_PERF.0 + (MENU_PERF.2 - ptw) / 2,
            MENU_PERF.1 + 20,
            p_label,
            COLOR_TEXT,
            FONT_TITLE,
            Some(p_bg),
        );

        self.draw_dev_badge(view, inter); // 菜单顶栏也带编号（同一位置）

        // ASR 转写开关（关=完全不建 ASR 连接，性能测试用；原底部设备
        // 信息行让位——编号/MAC 见顶栏徽章与信息卡）
        let a_on = view.asr_on;
        let a_color = if a_on { COLOR_WARN } else { COLOR_MUTED };
        let a_bg = if inter.press_target == Some(TouchTarget::MenuAsr) {
            a_color.brighten(40)
        } else {
            a_color
        };
        self.canvas.fill_rect(MENU_ASR.0, MENU_ASR.1, MENU_ASR.2, MENU_ASR.3, a_bg);
        self.canvas
            .draw_rect(MENU_ASR.0, MENU_ASR.1, MENU_ASR.2, MENU_ASR.3, a_color, 1);
        let a_label = if a_on { "ASR 转写: 开" } else { "ASR 转写: 关" };
        let atw = (display_units(a_label) * FONT_TITLE as f64) as i32 + 12;
        self.blit_text(
            MENU_ASR.0 + (MENU_ASR.2 - atw) / 2,
            MENU_ASR.1 + 8,
            a_label,
            COLOR_TEXT,
            FONT_TITLE,
            Some(a_bg),
        );
    }

    /// 顶栏设备编号徽章（琥珀色实底，config 的 device.label；空则不画）。
    fn draw_dev_badge(&mut self, view: &UiView, inter: &Interaction) {
        let label = view.device_label.as_str();
        if label.is_empty() {
            return;
        }
        let dw = (display_units(label) * FONT_BODY as f64) as i32 + 20;
        let d_bg = if inter.press_target == Some(TouchTarget::DevInfo) {
            COLOR_WARN.brighten(40)
        } else {
            COLOR_WARN
        };
        self.canvas.fill_rect(DEV_BADGE_X, 12, dw, 30, d_bg);
        self.blit_text(DEV_BADGE_X + 8, 14, label, Color(20, 20, 24), FONT_BODY, Some(d_bg));
    }

    /// 设备信息卡（点徽章弹出）：编号 / wlan MAC / 主机名，5s 自动消失。
    pub fn draw_info_card(&mut self, view: &UiView) {
        let label = view.device_label.as_str();
        let title = if label.is_empty() {
            "设备（未编号）".to_string()
        } else {
            format!("设备 {}", label)
        };
        let rows: [(String, u16, Color); 3] = [
            (title, FONT_TITLE, COLOR_TEXT),
            (
                format!("MAC  {}", if self.device.mac.is_empty() { "?" } else { &self.device.mac }),
                FONT_BODY,
                COLOR_DIM,
            ),
            (
                format!("主机  {}", if self.device.host.is_empty() { "?" } else { &self.device.host }),
                FONT_BODY,
                COLOR_DIM,
            ),
        ];
        let (pw, ph) = (400, 150);
        let (px, py) = ((self.screen_w - pw) / 2, (self.screen_h - ph) / 2);
        self.canvas.fill_rect(px, py, pw, ph, COLOR_BG);
        self.canvas.draw_rect(px, py, pw, ph, COLOR_WARN, 2);
        let mut y = py + 16;
        for (text, size, color) in rows {
            let tw = (display_units(&text) * size as f64) as i32;
            self.blit_text(px + (pw - tw) / 2, y, &text, color, size, None);
            y += 42;
        }
    }

    /// 整帧入口（`_draw`）：主界面或设置页 + 信息卡覆盖层。
    pub fn draw_frame(
        &mut self,
        view: &UiView,
        inter: &Interaction,
        now: Instant,
        snap: &TimingSnapshot,
        perf: &PerfSnapshot,
    ) {
        if inter.menu_open {
            self.draw_menu(view, inter);
        } else {
            self.draw_main(view, inter, snap, perf);
        }
        if let Some(until) = inter.info_until {
            if now < until {
                self.draw_info_card(view);
            }
        }
    }

    /// 当前会话状态（供测试断言按钮文案/颜色）。
    pub fn buttons_geometry(&self) -> &[Button] {
        &self.buttons
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::FontSet;
    use crate::state::{KnowledgeCard, SharedUiState};

    fn renderer() -> Renderer {
        Renderer::new(SCREEN_W, SCREEN_H, FontSet::fallback(), DeviceInfo::default())
    }

    fn view() -> UiView {
        SharedUiState::new().snapshot()
    }

    fn empty_snap() -> (TimingSnapshot, PerfSnapshot) {
        // TimingSnapshot 未实现 Default（o5-perf 契约），手动置空
        (
            TimingSnapshot {
                units_sent: 0,
                units_dropped: 0,
                turns: 0,
                audio_chunks: 0,
                capture_to_send: None,
                ack_rtt: None,
                recv_to_play: None,
                net_rtt: None,
                response: None,
                session_setup_s: None,
            },
            PerfSnapshot::default(),
        )
    }

    #[test]
    fn main_frame_topbar_blend_over_bg() {
        let mut r = renderer();
        let (snap, perf) = empty_snap();
        r.draw_main(&view(), &Interaction::default(), &snap, &perf);
        // 顶栏：bg(24,24,28) 与 panel(34,36,44) 的 alpha=110 混合
        assert_eq!(r.canvas.pixel(300, 30).unwrap(), Color(30, 31, 37));
        // 没有字幕、卡片时取景区是纯 bg
        assert_eq!(r.canvas.pixel(300, 200).unwrap(), COLOR_BG);
        assert_eq!(r.canvas.pixel(300, 320).unwrap(), COLOR_BG);
    }

    #[test]
    fn main_frame_button_colors() {
        let mut r = renderer();
        let (snap, perf) = empty_snap();
        r.draw_main(&view(), &Interaction::default(), &snap, &perf);
        // 更多 / 快门 / 静音（采样点避开按钮中央的文本芯片区和 2px 描边）
        assert_eq!(r.canvas.pixel(18, 380).unwrap(), COLOR_MUTED);
        assert_eq!(r.canvas.pixel(214, 380).unwrap(), PAXUEJI_ORANGE);
        assert_eq!(r.canvas.pixel(510, 380).unwrap(), COLOR_MUTED);
    }

    #[test]
    fn frozen_frame_covers_live_preview() {
        let st = SharedUiState::new();
        let solid = |c: u8| {
            Image::from_rgb(640, 480, vec![c; 640 * 480 * 3]).unwrap()
        };
        st.set_cam_preview(Some(solid(10)));
        st.set_frozen(Some(std::sync::Arc::new(solid(200))));
        let mut r = renderer();
        let (snap, perf) = empty_snap();
        r.draw_main(&st.snapshot(), &Interaction::default(), &snap, &perf);
        assert_eq!(r.canvas.pixel(300, 200).unwrap(), Color(200, 200, 200));
        // 定格后快门变成「再拍一张」蓝色
        assert_eq!(r.canvas.pixel(214, 380).unwrap(), COLOR_ACCENT);
        st.set_frozen(None);
        r.draw_main(&st.snapshot(), &Interaction::default(), &snap, &perf);
        assert_eq!(r.canvas.pixel(300, 200).unwrap(), Color(10, 10, 10));
    }

    #[test]
    fn menu_page_volume_bar_fill() {
        let st = SharedUiState::new();
        st.set_volume(50);
        let mut r = renderer();
        let inter = Interaction {
            menu_open: true,
            ..Default::default()
        };
        r.draw_menu(&st.snapshot(), &inter);
        // 音量条槽：track 120..460，fill = 340*0.5 = 170 → 120..290
        assert_eq!(r.canvas.pixel(250, 175).unwrap(), COLOR_ACCENT);
        assert_eq!(r.canvas.pixel(400, 175).unwrap(), COLOR_PANEL);
        // 手柄：fill=170 → hx=290（未触边界），手柄列在 286..294
        assert_eq!(r.canvas.pixel(290, 160).unwrap(), COLOR_TEXT);
        // vol=0 时无填充，手柄被夹到左端 +4（手柄列 120..128）
        st.set_volume(0);
        r.draw_menu(&st.snapshot(), &inter);
        assert_eq!(r.canvas.pixel(250, 175).unwrap(), COLOR_PANEL);
        assert_eq!(r.canvas.pixel(124, 160).unwrap(), COLOR_TEXT);
    }

    #[test]
    fn menu_page_cam_toggle_colors() {
        let st = SharedUiState::new();
        let mut r = renderer();
        let inter = Interaction {
            menu_open: true,
            ..Default::default()
        };
        r.draw_menu(&st.snapshot(), &inter);
        // camera_on=true → 开关为 MUTED 实底（采样点避开居中标签）
        assert_eq!(r.canvas.pixel(30, 260).unwrap(), COLOR_MUTED);
        // 关掉摄像头 → WARN
        st.set_camera_on(false);
        r.draw_menu(&st.snapshot(), &inter);
        assert_eq!(r.canvas.pixel(30, 260).unwrap(), COLOR_WARN);
    }

    #[test]
    fn transcript_strip_and_scrolling() {
        let st = SharedUiState::new();
        for i in 0..30 {
            st.add_sys(&format!("第 {} 句话内容", i));
        }
        let mut r = renderer();
        let (snap, perf) = empty_snap();
        r.draw_main(&st.snapshot(), &Interaction::default(), &snap, &perf);
        // 字幕带只占按钮区上方两行：(370-282-6)/28 = 2 行，压暗带从 311 开始
        let exp = Color(
            ((24u32 * 110 + 12 * 146 + 128) >> 8) as u8,
            ((24u32 * 110 + 12 * 146 + 128) >> 8) as u8,
            ((28u32 * 110 + 16 * 146 + 128) >> 8) as u8,
        );
        assert_eq!(r.canvas.pixel(400, 340).unwrap(), exp);
        // 带外（取景区）保持纯 bg，不被字幕挡住
        assert_eq!(r.canvas.pixel(300, 250).unwrap(), COLOR_BG);
    }

    #[test]
    fn info_card_overlay() {
        let st = SharedUiState::new();
        st.set_device_label("03");
        let mut r = Renderer::new(
            SCREEN_W,
            SCREEN_H,
            FontSet::fallback(),
            DeviceInfo {
                mac: "aa:bb:cc:dd:ee:ff".into(),
                host: "maixcam".into(),
            },
        );
        let (snap, perf) = empty_snap();
        let inter = Interaction {
            info_until: Some(Instant::now() + std::time::Duration::from_secs(5)),
            ..Default::default()
        };
        r.draw_frame(&st.snapshot(), &inter, Instant::now(), &snap, &perf);
        // 卡片边框 WARN 2px（信息卡 400x150 居中：x 120..520, y 165..315）
        assert_eq!(r.canvas.pixel(120, 240).unwrap(), COLOR_WARN);
        assert_eq!(r.canvas.pixel(121, 240).unwrap(), COLOR_WARN);
        assert_eq!(r.canvas.pixel(519, 240).unwrap(), COLOR_WARN);
        // 卡片左内边为 bg（避开居中文字行）
        assert_eq!(r.canvas.pixel(130, 310).unwrap(), COLOR_BG);
        // 过期不画：边框位置回到主页面底色
        let inter2 = Interaction {
            info_until: Some(Instant::now() - std::time::Duration::from_secs(1)),
            ..Default::default()
        };
        r.draw_frame(&st.snapshot(), &inter2, Instant::now(), &snap, &perf);
        assert_ne!(r.canvas.pixel(120, 240).unwrap(), COLOR_WARN);
        assert_eq!(r.canvas.pixel(120, 240).unwrap(), COLOR_BG);
    }

    #[test]
    fn dev_badge_drawn_when_label_set() {
        let st = SharedUiState::new();
        st.set_device_label("03");
        let mut r = renderer();
        let (snap, perf) = empty_snap();
        r.draw_main(&st.snapshot(), &Interaction::default(), &snap, &perf);
        // 徽章 (168, 12, dw, 30)：label "03" → units=1.1 → dw=42
        assert_eq!(r.canvas.pixel(170, 25).unwrap(), COLOR_WARN);
        st.set_device_label("");
        r.draw_main(&st.snapshot(), &Interaction::default(), &snap, &perf);
        assert_eq!(r.canvas.pixel(170, 25).unwrap(), Color(30, 31, 37)); // 回到顶栏压暗底
    }

    #[test]
    fn cam_preview_fullscreen_blit() {
        let st = SharedUiState::new();
        let mut cam = Image::new(SCREEN_W as u32, SCREEN_H as u32);
        cam.fill(Color(1, 2, 3));
        st.set_cam_preview(Some(cam));
        let mut r = renderer();
        let (snap, perf) = empty_snap();
        r.draw_main(&st.snapshot(), &Interaction::default(), &snap, &perf);
        // 相机画面铺满正文区
        assert_eq!(r.canvas.pixel(300, 200).unwrap(), Color(1, 2, 3));
        // 顶栏压暗在相机画面上：alpha110 混 (1,2,3) 与 panel(34,36,44)
        let p = r.canvas.pixel(300, 30).unwrap();
        let er = (110u32 + 34 * 146 + 128) >> 8;
        let eg = (2u32 * 110 + 36 * 146 + 128) >> 8;
        let eb = (3u32 * 110 + 44 * 146 + 128) >> 8;
        assert_eq!(p, Color(er as u8, eg as u8, eb as u8));
        // 尺寸不符的预览忽略（回退纯色底）
        st.set_cam_preview(Some(Image::new(320, 240)));
        r.draw_main(&st.snapshot(), &Interaction::default(), &snap, &perf);
        assert_eq!(r.canvas.pixel(300, 200).unwrap(), COLOR_BG);
    }

    #[test]
    fn perf_panel_replaces_metrics() {
        let st = SharedUiState::new();
        st.toggle_perf_panel();
        let mut r = renderer();
        let (mut snap, _perf) = empty_snap();
        snap.units_sent = 42;
        snap.units_dropped = 1;
        let perf = PerfSnapshot {
            cpu_pct: Some(50),
            wifi_dbm: Some("-55".into()),
            tx_kbs: 12,
            rx_kbs: 34,
            ui_fps: 15.0,
        };
        let v = st.snapshot();
        r.draw_main(&v, &Interaction::default(), &snap, &perf);
        // 面板底色：bg 混 strip(12,12,16)
        let p = r.canvas.pixel(600, 200).unwrap();
        let er = (24u32 * 110 + 12 * 146 + 128) >> 8;
        let eb = (28u32 * 110 + 16 * 146 + 128) >> 8;
        assert_eq!(p, Color(er as u8, er as u8, eb as u8));
        // 指标条区域（METRIC2_Y）在面板外（y=336 > 面板底 354？336 在面板内）
        // 面板区域 y 58..354：336 在内 → 也是面板底色
        assert_eq!(r.canvas.pixel(600, 340).unwrap(), Color(er as u8, er as u8, eb as u8));
        // 空快照 + None CPU 的面板也能画（不 panic）
        let (s2, p2) = empty_snap();
        r.draw_main(&v, &Interaction::default(), &s2, &p2);
    }

    #[test]
    fn knowledge_card_drawn_and_cleared_on_unfreeze() {
        let st = SharedUiState::new();
        st.set_card(Some(KnowledgeCard {
            title: "银杏叶".into(),
            points: vec!["秋天会变金黄".into(), "像一把小扇子".into()],
            source: Some("baike.com".into()),
        }));
        let mut r = renderer();
        let (snap, perf) = empty_snap();
        r.draw_main(&st.snapshot(), &Interaction::default(), &snap, &perf);
        let (x, y, w) = CARD_RECT;
        assert_eq!(r.canvas.pixel(x + 1, y + 1).unwrap(), PAXUEJI_ORANGE);
        assert_eq!(r.canvas.pixel(x + w - 10, y + 12).unwrap(), CARD_BG);
        st.set_frozen(None);
        r.draw_main(&st.snapshot(), &Interaction::default(), &snap, &perf);
        assert_eq!(r.canvas.pixel(x + w - 10, y + 12).unwrap(), COLOR_BG);
    }

    #[test]
    fn solid_chip_extends_bg() {
        // 实底芯片：blit_text(bg=Some) 先铺 est_w × (size+8) 的底
        let mut r = renderer();
        r.blit_text(100, 100, "ab", Color(255, 255, 255), FONT_BODY, Some(Color(9, 8, 7)));
        // est_w = 1.1*20 + 12 = 34 → 100..134 宽、100..128 高全是芯片底
        assert_eq!(r.canvas.pixel(101, 101).unwrap(), Color(9, 8, 7));
        assert_eq!(r.canvas.pixel(133, 127).unwrap(), Color(9, 8, 7));
        assert_eq!(r.canvas.pixel(135, 101).unwrap(), Color(0, 0, 0)); // 画布底（未清屏为黑）
        assert_eq!(r.canvas.pixel(101, 129).unwrap(), Color(0, 0, 0));
    }
}

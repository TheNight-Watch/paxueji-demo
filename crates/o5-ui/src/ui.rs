//! `Ui` 主循环：50Hz 触摸轮询 + 15fps 浮点节拍重绘 + feeder 帧投递。
//!
//! Python `ui.py run()` 的移植。节拍坑（ui.py 264-268 注释）：
//! 重绘间隔用**浮点节拍累加**（`last_draw += interval` 按计划时刻推进），
//! 而不是取整到主循环节拍——`sleep 0.04` 的主循环里 67ms 间隔总会滑到
//! 2 个节拍（~110ms → 实际 8fps）。落后超过一整个间隔（冷启动 / 卡顿）
//! 时重同步到当前时刻，不追帧。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use o5_config::Config;
use o5_perf::{PerfMonitor, Timing};

use crate::display::{self, DisplaySink};
use crate::events::UiEvent;
use crate::font::{font_candidates, FontSet};
use crate::key::ShutterKey;
use crate::layout::*;
use crate::render::{read_device_info, Interaction, Renderer, TouchTarget};
use crate::state::SharedUiState;
use crate::touch::{self, TouchSource};

/// 主循环节拍（50Hz：触摸轮询 + 状态轮询更跟手）。
const LOOP_INTERVAL_S: f64 = 0.02;
/// 相机预览活跃（且菜单关）时的重绘间隔：15fps（芯片缓存后每帧 ~20ms）。
const CAM_FRAME_INTERVAL_S: f64 = 0.067;
/// 纯文本界面/设置页的重绘间隔：10fps。
const TEXT_FRAME_INTERVAL_S: f64 = 0.1;
/// 设备信息卡显示时长。
const INFO_CARD_S: f64 = 5.0;
/// 主界面按钮与实体快门键的防抖窗口：hyn_ts 触摸驱动一次点击会抖出 3—4 个
/// 按下沿，跨度实测约 0.7 秒（dmesg release_all_finger），窗口要盖过它。
const ACTION_DEBOUNCE: Duration = Duration::from_millis(1000);

/// 浮点节拍累加器（可注入假时刻，单测覆盖滑档/重同步语义）。
#[derive(Debug, Default)]
pub(crate) struct DrawPacer {
    last_draw: Option<f64>,
}

impl DrawPacer {
    pub fn new() -> DrawPacer {
        DrawPacer { last_draw: None }
    }

    /// 本拍是否应重绘（`now` 为单调秒）。
    pub fn tick(&mut self, now: f64, interval: f64) -> bool {
        let Some(last) = self.last_draw else {
            // 首帧立即（Python last_draw=0.0 + 单调钟远大于 interval 的等效行为）
            self.last_draw = Some(now);
            return true;
        };
        if now - last < interval {
            return false;
        }
        // 浮点节拍累计：按计划时刻推进；落后超过一整个间隔时重同步，不追帧
        let mut next = last + interval;
        if now - next >= interval {
            next = now;
        }
        self.last_draw = Some(next);
        true
    }
}

/// 触屏 UI（状态栏 + 对话字幕 + 实时指标 + 按钮区 + 二级设置页）。
pub struct Ui {
    renderer: Renderer,
    sink: Box<dyn DisplaySink>,
    touch: Box<dyn TouchSource>,
    inter: Interaction,
    stop_flag: Arc<AtomicBool>,
    quit: bool,
    frames: u64,
    canvas_dump: bool,
    shutter_key: ShutterKey,
    /// 主界面按钮 / 实体键上次生效时刻（防抖：触摸控制器偶发连发误触）。
    last_action: Option<Instant>,
}

impl Ui {
    /// 设备构造：feeder 显示 + evdev 触摸 + 字体候选（config 决定 feeder
    /// 路径与触摸变体；PC 上自动落到 NullSink + 空闲 mock + 回退字体）。
    pub fn new(cfg: &Config) -> Ui {
        let fonts = FontSet::load_or_fallback(&font_candidates());
        let sink = display::open_default(cfg);
        let touch = touch::open_default(cfg);
        Ui::assemble(fonts, sink, touch)
    }

    /// 注入显示/触摸后端与字体（测试 / --test-pc 用）。
    pub fn with_backend(
        sink: Box<dyn DisplaySink>,
        touch: Box<dyn TouchSource>,
        fonts: FontSet,
    ) -> Ui {
        Ui::assemble(fonts, sink, touch)
    }

    fn assemble(fonts: FontSet, sink: Box<dyn DisplaySink>, touch: Box<dyn TouchSource>) -> Ui {
        Ui {
            renderer: Renderer::new(
                SCREEN_W,
                SCREEN_H,
                fonts,
                read_device_info(),
            ),
            sink,
            touch,
            inter: Interaction::default(),
            stop_flag: Arc::new(AtomicBool::new(false)),
            quit: false,
            frames: 0,
            // 调试：O5_CANVAS_DUMP=1 时每 15 帧转储画布到 /tmp/canvas_XXX.png
            canvas_dump: std::env::var("O5_CANVAS_DUMP").is_ok(),
            shutter_key: ShutterKey::open(),
            last_action: None,
        }
    }

    /// 请求退出主循环（跨线程安全；run() 每轮检查）。
    /// 注意：置位后此 Ui 实例不复用（退出后重建）。
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }

    /// 阻塞跑主循环（跑在专属线程；15fps 浮点节拍、触摸 50Hz、
    /// 事件从 channel 出）。`perf` 传 None 则性能面板用空快照。
    pub fn run(
        &mut self,
        state: SharedUiState,
        timing: Arc<Timing>,
        perf: Option<PerfMonitor>,
        events: Sender<UiEvent>,
    ) {
        let mut pressed_prev = false;
        let mut pacer = DrawPacer::new();
        let t_anchor = Instant::now();
        while !self.quit && !self.stop_flag.load(Ordering::Relaxed) {
            let t_iter = Instant::now();
            // 触摸轮询（官方推荐：按下->松开 = 一次点击）
            let sample = self.touch.poll();
            if sample.pressed && !pressed_prev {
                self.on_press(sample.x, sample.y, &state, &events);
            } else if !sample.pressed && pressed_prev {
                self.on_release(sample.x, sample.y, &state, &events);
            } else if sample.pressed && self.inter.press_target == Some(TouchTarget::MenuBar) {
                // 按住音量条拖动，实时调节
                self.on_bar_drag(sample.x, &state, &events);
            }
            pressed_prev = sample.pressed;
            if self.shutter_key.poll_presses() > 0 && !self.inter.menu_open && self.debounce_ok() {
                log::info!("[拍学机] Func 键快门");
                let _ = events.send(UiEvent::Shutter);
            }

            let now = t_anchor.elapsed().as_secs_f64();
            let interval = if state.has_cam_preview() && !self.inter.menu_open {
                CAM_FRAME_INTERVAL_S
            } else {
                TEXT_FRAME_INTERVAL_S
            };
            if pacer.tick(now, interval) {
                let t0 = Instant::now();
                let view = state.snapshot();
                let snap = timing.snapshot();
                let psnap = perf.as_ref().map(|p| p.snapshot()).unwrap_or_default();
                self.renderer
                    .draw_frame(&view, &self.inter, Instant::now(), &snap, &psnap);
                let t1 = Instant::now();
                self.sink.push_frame(&self.renderer.canvas.data);
                let t2 = Instant::now();
                // 注：Python 版 ui_draw 的口径含 show；Rust 契约拆成两条
                timing.ui_draw(ms_since(t0, t1));
                timing.ui_show(ms_since(t1, t2));
                self.frames += 1;
                if self.canvas_dump && self.frames.is_multiple_of(15) {
                    self.dump_canvas();
                }
            }
            // 主循环 50Hz（触摸轮询 + 状态轮询更跟手）
            std::thread::sleep(Duration::from_secs_f64(LOOP_INTERVAL_S));
            // 节拍探针：一轮 >~100ms 说明 UI 线程被别的线程挤掉了
            timing.ui_loop(t_iter.elapsed().as_secs_f64() * 1000.0);
        }
        log::info!("UI 退出");
    }

    fn dump_canvas(&self) {
        let idx = (self.frames / 15 % 40) as usize;
        let path: std::path::PathBuf = format!("/tmp/canvas_{:03}.png", idx).into();
        crate::png::write_png(&path, self.renderer.canvas.width, self.renderer.canvas.height, &self.renderer.canvas.data);
        let mean = self.renderer.canvas.data.iter().map(|&b| b as f64).sum::<f64>()
            / self.renderer.canvas.data.len() as f64;
        log::info!("[canvas] #{} mean {:.1}", self.frames, mean);
    }

    // -- 触摸 ------------------------------------------------------------

    /// 按下（`_on_press`）：只记录按压目标（渲染高亮用）。
    fn on_press(&mut self, x: i32, y: i32, state: &SharedUiState, events: &Sender<UiEvent>) {
        if self.inter.menu_open {
            // 设置页控件（主界面按钮区不响应）
            self.inter.press_target = if in_zone(x, y, MENU_BACK) {
                Some(TouchTarget::MenuBack)
            } else if in_zone(x, y, MENU_MINUS) {
                Some(TouchTarget::MenuMinus)
            } else if in_zone(x, y, MENU_BAR_HIT) {
                Some(TouchTarget::MenuBar)
            } else if in_zone(x, y, MENU_PLUS) {
                Some(TouchTarget::MenuPlus)
            } else if in_zone(x, y, MENU_OMNICAM) {
                Some(TouchTarget::MenuOmnicam)
            } else if in_zone(x, y, MENU_PERF) {
                Some(TouchTarget::MenuPerf)
            } else if in_zone(x, y, MENU_ASR) {
                Some(TouchTarget::MenuAsr)
            } else {
                None
            };
            if self.inter.press_target == Some(TouchTarget::MenuBar) {
                // 点哪儿设哪儿，按下立即生效
                self.on_bar_drag(x, state, events);
            }
            return;
        }
        let btns = self.renderer.buttons_geometry();
        for (i, b) in btns.iter().enumerate() {
            if b.hit(x, y) {
                self.inter.press_target = Some(TouchTarget::Btn(i));
                return;
            }
        }
        // 顶栏热区
        self.inter.press_target = if in_quit_zone(x, y, SCREEN_W) {
            Some(TouchTarget::Quit)
        } else if in_status_zone(x, y, SCREEN_W) {
            Some(TouchTarget::Status)
        } else if in_zone(x, y, DEV_BADGE_HIT) {
            Some(TouchTarget::DevInfo)
        } else {
            None
        };
    }

    /// 抬起（`_on_release`）：目标仍在命中区才触发动作（沿检测防误触）。
    fn on_release(
        &mut self,
        x: i32,
        y: i32,
        state: &SharedUiState,
        events: &Sender<UiEvent>,
    ) {
        let target = self.inter.press_target.take();
        let Some(target) = target else { return };
        if matches!(target, TouchTarget::Btn(_) | TouchTarget::Status) && !self.debounce_ok() {
            log::info!("[拍学机] 防抖丢弃一次点击 {target:?}");
            return;
        }
        match target {
            TouchTarget::MenuBack => {
                if in_zone(x, y, MENU_BACK) {
                    self.inter.menu_open = false;
                }
            }
            TouchTarget::MenuMinus => {
                if in_zone(x, y, MENU_MINUS) {
                    let v = state.volume().saturating_sub(10);
                    let _ = events.send(UiEvent::VolumeChanged(v as f32));
                }
            }
            TouchTarget::MenuPlus => {
                if in_zone(x, y, MENU_PLUS) {
                    let v = state.volume().saturating_add(10).min(100);
                    let _ = events.send(UiEvent::VolumeChanged(v as f32));
                }
            }
            TouchTarget::MenuBar => {
                if in_zone(x, y, MENU_BAR_HIT) {
                    // 松开位置再校一次（拖动结束）
                    self.on_bar_drag(x, state, events);
                }
            }
            TouchTarget::MenuOmnicam => {
                if in_zone(x, y, MENU_OMNICAM) {
                    // Python ui_toggle_camera：UI 线程翻转 camera_on（app 观察状态停/启流）
                    let now = state.toggle_camera_on();
                    log::info!("omni模式关闭摄像头: {}", if now { "关" } else { "开" });
                }
            }
            TouchTarget::MenuPerf => {
                if in_zone(x, y, MENU_PERF) {
                    // 状态由 UI 翻转（照 Python 的同步语义），事件为通知
                    let now = state.toggle_perf_panel();
                    log::info!("网络/性能面板: {}", if now { "开" } else { "关" });
                    let _ = events.send(UiEvent::PanelToggle);
                }
            }
            TouchTarget::MenuAsr => {
                if in_zone(x, y, MENU_ASR) {
                    // 启停 AsrClient 属 app 职责，状态由 app 处理器翻转后
                    // 写回 set_asr_on（Rust 独有：性能测试用）
                    let _ = events.send(UiEvent::AsrToggle);
                }
            }
            TouchTarget::Quit => {
                if in_quit_zone(x, y, SCREEN_W) {
                    self.quit = true; // run() 的循环条件会检查
                    let _ = events.send(UiEvent::CloseApp);
                }
            }
            TouchTarget::Status => {
                // 会话断了（失败/空闲）时点状态章重连；在跑时不响应，防误触挂断
                if in_status_zone(x, y, SCREEN_W) && !state.snapshot().session_state.running() {
                    let _ = events.send(UiEvent::StartStop);
                }
            }
            TouchTarget::DevInfo => {
                if in_zone(x, y, DEV_BADGE_HIT) {
                    // 信息卡显示 5s
                    self.inter.info_until =
                        Some(Instant::now() + Duration::from_secs_f64(INFO_CARD_S));
                }
            }
            TouchTarget::Btn(i) => {
                let b = self.renderer.buttons_geometry()[i];
                if b.hit(x, y) {
                    match b.action {
                        ButtonAction::Shutter => {
                            log::info!("[拍学机] 触屏快门");
                            let _ = events.send(UiEvent::Shutter);
                        }
                        ButtonAction::Mute => {
                            let _ = events.send(UiEvent::Mute);
                        }
                        ButtonAction::Menu => {
                            self.inter.menu_open = true;
                            // 通知事件（app 可暂停相机推流等；设置页纯 UI 态）
                            let _ = events.send(UiEvent::Menu);
                        }
                    }
                }
            }
        }
    }

    /// 距上次生效超过防抖窗口才放行，并记下本次时刻。
    fn debounce_ok(&mut self) -> bool {
        let now = Instant::now();
        if self.last_action.is_some_and(|t| now.duration_since(t) < ACTION_DEBOUNCE) {
            return false;
        }
        self.last_action = Some(now);
        true
    }

    /// 音量条拖动（`_on_bar_drag`）：横坐标换算成 0-100（5% 步进），
    /// 与当前值不同才触发回调。
    fn on_bar_drag(&mut self, x: i32, state: &SharedUiState, events: &Sender<UiEvent>) {
        let xc = x.clamp(MENU_BAR_X, MENU_BAR_X + MENU_BAR_W);
        let frac = (xc - MENU_BAR_X) as f64 / MENU_BAR_W as f64;
        let vol = (frac * 20.0).round() as i32 * 5;
        let vol = vol.clamp(0, 100) as u8;
        if vol != state.volume() {
            let _ = events.send(UiEvent::VolumeChanged(vol as f32));
        }
    }

    /// 当前交互态（测试探查用）。
    #[cfg(test)]
    pub(crate) fn interaction(&self) -> &Interaction {
        &self.inter
    }
}

fn ms_since(a: Instant, b: Instant) -> f64 {
    b.saturating_duration_since(a).as_secs_f64() * 1000.0
}

#[cfg(test)]
impl Ui {
    /// 测试辅助：把当前画布推给显示后端。
    fn sink_push_for_test(&mut self) {
        let data = self.renderer.canvas.data.clone();
        self.sink.push_frame(&data);
    }

    /// 测试辅助：修改交互态。
    fn interaction_mut(&mut self) -> &mut Interaction {
        &mut self.inter
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::PngDumpSink;
    use crate::events::UiEvent;
    use crate::font::FontSet;
    use crate::image::Color;
    use crate::png::decode_png_store;
    use crate::render::TouchTarget;
    use crate::state::SessionPhase;
    use crate::touch::MockTouch;
    use o5_perf::Timing;

    fn mk_ui() -> Ui {
        Ui::with_backend(
            Box::new(crate::display::NullSink),
            Box::new(MockTouch::idle()),
            FontSet::fallback(),
        )
    }

    // —— 节拍 -----------------------------------------------------------

    #[test]
    fn pacer_steady_grid() {
        let mut p = DrawPacer::new();
        let mut draws = 0;
        for step in 0..100 {
            if p.tick(step as f64 * 0.02, 0.1) {
                draws += 1;
            }
        }
        // 0.1s 间隔、0.02 轮询：2 秒内 20 帧（0.0, 0.1, ... 1.9? —— 0..100 是 2s，帧在 0.0..1.9）
        assert_eq!(draws, 20, "0.1s 节拍 2 秒应画 20 帧");
    }

    #[test]
    fn pacer_float_accumulation_avoids_slip() {
        // 0.04s 主循环 + 0.067s 间隔：整拍会滑到 0.08s（12.5fps）；
        // 浮点累加按 0.067 的倍数推进，2 秒应接近 30 帧
        let mut p = DrawPacer::new();
        let mut draws = 0;
        for step in 0..50 {
            if p.tick(step as f64 * 0.04, 0.067) {
                draws += 1;
            }
        }
        assert!(draws >= 28, "浮点累加应 ~15fps（0.067s），实际 2s {} 帧", draws);
        assert!(draws <= 30);
    }

    #[test]
    fn pacer_resync_no_catchup_burst() {
        let mut p = DrawPacer::new();
        assert!(p.tick(0.0, 0.067));
        // 停画 1 秒后回来：只补 1 帧（重同步），不追帧
        assert!(p.tick(1.0, 0.067));
        assert!(!p.tick(1.02, 0.067));
        assert!(!p.tick(1.05, 0.067));
        assert!(p.tick(1.068, 0.067));
    }

    #[test]
    fn pacer_first_frame_immediate() {
        let mut p = DrawPacer::new();
        assert!(p.tick(123.456, 0.1));
        assert!(!p.tick(123.5, 0.1));
        assert!(p.tick(123.56, 0.1));
    }

    // —— 沿检测点击 ------------------------------------------------------

    fn press(ui: &mut Ui, x: i32, y: i32) {
        let st = SharedUiState::new();
        let (tx, _rx) = crossbeam_channel::bounded(4);
        ui.on_press(x, y, &st, &tx);
    }

    fn release(ui: &mut Ui, x: i32, y: i32, st: &SharedUiState) -> Vec<UiEvent> {
        let (tx, rx) = crossbeam_channel::bounded(4);
        ui.on_release(x, y, st, &tx);
        rx.try_iter().collect()
    }

    fn release_with(ui: &mut Ui, x: i32, y: i32) -> Vec<UiEvent> {
        let st = SharedUiState::new();
        release(ui, x, y, &st)
    }

    #[test]
    fn button_press_release_edges() {
        let mut ui = mk_ui();
        let st = SharedUiState::new();
        press(&mut ui, 70, 418);
        assert_eq!(ui.interaction().press_target, Some(TouchTarget::Btn(0)));
        assert_eq!(release(&mut ui, 70, 418, &st), vec![UiEvent::Menu]);
        assert!(ui.interaction().menu_open);
        let mut ui = mk_ui();
        press(&mut ui, 566, 418);
        assert_eq!(ui.interaction().press_target, Some(TouchTarget::Btn(2)));
        assert_eq!(release(&mut ui, 566, 418, &st), vec![UiEvent::Mute]);
        // 按下后滑出按钮再松开不触发
        press(&mut ui, 320, 418);
        assert!(release(&mut ui, 320, 300, &st).is_empty());
    }

    #[test]
    fn release_outside_zone_no_event() {
        let mut ui = mk_ui();
        press(&mut ui, 86, 418); // 开始按钮
        // 滑出按钮区松开 → 不触发
        assert!(release_with(&mut ui, 400, 100).is_empty());
        assert_eq!(ui.interaction().press_target, None);
    }

    #[test]
    fn press_without_release_no_event() {
        let mut ui = mk_ui();
        press(&mut ui, 86, 418);
        assert!(release_with(&mut ui, 86, 418).len() == 1);
        // 未按下时直接 release：无目标无事件
        assert!(release_with(&mut ui, 86, 418).is_empty());
    }

    #[test]
    fn quit_zone_edge_detection() {
        let mut ui = mk_ui();
        press(&mut ui, 600, 20);
        assert_eq!(ui.interaction().press_target, Some(TouchTarget::Quit));
        let ev = release_with(&mut ui, 600, 20);
        assert_eq!(ev, vec![UiEvent::CloseApp]);
    }

    #[test]
    fn dev_badge_pop_info_card() {
        let mut ui = mk_ui();
        press(&mut ui, 200, 20); // DEV_BADGE_HIT = (160, 0, 120, 40)
        assert_eq!(ui.interaction().press_target, Some(TouchTarget::DevInfo));
        let ev = release_with(&mut ui, 200, 20);
        assert!(ev.is_empty());
        assert!(ui.interaction().info_until.is_some());
    }

    #[test]
    fn menu_open_blocks_main_buttons() {
        let mut ui = mk_ui();
        ui.interaction_mut().menu_open = true;
        press(&mut ui, 320, 228); // 菜单页空白带（音量条与开关之间的间隙）
        assert_eq!(ui.interaction().press_target, None);
    }

    #[test]
    fn menu_back_closes_page() {
        let mut ui = mk_ui();
        ui.interaction_mut().menu_open = true;
        press(&mut ui, 50, 20); // MENU_BACK = (14, 8, 126, 36)
        assert_eq!(ui.interaction().press_target, Some(TouchTarget::MenuBack));
        let st = SharedUiState::new();
        let ev = release(&mut ui, 50, 20, &st);
        assert!(ev.is_empty());
        assert!(!ui.interaction().menu_open);
    }

    #[test]
    fn menu_volume_buttons_step() {
        let mut ui = mk_ui();
        ui.interaction_mut().menu_open = true;
        let st = SharedUiState::new();
        st.set_volume(80);
        press(&mut ui, 60, 175); // MENU_MINUS = (20, 140, 80, 70)
        assert_eq!(ui.interaction().press_target, Some(TouchTarget::MenuMinus));
        assert_eq!(release(&mut ui, 60, 175, &st), vec![UiEvent::VolumeChanged(70.0)]);

        press(&mut ui, 520, 175); // MENU_PLUS = (480, 140, 80, 70)
        assert_eq!(ui.interaction().press_target, Some(TouchTarget::MenuPlus));
        assert_eq!(release(&mut ui, 520, 175, &st), vec![UiEvent::VolumeChanged(90.0)]);

        // 边界截断（偏离 Python：不再发负值/超 100，由 app clamp 变为 UI clamp）
        st.set_volume(0);
        press(&mut ui, 60, 175);
        assert_eq!(release(&mut ui, 60, 175, &st), vec![UiEvent::VolumeChanged(0.0)]);
        st.set_volume(100);
        press(&mut ui, 520, 175);
        assert_eq!(release(&mut ui, 520, 175, &st), vec![UiEvent::VolumeChanged(100.0)]);
    }

    #[test]
    fn volume_bar_drag_press_and_move() {
        let mut ui = mk_ui();
        ui.interaction_mut().menu_open = true;
        let st = SharedUiState::new();
        st.set_volume(80);
        let (tx, rx) = crossbeam_channel::bounded(8);
        // 按下在条中心（BAR_HIT 内）立即生效
        ui.on_press(290, 175, &st, &tx);
        assert_eq!(ui.interaction().press_target, Some(TouchTarget::MenuBar));
        let ev: Vec<_> = rx.try_iter().collect();
        assert_eq!(ev, vec![UiEvent::VolumeChanged(50.0)]);
        // 拖到右端
        st.set_volume(50);
        ui.on_bar_drag(460, &st, &tx);
        let ev: Vec<_> = rx.try_iter().collect();
        assert_eq!(ev, vec![UiEvent::VolumeChanged(100.0)]);
        // 同值不重复触发
        st.set_volume(100);
        ui.on_bar_drag(460, &st, &tx);
        assert!(rx.try_iter().next().is_none());
        // 左端夹紧 → 0
        ui.on_bar_drag(0, &st, &tx);
        let ev: Vec<_> = rx.try_iter().collect();
        assert_eq!(ev, vec![UiEvent::VolumeChanged(0.0)]);
        // 5% 步进：x=350 → frac=(350-120)/340=0.676 → 13.5 → round 14 → 70
        st.set_volume(0);
        ui.on_bar_drag(350, &st, &tx);
        let ev: Vec<_> = rx.try_iter().collect();
        assert_eq!(ev, vec![UiEvent::VolumeChanged(70.0)]);
    }

    #[test]
    fn menu_omnicam_and_perf_toggle_state() {
        let mut ui = mk_ui();
        ui.interaction_mut().menu_open = true;
        let st = SharedUiState::new();
        // omnicam：UI 直接翻转 camera_on
        press(&mut ui, 320, 267); // MENU_OMNICAM = (20, 236, 600, 58)
        assert_eq!(ui.interaction().press_target, Some(TouchTarget::MenuOmnicam));
        let ev = release(&mut ui, 320, 267, &st);
        assert!(ev.is_empty());
        assert!(!st.snapshot().camera_on);
        // perf：状态翻转 + PanelToggle 通知
        press(&mut ui, 320, 337); // MENU_PERF = (20, 302, 600, 58)
        let ev = release(&mut ui, 320, 337, &st);
        assert_eq!(ev, vec![UiEvent::PanelToggle]);
        assert!(st.snapshot().perf_panel);
        // 拍学机去掉了公网调用开关：原位置点了不响应（误触会断会话）
        press(&mut ui, 320, 397);
        assert_eq!(ui.interaction().press_target, None);
        // asr：UI 只发事件（状态由 app 翻转；关=完全不建连接）
        press(&mut ui, 320, 456); // MENU_ASR = (20, 434, 600, 44)
        assert_eq!(ui.interaction().press_target, Some(TouchTarget::MenuAsr));
        let ev = release(&mut ui, 320, 456, &st);
        assert_eq!(ev, vec![UiEvent::AsrToggle]);
        assert!(st.snapshot().asr_on, "app 未处理前状态不变");
    }

    #[test]
    fn status_chip_reconnects_only_when_idle() {
        let mut ui = mk_ui();
        let st = SharedUiState::new();
        press(&mut ui, 500, 20);
        assert_eq!(ui.interaction().press_target, Some(TouchTarget::Status));
        assert_eq!(release(&mut ui, 500, 20, &st), vec![UiEvent::StartStop]);
        st.set_session(SessionPhase::Ready, "");
        press(&mut ui, 500, 20);
        assert!(release(&mut ui, 500, 20, &st).is_empty(), "会话在跑时点状态章不挂断");
    }

    #[test]
    fn repeated_taps_within_window_are_dropped() {
        let mut ui = mk_ui();
        let st = SharedUiState::new();
        press(&mut ui, 566, 418);
        assert_eq!(release(&mut ui, 566, 418, &st), vec![UiEvent::Mute]);
        press(&mut ui, 566, 418);
        assert!(release(&mut ui, 566, 418, &st).is_empty(), "1 秒内的第二下是误触");
    }

    #[test]
    fn shutter_button_sends_shutter() {
        let mut ui = mk_ui();
        let st = SharedUiState::new();
        press(&mut ui, 320, 410);
        assert_eq!(release(&mut ui, 320, 410, &st), vec![UiEvent::Shutter]);
    }

    // —— 整帧渲染 + PNG 投递 ----------------------------------------------

    #[test]
    fn full_frame_png_roundtrip_pixel_assert() {
        let dir = std::env::temp_dir().join(format!("o5_ui_full_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let st = SharedUiState::new();
        let mut ui = Ui::with_backend(
            Box::new(PngDumpSink::new(&dir, SCREEN_W as u32, SCREEN_H as u32)),
            Box::new(MockTouch::idle()),
            FontSet::fallback(),
        );
        let timing = Arc::new(Timing::new());
        // 单帧直跑：手动驱动一次 draw + push（不进 run 循环）
        let view = st.snapshot();
        let snap = timing.snapshot();
        let perf = o5_perf::PerfSnapshot::default();
        ui.renderer.draw_frame(&view, &Interaction::default(), Instant::now(), &snap, &perf);
        ui.sink_push_for_test();
        // 解码断言
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        let bytes = std::fs::read(&files[0]).unwrap();
        let (w, h, rgb) = decode_png_store(&bytes);
        assert_eq!((w, h), (SCREEN_W as u32, SCREEN_H as u32));
        let px = |x: usize, y: usize| {
            let i = (y * w as usize + x) * 3;
            Color(rgb[i], rgb[i + 1], rgb[i + 2])
        };
        // 顶栏压暗底：bg(24,24,28)×panel(34,36,44) alpha110 → (30,31,37)
        assert_eq!(px(300, 30), Color(30, 31, 37));
        // 按钮区色块：更多=MUTED、快门=橙（避开文本区采样）
        assert_eq!(px(18, 380), COLOR_MUTED);
        assert_eq!(px(214, 380), PAXUEJI_ORANGE);
        // 正文区纯 bg
        assert_eq!(px(320, 200), COLOR_BG);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_flag_exits_run() {
        let mut ui = mk_ui();
        let st = SharedUiState::new();
        let timing = Arc::new(Timing::new());
        let (tx, _ev) = crossbeam_channel::bounded(4);
        ui.stop();
        // run 应立即退出（首拍前 stop_flag 已置位）
        let t0 = Instant::now();
        ui.run(st, timing, None, tx);
        assert!(t0.elapsed().as_millis() < 500);
    }
}

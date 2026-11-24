//! 拍学机：快门定格、知识卡片、联网查询芯片。
//!
//! 定格的那一帧同时用于屏显和上行，孩子看到的就是模型看到的。「我拍了新照片」
//! 这句话要等定格帧随下一个 unit 上行之后再发，否则模型会对着上一帧讲。

use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::VecDeque;

use o5_camera::PreviewFrame;
use o5_protocol::ParsedToolCall;
use o5_ui::{Image, KnowledgeCard};
use serde_json::Value;

use super::App;

/// 快门事件（v5 作为独立事件原样送入）。真机上只说「这是什么」时模型只描述画面，
/// 所以把讲解步骤直接写进事件，逼它讲知识点、以小问题收尾。
pub(super) const SHUTTER_PROMPT: &str = "【快门】小朋友刚拍了一张新照片。只讲这张新照片，之前没讲完的话题不用再接着讲。一共说三句话：第一句说出这是什么，同时调用 show_card；第二句讲一个小朋友会觉得好玩的知识点；第三句必须是一个问句，引导他继续观察或动手试试，比如“你数一数，它有几条腿？”。不要描述桌面、背景这些无关的东西。";

const SEARCH_CHIP_S: f64 = 30.0;
const FOUND_CHIP_S: f64 = 6.0;
/// 卡片没带来源时，借用这么久以内的一次搜索来源。
const SEARCH_SOURCE_TTL: Duration = Duration::from_secs(90);

/// 镜头是手动对焦的 M12 定焦头，按快门（尤其按实体键）时手会抖。快门不取「当下一帧」，
/// 而是从最近这段时间的预览里挑最清楚的一帧。
const PICK_WINDOW: Duration = Duration::from_millis(800);
const RECENT_CAP: usize = 10;
/// 低于这个清晰度提示孩子拿稳或离远一点（拉普拉斯方差，真机标定前的保守值）。
const BLUR_THRESHOLD: f64 = 25.0;

#[derive(Default)]
pub(super) struct Paxueji {
    pub recent: VecDeque<(Instant, PreviewFrame)>,
    pub frozen_jpeg: Option<String>,
    /// 定格帧已就位、还没把快门那句话发给模型。
    pub prompt_pending: bool,
    pub last_search_source: Option<(String, Instant)>,
    /// 快门时刻：第一句口播到达时记一次「快门→首句」延迟。
    pub shutter_at: Option<Instant>,
    /// 静音期间已记过一次「丢弃口播音频」（每次静音只记一次）。
    pub mute_logged: bool,
    /// 每次定格 / 回到取景加一：工具结果回来时照片已经换了，就不该再讲旧东西。
    pub photo_gen: u64,
    /// 测试通道注入的 16k 单声道 PCM：有数据时代替麦克风上行。
    pub inject: VecDeque<u8>,
    pub reconnects: u32,
}

impl App {
    pub(super) fn ui_shutter(&self) {
        if self.state.is_frozen() {
            let mut p = self.paxueji.lock().unwrap();
            let gen = p.photo_gen + 1;
            let inject = std::mem::take(&mut p.inject);
            let reconnects = p.reconnects;
            *p = Paxueji { photo_gen: gen, inject, reconnects, ..Paxueji::default() };
            drop(p);
            self.state.set_frozen(None);
            self.stop_talking();
            return;
        }
        let Some((frame, score)) = self.sharpest_recent() else {
            self.add_sys("摄像头还没准备好，稍等一下再拍");
            return;
        };
        self.freeze_with(frame, score);
    }

    fn freeze_with(&self, frame: PreviewFrame, score: f64) {
        log::info!(target: "o5.main", "[拍学机] 快门取帧清晰度 {score:.1}");
        if score < BLUR_THRESHOLD {
            self.state.set_tool_chip("有点糊，拿稳或离远一点再拍", 5.0);
        }
        let quality = self.cfg.read().unwrap().video_jpeg_quality;
        let Some(b64) = frame.to_jpeg_b64(quality) else {
            self.add_sys("这张没拍好，再按一次试试");
            return;
        };
        let Some(img) = Image::from_rgb(frame.width, frame.height, frame.data) else {
            return;
        };
        {
            let mut p = self.paxueji.lock().unwrap();
            p.frozen_jpeg = Some(b64);
            p.prompt_pending = true;
            p.shutter_at = Some(Instant::now());
            p.photo_gen += 1;
        }
        self.state.set_frozen(Some(Arc::new(img)));
        self.stop_talking();
        log::info!(target: "o5.main", "[拍学机] 快门定格");
    }

    /// 测试注入：注入队列里有声音就整单元替换麦克风数据（不足补静音）。
    pub(super) fn injected_unit(&self, mic: Vec<u8>) -> Vec<u8> {
        let mut p = self.paxueji.lock().unwrap();
        if p.inject.is_empty() {
            return mic;
        }
        let n = mic.len().min(p.inject.len());
        let mut unit: Vec<u8> = p.inject.drain(..n).collect();
        unit.resize(mic.len(), 0);
        unit
    }

    pub(super) fn photo_gen(&self) -> u64 {
        self.paxueji.lock().unwrap().photo_gen
    }

    /// 工具结果回填前调用：发起时的照片已经换掉了，就让模型静默丢弃这条结果。
    pub(super) fn stale_tool_result(&self, gen_at_call: u64, name: &str) -> bool {
        let stale = self.photo_gen() != gen_at_call;
        if stale {
            log::info!(target: "o5.main", "[拍学机] 照片已换，丢弃过期的 {name} 结果");
        }
        stale
    }

    pub(super) fn note_muted_audio(&self) {
        let mut p = self.paxueji.lock().unwrap();
        if !std::mem::replace(&mut p.mute_logged, true) {
            log::info!(target: "o5.main", "[拍学机] 静音中，丢弃口播音频");
        }
    }

    /// 第一段口播文字到达时调用：快门后的第一句记一次延迟。
    pub(super) fn note_speech(&self) {
        if let Some(t) = self.paxueji.lock().unwrap().shutter_at.take() {
            log::info!(target: "o5.main", "[拍学机] 快门→首句 {}ms", t.elapsed().as_millis());
        }
    }

    /// 测试控制通道（环境变量 PAXUEJI_TEST_CTL 指向一个 FIFO）：真机自动化测试用，
    /// 正式启动（桌面图标）不设这个变量就不存在。每行一条命令：
    ///   shutter | shutter_rgb <640x480 RGB 原始文件> | say <文字> |
    ///   say_pcm <16k 单声道 s16 原始音频，替代麦克风上行> | mute
    pub(super) fn spawn_test_control(self: &Arc<Self>) {
        use std::os::unix::fs::FileTypeExt;
        let Ok(path) = std::env::var("PAXUEJI_TEST_CTL") else { return };
        if !std::fs::metadata(&path).is_ok_and(|m| m.file_type().is_fifo()) {
            log::warn!(target: "o5.main", "[测试] {path} 不是 FIFO，测试通道不启用");
            return;
        }
        let weak = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("pax-test-ctl".into()).spawn(move || loop {
            let Ok(text) = std::fs::read_to_string(&path) else {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            };
            let Some(app) = weak.upgrade() else { return };
            for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                log::info!(target: "o5.main", "[测试] {line}");
                app.run_test_command(line);
            }
        });
    }

    fn run_test_command(&self, line: &str) {
        let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
        match cmd {
            "shutter" => self.ui_shutter(),
            "shutter_rgb" => {
                if self.state.is_frozen() {
                    self.ui_shutter();
                }
                match std::fs::read(arg.trim()) {
                    Ok(data) if data.len() == 640 * 480 * 3 => {
                        let frame = PreviewFrame { width: 640, height: 480, data };
                        let score = sharpness(&frame);
                        self.freeze_with(frame, score);
                    }
                    other => log::warn!(target: "o5.main", "[测试] 读图失败 {arg}: {:?}", other.map(|d| d.len())),
                }
            }
            "say_pcm" => match std::fs::read(arg.trim()) {
                Ok(data) => {
                    let mut p = self.paxueji.lock().unwrap();
                    p.inject.extend(data);
                    p.inject.extend(std::iter::repeat_n(0u8, 32000));
                }
                Err(e) => log::warn!(target: "o5.main", "[测试] 读音频失败 {arg}: {e}"),
            },
            "say" => {
                if let Some(s) = self.session.lock().unwrap().as_ref() {
                    s.send_standalone_event(arg);
                }
            }
            "mute" => self.ui_toggle_mute(),
            _ => log::warn!(target: "o5.main", "[测试] 未知命令 {line}"),
        }
    }

    /// 播放期间麦克风是硬门控的（防回声自触发），孩子没法用说话打断，
    /// 所以按快门 / 再拍一张就是「打断」：清掉正在播的话，并让服务端让出话轮。
    fn stop_talking(&self) {
        let audio = self.audio.lock().unwrap();
        if audio.playing(0.5) {
            o5_perf::trace_ev!("interrupt", src = "shutter");
            audio.interrupt_playback();
            drop(audio);
            if let Some(s) = self.session.lock().unwrap().as_ref() {
                s.interrupt();
            }
        }
    }

    /// 实时取景时记下最近的预览帧（主循环约 10Hz 调用；定格中不记）。
    pub(super) fn remember_preview(&self, frame: &PreviewFrame) {
        if self.state.is_frozen() {
            return;
        }
        let mut p = self.paxueji.lock().unwrap();
        if p.recent.len() >= RECENT_CAP {
            p.recent.pop_front();
        }
        p.recent.push_back((Instant::now(), frame.clone()));
    }

    fn sharpest_recent(&self) -> Option<(PreviewFrame, f64)> {
        let mut p = self.paxueji.lock().unwrap();
        let frames = std::mem::take(&mut p.recent);
        drop(p);
        let newest = frames.back()?.0;
        frames
            .into_iter()
            .filter(|(t, _)| newest.duration_since(*t) <= PICK_WINDOW)
            .map(|(_, f)| {
                let s = sharpness(&f);
                (f, s)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// 定格中用定格帧代替实时帧上行（mic 线程每 unit 调一次）。
    /// 每个 unit 上行的画面：定格时是照片，取景时是一张同尺寸的纯灰图。
    /// 服务端要求同一会话帧尺寸不变（协议层的 8×8 黑帧占位会让它直接断会话），
    /// 而取景画面又不能给模型看（否则它会自己对着取景讲），所以用灰图占位。
    pub(super) fn uplink_frame(&self) -> Option<String> {
        static BLANK: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        if let Some(f) = self.paxueji.lock().unwrap().frozen_jpeg.clone() {
            return Some(f);
        }
        BLANK
            .get_or_init(|| {
                PreviewFrame { width: 640, height: 480, data: vec![128; 640 * 480 * 3] }.to_jpeg_b64(60)
            })
            .clone()
    }

    /// 会话被服务端异常断开时自动重连（最多连续 3 次，就绪后清零），
    /// 演示中遇到一次服务端抖动不至于卡在「点我重连」。
    pub(super) fn schedule_reconnect(&self) {
        let attempt = {
            let mut p = self.paxueji.lock().unwrap();
            p.reconnects += 1;
            p.reconnects
        };
        if attempt > 3 {
            log::warn!(target: "o5.main", "[拍学机] 连续重连 3 次失败，等孩子点「点我重连」");
            return;
        }
        let weak = self.weak.clone();
        let _ = std::thread::Builder::new().name("pax-reconnect".into()).spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            if let Some(app) = weak.upgrade() {
                if !app.session_running() {
                    log::info!(target: "o5.main", "[拍学机] 自动重连（第 {attempt} 次）");
                    app.start_session();
                }
            }
        });
    }

    pub(super) fn reset_reconnects(&self) {
        self.paxueji.lock().unwrap().reconnects = 0;
    }

    /// 定格帧随本 unit 上行后，补发快门那句话（只发一次）。
    pub(super) fn flush_shutter_prompt(&self, sess: &o5_session::O5Session) {
        let send = std::mem::take(&mut self.paxueji.lock().unwrap().prompt_pending);
        if send {
            sess.send_standalone_event(SHUTTER_PROMPT);
        }
    }

    /// show_card：只更新屏幕，立即回执（不布防开口兜底，模型此时本就在讲）。
    pub(super) fn handle_show_card(&self, call: &ParsedToolCall) {
        let card = parse_card(&o5_tools::parse_arguments(&call.arguments)).map(|mut c| {
            if c.source.is_none() {
                c.source = self.recent_search_source();
            }
            c
        });
        let reply = match card {
            Some(c) => {
                log::info!(target: "o5.main", "[拍学机] 卡片: {}", c.title);
                self.state.set_card(Some(c));
                "卡片已显示在屏幕上，继续讲解即可。"
            }
            None => "[工具失败] show_card 需要 title 和 points",
        };
        if let Some(s) = self.session.lock().unwrap().as_ref() {
            s.send_tool_result(&call.id, reply);
        }
    }

    pub(super) fn on_search_start(&self) {
        self.state.set_tool_chip("正在联网查…", SEARCH_CHIP_S);
    }

    pub(super) fn on_search_done(&self, result: &str) {
        match first_source_domain(result) {
            Some(domain) => {
                self.state
                    .set_tool_chip(&format!("查到了 · {domain}"), FOUND_CHIP_S);
                self.paxueji.lock().unwrap().last_search_source = Some((domain, Instant::now()));
            }
            None => self.state.set_tool_chip("没查到，换个问法试试", FOUND_CHIP_S),
        }
    }

    fn recent_search_source(&self) -> Option<String> {
        let p = self.paxueji.lock().unwrap();
        p.last_search_source
            .as_ref()
            .filter(|(_, at)| at.elapsed() < SEARCH_SOURCE_TTL)
            .map(|(d, _)| d.clone())
    }
}

/// 清晰度：画面中央一半区域亮度的拉普拉斯方差（越糊越小）。
pub(super) fn sharpness(f: &PreviewFrame) -> f64 {
    let (w, h) = (f.width as usize, f.height as usize);
    if w < 8 || h < 8 || f.data.len() < w * h * 3 {
        return 0.0;
    }
    let luma = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        (f.data[i] as f64 * 0.299) + (f.data[i + 1] as f64 * 0.587) + (f.data[i + 2] as f64 * 0.114)
    };
    let (x0, x1, y0, y1) = (w / 4, w * 3 / 4, h / 4, h * 3 / 4);
    let (mut sum, mut sq, mut n) = (0.0, 0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let lap = 4.0 * luma(x, y) - luma(x - 1, y) - luma(x + 1, y) - luma(x, y - 1) - luma(x, y + 1);
            sum += lap;
            sq += lap * lap;
            n += 1.0;
        }
    }
    let mean = sum / n;
    sq / n - mean * mean
}

/// 会话断开时给孩子看的话（技术原因只进日志）。
pub(super) fn friendly_session_end(closed: bool, reason: &str) -> &'static str {
    const NET_HINTS: [&str; 5] = ["域名解析", "连接失败", "timed out", "超时", "Network"];
    if closed {
        "小松果老师下线了，点右上角「点我重连」"
    } else if NET_HINTS.iter().any(|h| reason.contains(h)) {
        "小松果老师没连上网，检查一下 WiFi，再点右上角「点我重连」"
    } else {
        "小松果老师走神了，点右上角「点我重连」再试一次"
    }
}

pub(super) fn parse_card(args: &Value) -> Option<KnowledgeCard> {
    let title = args.get("title")?.as_str()?.trim().to_string();
    // 真机上模型按 XML 参数出值，数组类型常被吐成裸字符串、服务端反序列化失败，
    // 所以声明成字符串、按分隔符切；兼容数组以防以后改回去。
    let raw: Vec<String> = match args.get("points")? {
        Value::Array(items) => items.iter().filter_map(|p| p.as_str().map(str::to_string)).collect(),
        Value::String(s) => s
            .split(['；', ';', '、', '\n', '|', '｜'])
            .map(str::to_string)
            .collect(),
        _ => return None,
    };
    let points: Vec<String> = raw
        .iter()
        .map(|p| p.trim().trim_start_matches(['·', '-', '•']).trim())
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if title.is_empty() || points.is_empty() {
        return None;
    }
    let source = args
        .get("source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(KnowledgeCard { title, points, source })
}

/// web_search 结果形如「1. 标题（https://baike.baidu.com/…）：摘要」，取第一条的域名。
pub(super) fn first_source_domain(result: &str) -> Option<String> {
    let start = result.find("http")?;
    let rest = &result[start..];
    let host = rest.split("://").nth(1)?;
    let host: String = host
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
        .collect();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn card_needs_title_and_points() {
        let c = parse_card(&json!({"title": "银杏叶", "points": ["秋天变金黄", " "], "source": ""}))
            .unwrap();
        assert_eq!(c.points, vec!["秋天变金黄"]);
        assert_eq!(c.source, None);
        assert!(parse_card(&json!({"title": "银杏叶", "points": []})).is_none());
        let c = parse_card(&json!({"title": "电脑显示器", "points": "显示文字和图像；- 有很多小灯珠；"}))
            .unwrap();
        assert_eq!(c.points, vec!["显示文字和图像", "有很多小灯珠"]);
        assert!(parse_card(&json!({"points": ["a"]})).is_none());
    }

    #[test]
    fn sharpness_separates_detail_from_flat() {
        let (w, h) = (64u32, 48u32);
        let flat = PreviewFrame { width: w, height: h, data: vec![128; (w * h * 3) as usize] };
        let checker: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let v = if ((i % w) / 2 + (i / w) / 2) % 2 == 0 { 0 } else { 255 };
                [v, v, v]
            })
            .collect();
        let sharp = PreviewFrame { width: w, height: h, data: checker };
        assert_eq!(sharpness(&flat), 0.0);
        assert!(sharpness(&sharp) > BLUR_THRESHOLD * 100.0);
    }

    #[test]
    fn network_failure_reads_as_wifi_hint() {
        assert!(friendly_session_end(false, "连接失败: 域名解析失败").contains("WiFi"));
        assert!(friendly_session_end(false, "server_error").contains("走神"));
        assert!(friendly_session_end(true, "").contains("下线"));
    }

    #[test]
    fn source_domain_from_search_text() {
        let r = "搜到 2 条（来源 tavily）：\n1. 银杏（https://www.baike.com/wiki/银杏）：摘要";
        assert_eq!(first_source_domain(r).as_deref(), Some("baike.com"));
        assert_eq!(first_source_domain("[工具失败] 超时"), None);
    }
}

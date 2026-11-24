//! 配置加载 / 校验 / 音色（WAV）加载。
//!
//! 配置文件按以下顺序解析（前者优先）：
//! 1. `--config` 指定的路径
//! 2. 环境变量 `O5_HARNESS_CONFIG`
//! 3. `/root/o5_config.json`
//! 4. 可执行文件所在目录下的 `config.example.json`（内置默认）
//!
//! 以上都不存在时直接使用内置 DEFAULTS（不报错）。结构对齐
//! Glass_Omni_Cpm_Harness_Demo 的 config.example.json
//! （foreground/background/audio/video）。
//!
//! 本 crate 是 Python `o5/config.py` 的忠实移植：
//! - 字段抽取逐行对齐 Python `Config.__init__`，包括 Python
//!   `x or default` 的「假值回退」语义（null/0/""/false/空容器视同未配置）；
//! - 音色 WAV 规范化复刻 `_ensure_wav_16k_mono_pcm16`（含 numpy 的
//!   f32/f64 分层数值语义：均值与无重采样路径在 f32 中量化、重采样路径
//!   在 f64 中量化、rint 取偶舍入），已合规文件原字节直通（保 sha256
//!   与服务器 resume 对账一致）；
//! - 保存为原子写（`<path>.o5tmp` + rename）。
//!
//! 与 Python 的已知差异（详见 README / 移植说明）：
//! - `fg_protocol_version` 按跨 crate 契约存 `u8`（3/4/5），无法承载
//!   Python 里任意字符串的原始值，非法文本在解析期报错而非校验期；
//! - Python `wave` 拒绝而 `hound` 接受 / 反之的极端 WAV（IEEE float、
//!   WAVE_FORMAT_EXTENSIBLE、非 8/16/24/32 位宽）报错细节略有不同；
//! - Python `config.py` 的 `log.warning`（echoMode 非法分支）引用了
//!   未定义的 `log`，实际会 NameError；Rust 版按原意用 `log::warn!`。

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// 任务提示词的「中段」（前后还会拼双工协议 / 委派纪律 / 工具路由 / 委派协议，
/// 见 main.py `_compose_task_prompt`；结构对齐 cpm-harness 训练版 system_prompt）。
pub const DEFAULT_TASK_PROMPT: &str = "你是「小松果老师」，住在松果CAM 里，陪 3 到 12 岁的小朋友认识世界。\n【你能看到什么】小朋友按下快门后画面会定格，你看到的就是这张照片，直到他再拍一张。\n【拍照后怎么讲】\n1. 第一句就说出它是什么，用小朋友听得懂的词，比如“这是一片银杏叶！”。\n2. 同时调用 show_card，把名称和 2 到 3 个要点放到屏幕上，每条要点不超过 12 个字。\n3. 再讲一个好玩的知识点，像讲小故事一样，整段不超过 4 句话。\n4. 最后抛一个小问题，引导小朋友继续观察，比如“你数一数，它有几个叶片？”。\n【说话方式】句子短，语气亲切、有好奇心，多用“你看”“你猜”。不说专业术语，非说不可时马上用一句大白话解释。小朋友插话时立刻停下来，先回答他的问题。\n【不确定时】看不清就直说“我没看清，可以离近一点再拍一次吗？”，不要乱猜。涉及具体数字、年份、最新消息或你不确定的事，先调用 web_search 查，再告诉小朋友“我查到的是……”。不知道就说不知道，不编。\n【看画面】只有小朋友按下快门后你才能看到照片；没按快门时问你“这是什么”，就请他对准以后按一下「拍」。\n【语言】只说中文，不夹英文单词。\n【只回应对你说的话】身边大人之间的聊天、工作安排、打电话这类明显不是在问你的内容，保持安静，不要接话，也不要去查。\n【工具结果】收到内容为【忽略】的工具结果时静默处理，什么都不要说。\n【安全】不讲恐怖、暴力和不适合儿童的内容。遇到刀、药、电源插座、火这类危险东西，先提醒“这个很危险，不要自己碰，要请大人帮忙”。不问小朋友的姓名、住址、学校等个人信息。";

pub const DEFAULT_BACKGROUND_SYSTEM_PROMPT: &str = "你是 MaixCAM2 语音助手的后台模型。完成前台委派的推理任务，返回可口播的简短真实结论。\n可调用工具：web_search（联网搜索实时信息）、map_search（高德地点 / 路线 / 距离）。需要实时信息、地点或路线时先调工具，按工具真实返回回答；不编造搜索结果、坐标或距离。不声称执行了没有实际执行的文件修改或外部操作。需要补充条件时直接说明。";

/// 旧版默认任务提示词指纹：配置里还是这些原文时自动升级到新默认
/// （老部署 `/root/o5_config.json` 里存着旧默认原文，不手工改也能吃到新结构）。
pub const LEGACY_TASK_PROMPTS: &[&str] = &[
    // v3：O5 Voice 通用助手默认（拍学机分叉换成小松果老师）
    "你是运行在 MaixCAM2 开发板上的语音助手。\n【什么时候用工具】\n- 随时随地可直接回答：寒暄、闲聊、解释概念、转述你自己的知识——直接说，不用任何工具。\n- 简单搜索、查地点路线优先调用已启用的前台工具（web_search / map_search / get_current_time）。\n- 需要多步推理、持续执行、写入或修改数据，或前台工具无法完成时，用 delegate_task 委派后台。\n【口语意图】用户说“看一下”“帮我看看”“查一下”“搜一下”这类话并带上对象时（如“看一下今天有什么新闻”“帮我看看附近有没有咖啡店”“看看现在几点”），意思是检索或查询该对象的当前信息，按对象类型直接走对应工具：新闻、天气等实时信息走 web_search，地点、路线走 map_search，时间、日期走 get_current_time，需要多步整理的交给 delegate_task。不要按字面理解成视觉上的张望，也不要回答“我看不了”“我没有摄像头”之类。\n【什么时候不用工具】只聊天、只寒暄，用户没要任何事实/计算/变更时，绝不乱调工具。\n【简洁转述】用简洁、自然的口语概括工具与后台结果，先说与用户需求最相关的结论。按需补充关键条件、必要说明或追问。避免重复、无关铺垫和冗长罗列；只转述已有结果支持的信息。",
    // v1
    "你是运行在 MaixCAM2 开发板上的语音助手。简单对话直接回答；需要深入推理或用户明确要求后台处理时，调用 delegate_task，message 保留用户的完整问题和条件。委派后继续听用户，结果回来前不编造答案，不重复发起同一任务。用户问进度时用 delegate_task 的 task_id 查询，不带 message。后台未配置时如实告知。收到后台结果后简短口头转述。收到【忽略】时保持安静。",
    // v2（无【口语意图】段）：加上前的新默认，用于指纹自动升级
    "你是运行在 MaixCAM2 开发板上的语音助手。\n【什么时候用工具】\n- 随时随地可直接回答：寒暄、闲聊、解释概念、转述你自己的知识——直接说，不用任何工具。\n- 简单搜索、查地点路线优先调用已启用的前台工具（web_search / map_search / get_current_time）。\n- 需要多步推理、持续执行、写入或修改数据，或前台工具无法完成时，用 delegate_task 委派后台。\n【什么时候不用工具】只聊天、只寒暄，用户没要任何事实/计算/变更时，绝不乱调工具。\n【简洁转述】用简洁、自然的口语概括工具与后台结果，先说与用户需求最相关的结论。按需补充关键条件、必要说明或追问。避免重复、无关铺垫和冗长罗列；只转述已有结果支持的信息。",
];

/// 配置错误（对齐 Python `ConfigError`；面向用户的文案与 Python 版一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// 扁平化的运行配置（Python `o5/config.py` `Config` 的忠实移植）。
///
/// 字段与 Python 一一对应；JSON 键为配置文件的嵌套 camelCase 键
/// （`foreground.endpoint` 等），解析时先与 DEFAULTS 深合并再逐字段抽取。
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    // ---- foreground ----
    pub fg_endpoint: String,
    pub fg_api_key: String,
    /// 3 | 4 | 5（JSON 里是 "4" 这样的字符串或数字）
    pub fg_protocol_version: u8,
    /// `protocolVersion: "auto"`：连接前 GET `<端点>/v1/info` 协商生效版本
    /// （见 `o5-session` 的 `deploy_probe`）；true 时 `fg_protocol_version`
    /// 是协商失败/未协商的兜底值（4）。
    pub fg_protocol_auto: bool,
    pub fg_mode: String,
    pub fg_omni_mode: String,
    pub fg_auto_start_session: bool,
    pub fg_queue_gate: bool,
    pub fg_progress_timeout_s: f64,
    pub fg_ack_timeout_s: f64,
    pub fg_connect_timeout_s: f64,
    pub fg_tls_insecure_skip_verify: bool,
    pub fg_shell_prompt: String,
    pub fg_ref_audio_segment: bool,
    pub fg_task_prompt: String,
    pub fg_init_payload: Value,
    pub fg_voice: String,

    // ---- background ----
    pub bg_enabled: bool,
    pub bg_endpoint: String,
    pub bg_api_key: String,
    pub bg_model: String,
    pub bg_timeout_s: f64,
    pub bg_max_concurrent: usize,
    pub bg_system_prompt: String,
    pub bg_max_result_chars: usize,
    pub bg_request_parameters: Value,

    // ---- audio ----
    pub audio_input_volume: u8,
    pub audio_output_volume: u8,
    pub audio_output_sample_rate: u32,
    pub audio_mic_gate_while_playing: bool,
    pub audio_gate_tail_ms: u64,
    pub audio_echo_cancel: bool,
    pub audio_echo_mode: String,
    pub audio_echo_max_delay_ms: f64,
    pub audio_echo_debug_dump: bool,
    pub audio_mic_duck_while_playing: f64,
    /// 近端软件 AGC（audio.nearAgc，Rust 版新增）：arecord 裸路径补
    /// MaixPy Recorder 管线的隐藏回声抑制（快压慢放下行压缩）。
    pub audio_near_agc: bool,
    pub audio_mic_debug_dump: bool,
    pub audio_mic_dump_path: String,
    pub audio_max_session_minutes: f64,

    // ---- video ----
    pub video_enabled: bool,
    pub video_jpeg_quality: u8,
    pub video_width: u32,
    pub video_height: u32,
    pub video_fps: u32,
    pub video_hmirror: bool,
    pub video_vflip: bool,

    // ---- log ----
    pub log_file: String,
    pub log_level: String,

    // ---- tools ----
    pub tools_tavily_api_key: String,
    pub tools_serper_api_key: String,
    pub tools_brave_api_key: String,
    pub tools_amap_api_key: String,
    pub tools_amap_service_host: String,

    // ---- vacuum（可选：Home Assistant 扫地机直连工具）----
    /// 扫地机工具总开关，默认 false（普通用户用不上）。开启 = enabled: true
    /// 且 url+token 非空，三条件齐才把 vacuum_* 工具注入会话。
    pub vacuum_enabled: bool,
    /// HA 基地址，如 `http://192.168.55.34:8123`
    pub vacuum_url: String,
    /// HA 长期访问令牌（Bearer）
    pub vacuum_token: String,
    /// 实体 id；空 = `vacuum.zhui_mi_w10s_pro`
    pub vacuum_entity_id: String,

    // ---- asr ----
    pub asr_enabled: bool,
    pub asr_endpoint: String,
    pub asr_api_key: String,
    pub asr_service_id: i64,
    pub asr_max_reconnects: u32,
    pub asr_gate_while_playing: bool,

    // ---- network ----
    pub net_mode: String,
    pub net_usb_relay_host: String,
    pub net_usb_relay_port_map: BTreeMap<String, u16>,
    /// 公网调用（更多菜单开关）：True = 前台端点域名换成公网网关
    /// `o5gw.modelbest.cn`，协议 / 路径 / Token 等其余不变
    /// （见 `o5-perf::netmode::public_gateway_url`）。后台 / ASR 端点不走这里。
    pub net_public: bool,

    // ---- device ----
    /// 多机混用时区分机器的标识（顶栏徽章，如 "02"）
    pub device_label: String,
    /// 触摸坐标映射变体（0-7；本仓库设备 03 未标定，默认 2）
    pub device_touch_variant: u8,
    /// 空串 = `<exe_dir>/feeder_display.py`
    pub device_display_feeder_path: String,
    /// 空串 = `<exe_dir>/feeder_camera.py`
    pub device_camera_feeder_path: String,

    // ---- 缓存（非配置字段）----
    voice: Option<Voice>,
    source_path: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config::defaults()
    }
}

impl Config {
    /// 内置默认（对齐 Python `DEFAULTS`）。
    pub fn defaults() -> Config {
        // DEFAULTS 与空覆盖深合并 = DEFAULTS 本身
        Config::from_user_value(json!({})).expect("内置默认配置必然可解析")
    }

    /// 从 JSON 文本构造（不校验；等价于 Python `Config(raw)`）。
    pub fn from_json_str(s: &str) -> Result<Config, ConfigError> {
        let user: Value =
            serde_json::from_str(s).map_err(|e| ConfigError(format!("配置不是有效的 JSON: {e}")))?;
        Config::from_user_value(user)
    }

    /// 搜索并加载配置。
    ///
    /// 搜索顺序（前者优先）：`cli_path`、环境变量 `O5_HARNESS_CONFIG`、
    /// `/root/o5_config.json`、`<exe_dir>/config.example.json`、内置默认。
    /// 文件不存在时用默认（不报错）；找到文件时解析后执行
    /// [`Config::validate`]（对齐 Python `load_config`）。
    pub fn load(cli_path: Option<&str>) -> Result<Config, ConfigError> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(p) = cli_path {
            if !p.is_empty() {
                candidates.push(PathBuf::from(p));
            }
        }
        if let Ok(env) = std::env::var("O5_HARNESS_CONFIG") {
            if !env.is_empty() {
                candidates.push(PathBuf::from(env));
            }
        }
        candidates.push(PathBuf::from("/root/o5_config.json"));
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("config.example.json"));
            }
        }
        for p in &candidates {
            if !p.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(p)
                .map_err(|e| ConfigError(format!("读取配置文件 {} 失败: {}", p.display(), e)))?;
            let user: Value = serde_json::from_str(&text).map_err(|e| {
                ConfigError(format!("配置文件 {} 不是有效的 JSON: {}", p.display(), e))
            })?;
            let mut cfg = Config::from_user_value(user)?;
            cfg.source_path = Some(p.to_string_lossy().into_owned());
            cfg.validate()?;
            return Ok(cfg);
        }
        // 没有任何配置文件：直接用内置默认（不校验，对齐 Python load_config）
        Ok(Config::defaults())
    }

    /// 原子保存：写 `<path>.o5tmp` 后 rename（嵌套结构与配置文件一致）。
    pub fn save(&self, path: &str) -> Result<(), ConfigError> {
        let text = serde_json::to_string_pretty(&self.to_json_value())
            .map_err(|e| ConfigError(format!("序列化配置失败: {e}")))?;
        let tmp = format!("{path}.o5tmp");
        std::fs::write(&tmp, text)
            .map_err(|e| ConfigError(format!("写入配置文件 {tmp} 失败: {e}")))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| ConfigError(format!("保存配置文件 {path} 失败: {e}")))?;
        Ok(())
    }

    /// 配置合法性校验（错误文案与 Python `validate` 逐字一致）。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.fg_endpoint.is_empty() {
            return Err(ConfigError("foreground.endpoint 未配置（WebSocket 地址）".into()));
        }
        if !(self.fg_endpoint.starts_with("ws://") || self.fg_endpoint.starts_with("wss://")) {
            return Err(ConfigError("foreground.endpoint 必须以 ws:// 或 wss:// 开头".into()));
        }
        if !self.fg_protocol_auto && !(3..=5).contains(&self.fg_protocol_version) {
            return Err(ConfigError(format!(
                "foreground.protocolVersion 只支持 3/4/5/auto，当前 {}",
                self.fg_protocol_version
            )));
        }
        if !matches!(self.fg_mode.as_str(), "audio" | "omni" | "video") {
            return Err(ConfigError("foreground.mode 只支持 audio/omni/video".into()));
        }
        if self.bg_enabled && !self.bg_endpoint.starts_with("http") {
            return Err(ConfigError("background.endpoint 必须是 http(s) 地址".into()));
        }
        if self.net_mode == "usb"
            && (self.net_usb_relay_host.is_empty() || self.net_usb_relay_port_map.is_empty())
        {
            return Err(ConfigError(
                "network.mode=usb 需要 network.usbRelay.host 和 portMap（电脑中继配置）".into(),
            ));
        }
        Ok(())
    }

    /// 当前媒体模式（UI 可在 audio 与 omni 之间切换后调用）。
    pub fn effective_media_mode(&self) -> &str {
        &self.fg_mode
    }

    /// 连接 URL 上的 mode 参数：omni 档位按配置取 omni/video，audio 档固定 audio。
    pub fn media_mode_param(&self) -> &str {
        if self.fg_mode == "audio" {
            "audio"
        } else if self.fg_mode == "omni" {
            &self.fg_omni_mode
        } else if self.fg_mode == "video" {
            "video"
        } else {
            "audio"
        }
    }

    /// 是否需要相机帧。
    pub fn with_video(&self) -> bool {
        self.fg_mode != "audio" && self.video_enabled
    }

    /// 缓存的音色（None = 未配置 / 未加载）。
    pub fn voice(&self) -> Option<&Voice> {
        self.voice.as_ref()
    }

    /// 按 `fg_voice` 加载并缓存音色；spec 为空时不加载（缓存保持 None）。
    pub fn load_voice(&mut self, assets_dir: &Path) -> Result<(), ConfigError> {
        if self.fg_voice.is_empty() {
            self.voice = None;
            return Ok(());
        }
        self.voice = Some(Voice::load(&self.fg_voice, assets_dir)?);
        Ok(())
    }

    /// 实际加载配置时使用的文件路径（未从文件加载时为 None）。
    pub fn source_path(&self) -> Option<&str> {
        self.source_path.as_deref()
    }

    /// 实际使用的显示 feeder 路径：配置了就用配置值，否则 `<exe_dir>/feeder_display.py`。
    pub fn display_feeder_path(&self) -> PathBuf {
        resolve_feeder_path(&self.device_display_feeder_path, "feeder_display.py")
    }

    /// 实际使用的相机 feeder 路径：配置了就用配置值，否则 `<exe_dir>/feeder_camera.py`。
    pub fn camera_feeder_path(&self) -> PathBuf {
        resolve_feeder_path(&self.device_camera_feeder_path, "feeder_camera.py")
    }

    // ------------------------------------------------------------------
    // 构造：DEFAULTS 深合并 + 逐字段抽取（对齐 Python Config.__init__）
    // ------------------------------------------------------------------

    fn from_user_value(user: Value) -> Result<Config, ConfigError> {
        let mut d = defaults_value();
        if !user.is_null() {
            if !user.is_object() {
                return Err(ConfigError("配置必须是 JSON 对象".into()));
            }
            deep_merge(&mut d, &user);
        }

        let fg = required_section(&d, "foreground")?;
        let bg = required_section(&d, "background")?;
        let au = required_section(&d, "audio")?;
        let vi = required_section(&d, "video")?;
        let lo = required_section(&d, "log")?;
        let ar = required_section(&d, "asr")?;
        let nw = required_section(&d, "network")?;
        let tl = soft_section(&d, "tools")?;
        let dv = soft_section(&d, "device")?;
        let vc = soft_section(&d, "vacuum")?;

        // ---- foreground ----
        let fg_endpoint = s_trim(g(fg, "endpoint"), "foreground.endpoint", "")?;
        let fg_api_key = s_trim(g(fg, "apiKey"), "foreground.apiKey", "")?;
        let (fg_protocol_version, fg_protocol_auto) =
            parse_protocol_version(g(fg, "protocolVersion"))?;
        let fg_mode =
            s_trim(g(fg, "mode"), "foreground.mode", "audio")?.to_lowercase();
        let fg_omni_mode =
            s_trim(g(fg, "omniMode"), "foreground.omniMode", "omni")?.to_lowercase();
        let fg_auto_start_session = py_truthy(g(fg, "autoStartSession"));
        let fg_queue_gate = py_truthy(g(fg, "queueGate"));
        let fg_progress_timeout_s =
            py_float(g(fg, "progressTimeoutSeconds"), "foreground.progressTimeoutSeconds", 60.0)?;
        let fg_ack_timeout_s =
            py_float(g(fg, "ackTimeoutSeconds"), "foreground.ackTimeoutSeconds", 30.0)?;
        let fg_connect_timeout_s = py_float(
            g(fg, "connectTimeoutSeconds"),
            "foreground.connectTimeoutSeconds",
            15.0,
        )?;
        let fg_tls_insecure_skip_verify = py_truthy(g(fg, "tlsInsecureSkipVerify"));
        let fg_shell_prompt = py_str(g(fg, "shellPrompt"), "foreground.shellPrompt", "")?;
        let fg_ref_audio_segment = py_truthy(g(fg, "refAudioSegment"));
        let mut fg_task_prompt =
            py_str(g(fg, "taskPrompt"), "foreground.taskPrompt", DEFAULT_TASK_PROMPT)?;
        if LEGACY_TASK_PROMPTS.contains(&fg_task_prompt.as_str()) {
            // 旧默认指纹 -> 新默认
            fg_task_prompt = DEFAULT_TASK_PROMPT.to_string();
        }
        let fg_init_payload = payload_or_empty(g(fg, "initPayload"));
        let fg_voice = s_trim(g(fg, "voice"), "foreground.voice", "")?;

        // ---- background ----
        let bg_endpoint = s_trim(g(bg, "endpoint"), "background.endpoint", "")?;
        let bg_enabled = py_truthy(g(bg, "enabled")) && !bg_endpoint.is_empty();
        let bg_api_key = s_trim(g(bg, "apiKey"), "background.apiKey", "")?;
        let bg_model = s_trim(g(bg, "model"), "background.model", "")?;
        let bg_timeout_s =
            py_float(g(bg, "timeoutSeconds"), "background.timeoutSeconds", 120.0)?;
        let bg_max_concurrent = fit(
            "background.maxConcurrent",
            py_int(g(bg, "maxConcurrent"), "background.maxConcurrent", 2)?,
        )?;
        let bg_system_prompt = py_str(
            g(bg, "systemPrompt"),
            "background.systemPrompt",
            DEFAULT_BACKGROUND_SYSTEM_PROMPT,
        )?;
        let bg_max_result_chars = fit(
            "background.maxResultChars",
            py_int(g(bg, "maxResultChars"), "background.maxResultChars", 1200)?,
        )?;
        let bg_request_parameters = object_or_empty(g(bg, "requestParameters"));

        // ---- audio ----
        let audio_input_volume = fit(
            "audio.inputVolume",
            py_int(g(au, "inputVolume"), "audio.inputVolume", 100)?,
        )?;
        let audio_output_volume = fit(
            "audio.outputVolume",
            py_int(g(au, "outputVolume"), "audio.outputVolume", 80)?,
        )?;
        let audio_output_sample_rate = fit(
            "audio.outputSampleRate",
            py_int(g(au, "outputSampleRate"), "audio.outputSampleRate", 24000)?,
        )?;
        let audio_mic_gate_while_playing = py_truthy(g(au, "micGateWhilePlaying"));
        let audio_gate_tail_ms = fit(
            "audio.gateTailMs",
            py_int(g(au, "gateTailMs"), "audio.gateTailMs", 200)?,
        )?;
        let audio_echo_cancel = py_truthy(g(au, "echoCancel"));
        let mut audio_echo_mode =
            s_trim(g(au, "echoMode"), "audio.echoMode", "")?.to_lowercase();
        if !matches!(audio_echo_mode.as_str(), "" | "nlms" | "subband" | "off") {
            log::warn!("audio.echoMode 非法（{}），忽略", audio_echo_mode);
            audio_echo_mode.clear();
        }
        let audio_echo_max_delay_explicit = py_float_parse(g(au, "echoMaxDelayMs"));
        let audio_echo_debug_dump = py_truthy(g(au, "echoDebugDump"));
        let audio_mic_duck_explicit = py_float_parse(g(au, "micDuckWhilePlaying"));
        // nearAgc：Rust 版新增键，Python 版无此语义，默认开
        let audio_near_agc = au.get("nearAgc").is_none_or(py_truthy);
        let audio_mic_debug_dump = py_truthy(g(au, "micDebugDump"));
        let audio_mic_dump_path = py_str(
            g(au, "micDumpPath"),
            "audio.micDumpPath",
            "/root/o5_harness/o5_mic_debug.pcm",
        )?;
        let audio_max_session_minutes =
            py_float(g(au, "maxSessionMinutes"), "audio.maxSessionMinutes", 0.0)?;

        // ---- video ----
        let video_enabled = py_truthy(g(vi, "enabled"));
        let video_jpeg_quality =
            fit("video.jpegQuality", py_int(g(vi, "jpegQuality"), "video.jpegQuality", 60)?)?;
        let video_width = fit("video.width", py_int(g(vi, "width"), "video.width", 512)?)?;
        let video_height = fit("video.height", py_int(g(vi, "height"), "video.height", 384)?)?;
        let video_fps = fit("video.fps", py_int(g(vi, "fps"), "video.fps", 15)?)?;
        let video_hmirror = py_truthy(g(vi, "hmirror"));
        let video_vflip = py_truthy(g(vi, "vflip"));

        // ---- device（Python 里为可选节）----
        let device_label = s_trim(gm(dv, "label"), "device.label", "")?;
        let device_touch_variant = py_int(gm(dv, "touchVariant"), "device.touchVariant", 2)
            .ok()
            .and_then(|v| u8::try_from(v).ok())
            .unwrap_or(2);

        // 设备音频标定表：同型机器的硬件个体差异（回声延迟/近场耦合强度）
        // 按 device.label 内建差异化参数，免得每台手改 json。优先级：
        // 设备 json 显式值 > 本表 > 全局默认。03 实测回声延迟 ~975ms，贴着
        // 默认 1000ms 搜索窗顶导致 AEC 锁不住；近场耦合也更强，duck 压深。
        let profile = device_audio_profile(&device_label);
        let audio_echo_max_delay_ms = audio_echo_max_delay_explicit
            .or_else(|| profile.as_ref().map(|p| p.echo_max_delay_ms))
            .unwrap_or(1000.0)
            .max(100.0);
        let audio_mic_duck_while_playing = audio_mic_duck_explicit
            .or_else(|| profile.as_ref().map(|p| p.mic_duck_while_playing))
            .unwrap_or(0.4)
            .clamp(0.0, 1.0);
        let device_display_feeder_path =
            s_trim(gm(dv, "displayFeederPath"), "device.displayFeederPath", "")?;
        let device_camera_feeder_path =
            s_trim(gm(dv, "cameraFeederPath"), "device.cameraFeederPath", "")?;

        // ---- log ----
        let log_file = py_str(g(lo, "file"), "log.file", "")?;
        let log_level = py_str(g(lo, "level"), "log.level", "info")?.to_lowercase();

        // ---- tools（Python 里为可选节）----
        let tools_tavily_api_key =
            s_trim(gm(tl, "tavilyApiKey"), "tools.tavilyApiKey", "")?;
        let tools_serper_api_key =
            s_trim(gm(tl, "serperApiKey"), "tools.serperApiKey", "")?;
        let tools_brave_api_key = s_trim(gm(tl, "braveApiKey"), "tools.braveApiKey", "")?;
        let tools_amap_api_key = s_trim(gm(tl, "amapApiKey"), "tools.amapApiKey", "")?;
        let tools_amap_service_host =
            s_trim(gm(tl, "amapServiceHost"), "tools.amapServiceHost", "")?;

        // ---- vacuum（可选节；enabled 且 url+token 都非空才启用扫地机工具，
        //      默认关闭——普通用户可能用不上，需要显式设置开启）----
        let vacuum_enabled = match vc {
            Some(m) => py_truthy(g(m, "enabled")),
            None => false,
        };
        let vacuum_url = s_trim(gm(vc, "url"), "vacuum.url", "")?;
        let vacuum_token = s_trim(gm(vc, "token"), "vacuum.token", "")?;
        let vacuum_entity_id =
            s_trim(gm(vc, "entityId"), "vacuum.entityId", "vacuum.zhui_mi_w10s_pro")?;

        // ---- asr ----
        let asr_endpoint = s_trim(g(ar, "endpoint"), "asr.endpoint", "")?;
        let asr_api_key = s_trim(g(ar, "apiKey"), "asr.apiKey", "")?;
        let asr_enabled = py_truthy(g(ar, "enabled"))
            && !asr_endpoint.is_empty()
            && !asr_api_key.is_empty();
        let asr_service_id = if py_truthy(g(ar, "serviceId")) {
            py_int_parse(g(ar, "serviceId")).unwrap_or(0)
        } else {
            0
        };
        let asr_max_reconnects = fit(
            "asr.maxReconnects",
            py_int(g(ar, "maxReconnects"), "asr.maxReconnects", 3)?,
        )?;
        let asr_gate_while_playing = py_truthy(g(ar, "gateWhilePlaying"));

        // ---- network ----
        let mut net_mode = s_trim(g(nw, "mode"), "network.mode", "wifi")?.to_lowercase();
        if !matches!(net_mode.as_str(), "wifi" | "usb") {
            net_mode = "wifi".to_string();
        }
        // network.usbRelay（Python `nw.get("usbRelay") or {}`：可缺省，真值非对象报错）
        let relay_val = g(nw, "usbRelay");
        let relay: Option<&Map<String, Value>> = if py_truthy(relay_val) {
            Some(relay_val.as_object().ok_or_else(|| {
                ConfigError("配置字段 network.usbRelay 必须是对象".into())
            })?)
        } else {
            None
        };
        let net_usb_relay_host =
            s_trim(gm(relay, "host"), "network.usbRelay.host", "")?;
        let mut net_usb_relay_port_map = BTreeMap::new();
        if let Some(pm) = relay.and_then(|m| m.get("portMap")).and_then(Value::as_object) {
            for (k, v) in pm {
                if let Some(port) = py_int_parse(v).and_then(|p| u16::try_from(p).ok()) {
                    net_usb_relay_port_map.insert(k.trim().to_lowercase(), port);
                }
            }
        }
        // 公网调用开关（更多菜单持久化；Python `bool(nw.get("publicNetwork", False))`）
        let net_public = py_truthy(g(nw, "publicNetwork"));

        Ok(Config {
            fg_endpoint,
            fg_api_key,
            fg_protocol_version,
            fg_protocol_auto,
            fg_mode,
            fg_omni_mode,
            fg_auto_start_session,
            fg_queue_gate,
            fg_progress_timeout_s,
            fg_ack_timeout_s,
            fg_connect_timeout_s,
            fg_tls_insecure_skip_verify,
            fg_shell_prompt,
            fg_ref_audio_segment,
            fg_task_prompt,
            fg_init_payload,
            fg_voice,
            bg_enabled,
            bg_endpoint,
            bg_api_key,
            bg_model,
            bg_timeout_s,
            bg_max_concurrent,
            bg_system_prompt,
            bg_max_result_chars,
            bg_request_parameters,
            audio_input_volume,
            audio_output_volume,
            audio_output_sample_rate,
            audio_mic_gate_while_playing,
            audio_gate_tail_ms,
            audio_echo_cancel,
            audio_echo_mode,
            audio_echo_max_delay_ms,
            audio_echo_debug_dump,
            audio_mic_duck_while_playing,
            audio_near_agc,
            audio_mic_debug_dump,
            audio_mic_dump_path,
            audio_max_session_minutes,
            video_enabled,
            video_jpeg_quality,
            video_width,
            video_height,
            video_fps,
            video_hmirror,
            video_vflip,
            log_file,
            log_level,
            tools_tavily_api_key,
            tools_serper_api_key,
            tools_brave_api_key,
            tools_amap_api_key,
            tools_amap_service_host,
            vacuum_enabled,
            vacuum_url,
            vacuum_token,
            vacuum_entity_id,
            asr_enabled,
            asr_endpoint,
            asr_api_key,
            asr_service_id,
            asr_max_reconnects,
            asr_gate_while_playing,
            net_mode,
            net_usb_relay_host,
            net_usb_relay_port_map,
            net_public,
            device_label,
            device_touch_variant,
            device_display_feeder_path,
            device_camera_feeder_path,
            voice: None,
            source_path: None,
        })
    }

    /// 序列化为配置文件的嵌套 JSON 结构（camelCase 键）。
    fn to_json_value(&self) -> Value {
        json!({
            "version": 1,
            "foreground": {
                "endpoint": self.fg_endpoint,
                "apiKey": self.fg_api_key,
                "protocolVersion": if self.fg_protocol_auto {
                    "auto".to_string()
                } else {
                    self.fg_protocol_version.to_string()
                },
                "mode": self.fg_mode,
                "omniMode": self.fg_omni_mode,
                "autoStartSession": self.fg_auto_start_session,
                "queueGate": self.fg_queue_gate,
                "progressTimeoutSeconds": num(self.fg_progress_timeout_s),
                "ackTimeoutSeconds": num(self.fg_ack_timeout_s),
                "connectTimeoutSeconds": num(self.fg_connect_timeout_s),
                "tlsInsecureSkipVerify": self.fg_tls_insecure_skip_verify,
                "shellPrompt": self.fg_shell_prompt,
                "refAudioSegment": self.fg_ref_audio_segment,
                "taskPrompt": self.fg_task_prompt,
                "initPayload": self.fg_init_payload,
                "voice": self.fg_voice,
            },
            "background": {
                "enabled": self.bg_enabled,
                "endpoint": self.bg_endpoint,
                "apiKey": self.bg_api_key,
                "model": self.bg_model,
                "timeoutSeconds": num(self.bg_timeout_s),
                "maxConcurrent": self.bg_max_concurrent,
                "systemPrompt": self.bg_system_prompt,
                "maxResultChars": self.bg_max_result_chars,
                "requestParameters": self.bg_request_parameters,
            },
            "audio": {
                "inputVolume": self.audio_input_volume,
                "outputVolume": self.audio_output_volume,
                "outputSampleRate": self.audio_output_sample_rate,
                "micGateWhilePlaying": self.audio_mic_gate_while_playing,
                "gateTailMs": self.audio_gate_tail_ms,
                "echoCancel": self.audio_echo_cancel,
                "echoMode": self.audio_echo_mode,
                "echoMaxDelayMs": num(self.audio_echo_max_delay_ms),
                "echoDebugDump": self.audio_echo_debug_dump,
                "micDuckWhilePlaying": self.audio_mic_duck_while_playing,
                "nearAgc": self.audio_near_agc,
                "micDebugDump": self.audio_mic_debug_dump,
                "micDumpPath": self.audio_mic_dump_path,
                "maxSessionMinutes": num(self.audio_max_session_minutes),
            },
            "video": {
                "enabled": self.video_enabled,
                "jpegQuality": self.video_jpeg_quality,
                "width": self.video_width,
                "height": self.video_height,
                "fps": self.video_fps,
                "hmirror": self.video_hmirror,
                "vflip": self.video_vflip,
            },
            "log": {
                "file": self.log_file,
                "level": self.log_level,
            },
            "tools": {
                "tavilyApiKey": self.tools_tavily_api_key,
                "serperApiKey": self.tools_serper_api_key,
                "braveApiKey": self.tools_brave_api_key,
                "amapApiKey": self.tools_amap_api_key,
                "amapServiceHost": self.tools_amap_service_host,
            },
            "vacuum": {
                "enabled": self.vacuum_enabled,
                "url": self.vacuum_url,
                "token": self.vacuum_token,
                "entityId": self.vacuum_entity_id,
            },
            "asr": {
                "enabled": self.asr_enabled,
                "endpoint": self.asr_endpoint,
                "apiKey": self.asr_api_key,
                "serviceId": self.asr_service_id,
                "maxReconnects": self.asr_max_reconnects,
                "gateWhilePlaying": self.asr_gate_while_playing,
            },
            "network": {
                "mode": self.net_mode,
                "usbRelay": {
                    "host": self.net_usb_relay_host,
                    "portMap": self.net_usb_relay_port_map,
                },
                "publicNetwork": self.net_public,
            },
            "device": {
                "label": self.device_label,
                "touchVariant": self.device_touch_variant,
                "displayFeederPath": self.device_display_feeder_path,
                "cameraFeederPath": self.device_camera_feeder_path,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// 音色（WAV）加载
// ---------------------------------------------------------------------------

/// 一个已加载的参考音色：规范化后的 16k 单声道 PCM16 WAV 字节及其摘要。
///
/// `spec` 为空（或 `builtin:` 后为空）表示不使用参考音色，此时
/// `data` / `sha256` / `data_b64` 均为空（[`Voice::is_empty`] 为真）。
#[derive(Debug, Clone, PartialEq)]
pub struct Voice {
    /// 原始配置 spec：`""` | `"builtin:<name>"` | 路径
    pub spec: String,
    /// 规范化后的 16k mono PCM16 WAV 字节（合规文件原样直通）
    pub data: Vec<u8>,
    /// `data` 的 hex sha256
    pub sha256: String,
    /// `data` 的 base64
    pub data_b64: String,
}

impl Voice {
    /// 解析并加载音色。
    ///
    /// spec：`""` / `"builtin:"` → 不加载（返回空 Voice，不报错）；
    /// `builtin:<name>` → `<assets_dir>/<name>.wav`（无后缀自动补）；
    /// 其他 → 按路径。文件不存在报 `音色文件不存在: <path>`。
    pub fn load(spec: &str, assets_dir: &Path) -> Result<Voice, ConfigError> {
        let Some(path) = resolve_voice_path(spec, assets_dir) else {
            return Ok(Voice::empty(spec));
        };
        if !path.is_file() {
            return Err(ConfigError(format!("音色文件不存在: {}", path.display())));
        }
        let raw = std::fs::read(&path)
            .map_err(|e| ConfigError(format!("读取音色文件 {} 失败: {}", path.display(), e)))?;
        let data = normalize_wav_16k_mono_pcm16(&raw, &path)?;
        Ok(Voice::from_data(spec, data))
    }

    /// 是否为「不使用参考音色」的空音色。
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn empty(spec: &str) -> Voice {
        Voice {
            spec: spec.to_string(),
            data: Vec::new(),
            sha256: String::new(),
            data_b64: String::new(),
        }
    }

    fn from_data(spec: &str, data: Vec<u8>) -> Voice {
        let mut h = Sha256::new();
        h.update(&data);
        Voice {
            spec: spec.to_string(),
            sha256: hex_lower(&h.finalize()),
            data_b64: base64::engine::general_purpose::STANDARD.encode(&data),
            data,
        }
    }
}

/// voice 配置：`''` → None（不加载）；`builtin:<name>` → 内置资源；其他按路径。
pub fn resolve_voice_path(spec: &str, builtin_dir: &Path) -> Option<PathBuf> {
    if spec.is_empty() {
        return None;
    }
    if let Some(name) = spec.strip_prefix("builtin:") {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let file = if name.ends_with(".wav") {
            name.to_string()
        } else {
            format!("{name}.wav")
        };
        return Some(builtin_dir.join(file));
    }
    Some(PathBuf::from(spec))
}

/// 读取 WAV 字节并保证 16kHz 单声道 PCM16（Python `_ensure_wav_16k_mono_pcm16`）。
///
/// 已经符合规范的文件原字节直通（保 sha256 与服务器 resume 对账一致）；
/// 其余转换后重打包。非 PCM WAV（如 IEEE float / 压缩格式）报错。
fn normalize_wav_16k_mono_pcm16(raw: &[u8], path: &Path) -> Result<Vec<u8>, ConfigError> {
    let wav_err =
        |e: String| ConfigError(format!("音色 {} 不是可解析的 PCM WAV: {}", path.display(), e));
    let mut reader =
        hound::WavReader::new(std::io::Cursor::new(raw)).map_err(|e| wav_err(e.to_string()))?;
    let spec = reader.spec();
    if spec.sample_format == hound::SampleFormat::Float {
        // Python wave 模块不接受 IEEE float（格式 tag 3）
        return Err(wav_err("unknown format: 3".into()));
    }
    let n_ch = spec.channels as usize;
    let bits = spec.bits_per_sample as usize;
    let rate = spec.sample_rate;
    if n_ch == 1 && bits == 16 && rate == 16000 {
        return Ok(raw.to_vec()); // 原字节直通
    }
    let sampwidth = bits / 8;
    if !matches!(sampwidth, 1 | 2 | 4) {
        return Err(ConfigError(format!(
            "音色 {} 采样宽度 {}bit 不支持",
            path.display(),
            sampwidth * 8
        )));
    }

    // 1) 读交织采样并转 f32（对齐 numpy astype(np.float32) 的数值语义）。
    //    注意照抄 Python：S32 不除以满量程（astype 原值保留），
    //    S16 / 32768、U8 (x-128)/128。
    let mut samples_f32: Vec<f32> = Vec::new();
    match sampwidth {
        2 => {
            for s in reader.samples::<i16>() {
                let s = s.map_err(|e| wav_err(e.to_string()))?;
                samples_f32.push(s as f32 / 32768.0);
            }
        }
        4 => {
            for s in reader.samples::<i32>() {
                let s = s.map_err(|e| wav_err(e.to_string()))?;
                samples_f32.push(s as f32);
            }
        }
        1 => {
            for s in reader.samples::<i8>() {
                let s = s.map_err(|e| wav_err(e.to_string()))?;
                // hound 的 i8 采样 = 原字节 - 128（signed_from_u8），
                // 恰为 Python (u8 - 128) / 128 的分子
                samples_f32.push(s as f32 / 128.0);
            }
        }
        _ => unreachable!("sampwidth 已限定 1/2/4"),
    }

    // 2) 多声道均值混单声道（f32 顺序求和，对齐 np.mean(axis=1) 小数组路径）。
    let n_frames = samples_f32.len() / n_ch;
    let mono: Vec<f32> = if n_ch > 1 {
        (0..n_frames)
            .map(|i| {
                let mut acc = 0.0f32;
                for c in 0..n_ch {
                    acc += samples_f32[i * n_ch + c];
                }
                acc / n_ch as f32
            })
            .collect()
    } else {
        samples_f32
    };

    // 3) 任意采样率线性插值重采样到 16k（np.interp：f64 算术），
    //    clip ±1.0，rint(x*32767)（round-half-to-even）。
    //    无重采样路径 numpy 在 f32 中量化（结果与 Python 逐位一致），
    //    重采样路径 np.interp 返回 f64、在 f64 中量化。
    let pcm_i16: Vec<i16> = if rate != 16000 {
        // Python int(len * 16000 / rate)：f64 乘除后向零截断
        let n_out = (mono.len() as f64 * 16000.0 / f64::from(rate)) as usize;
        np_interp_linspace(&mono, n_out)
            .into_iter()
            .map(|x| {
                let c = x.clamp(-1.0, 1.0);
                (c * 32767.0).round_ties_even() as i16
            })
            .collect()
    } else {
        mono.iter()
            .map(|&x| {
                let c = x.clamp(-1.0, 1.0);
                (c * 32767.0f32).round_ties_even() as i16
            })
            .collect()
    };

    // 4) 重打包 WAV（与 Python wave 模块输出逐字节一致）
    let mut pcm_bytes = Vec::with_capacity(pcm_i16.len() * 2);
    for s in &pcm_i16 {
        pcm_bytes.extend_from_slice(&s.to_le_bytes());
    }
    Ok(pack_wav_16k_mono_pcm16(&pcm_bytes))
}

/// `np.interp(np.linspace(0, len-1, n_out), arange(len), fp)` 的逐位复刻
/// （fp 为 f32 时 numpy 内部提升到 f64 计算，返回 f64）。
fn np_interp_linspace(fp: &[f32], n_out: usize) -> Vec<f64> {
    let n = fp.len();
    if n_out == 0 {
        return Vec::new();
    }
    if n <= 1 {
        // np.interp 对单点 fp 返回常数
        let v = fp.first().map(|&x| f64::from(x)).unwrap_or(0.0);
        return vec![v; n_out];
    }
    let stop = (n - 1) as f64;
    let step = stop / (n_out - 1) as f64;
    let mut out = Vec::with_capacity(n_out);
    for i in 0..n_out {
        // np.linspace：y[i] = i*step + start（start=0），末点精确取 stop；num==1 时取 start
        let x = if n_out == 1 {
            0.0
        } else if i == n_out - 1 {
            stop
        } else {
            f64::from(i as u32) * step
        };
        if x >= stop {
            // np.interp：x >= xp[-1] 时取右端点
            out.push(f64::from(fp[n - 1]));
            continue;
        }
        let j = (x as usize).min(n - 2); // xp 为整数等距，j = floor(x)
        let fj = f64::from(fp[j]);
        let fj1 = f64::from(fp[j + 1]);
        // slope = (fp[j+1]-fp[j]) / (xp[j+1]-xp[j])，除数为 1.0（精确恒等）
        out.push((fj1 - fj) * (x - f64::from(j as u32)) + fj);
    }
    out
}

/// 与 Python `wave` 模块写出的 16k mono PCM16 WAV 逐字节一致。
fn pack_wav_16k_mono_pcm16(pcm: &[u8]) -> Vec<u8> {
    let data_len = pcm.len();
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32 + data_len as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    out.extend_from_slice(&16000u32.to_le_bytes());
    out.extend_from_slice(&32000u32.to_le_bytes()); // 每秒字节数
    out.extend_from_slice(&2u16.to_le_bytes()); // 块对齐
    out.extend_from_slice(&16u16.to_le_bytes()); // 位宽
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

// ---------------------------------------------------------------------------
// JSON 工具：DEFAULTS / 深合并 / Python 式取值
// ---------------------------------------------------------------------------

/// Python `DEFAULTS`（深合并的基底）。
fn defaults_value() -> Value {
    json!({
        "version": 1,
        "foreground": {
            "endpoint": "wss://47.95.219.248:7029/e/merge1006-128k-300step/v1/realtime",
            "apiKey": "",
            "protocolVersion": "4",
            "mode": "audio",
            "omniMode": "omni",
            "autoStartSession": false,
            "queueGate": true,
            "progressTimeoutSeconds": 60,
            "ackTimeoutSeconds": 30,
            "connectTimeoutSeconds": 15,
            "tlsInsecureSkipVerify": true,
            "shellPrompt": "",
            "refAudioSegment": true,
            "taskPrompt": DEFAULT_TASK_PROMPT,
            "initPayload": {},
            "voice": "builtin:ref_minicpm_signature_fix.wav",
        },
        "background": {
            "enabled": false,
            "endpoint": "",
            "apiKey": "",
            "model": "",
            "timeoutSeconds": 120,
            "maxConcurrent": 2,
            "systemPrompt": DEFAULT_BACKGROUND_SYSTEM_PROMPT,
            "maxResultChars": 1200,
            "requestParameters": {},
        },
        "audio": {
            "inputVolume": 100,
            "outputVolume": 80,
            "outputSampleRate": 24000,
            "micGateWhilePlaying": false,
            "gateTailMs": 200,
            "echoCancel": true,
            "echoMode": "",
            "echoDebugDump": false,
            "nearAgc": true,
            "micDebugDump": false,
            "micDumpPath": "/root/o5_harness/o5_mic_debug.pcm",
            "maxSessionMinutes": 0,
        },
        "video": {
            "enabled": true,
            "jpegQuality": 60,
            "width": 512,
            "height": 384,
            "fps": 15,
            "hmirror": false,
            "vflip": false,
        },
        "log": {
            "file": "",
            "level": "info",
        },
        "tools": {
            "tavilyApiKey": "",
            "serperApiKey": "",
            "braveApiKey": "",
            "amapApiKey": "",
            "amapServiceHost": "",
        },
        "asr": {
            "enabled": false,
            "endpoint": "wss://tts-center.ea.modelbest.co/api/v1/asr/realtime",
            "apiKey": "",
            "serviceId": 2,
            "maxReconnects": 3,
            "gateWhilePlaying": true,
        },
        "network": {
            "mode": "wifi",
            "usbRelay": {"host": "", "portMap": {}},
            "publicNetwork": false,
        },
    })
}

/// Python `_deep_merge`：override 的 dict 与 base 的 dict 递归合并，其余整体替换。
fn deep_merge(base: &mut Value, over: &Value) {
    if let (Some(b), Some(o)) = (base.as_object_mut(), over.as_object()) {
        for (k, v) in o {
            let recurse = matches!(b.get(k), Some(bv) if bv.is_object()) && v.is_object();
            if recurse {
                if let Some(bv) = b.get_mut(k) {
                    deep_merge(bv, v);
                }
            } else {
                b.insert(k.clone(), v.clone());
            }
        }
    }
}

static NULL: Value = Value::Null;

/// Map 取值（缺失返回 Null 静态值）。
fn g<'a>(m: &'a Map<String, Value>, k: &str) -> &'a Value {
    m.get(k).unwrap_or(&NULL)
}

/// 可选 Map 取值（节缺失返回 Null 静态值）。
fn gm<'a>(m: Option<&'a Map<String, Value>>, k: &str) -> &'a Value {
    m.and_then(|m| m.get(k)).unwrap_or(&NULL)
}

/// 必需节（DEFAULTS 保证存在；用户用非对象覆盖时报错）。
fn required_section<'a>(d: &'a Value, key: &str) -> Result<&'a Map<String, Value>, ConfigError> {
    match d.as_object().and_then(|o| o.get(key)) {
        Some(v) => v
            .as_object()
            .ok_or_else(|| ConfigError(format!("配置字段 {key} 必须是对象"))),
        None => Err(ConfigError(format!("缺少配置节 {key}"))),
    }
}

/// 可选节（Python `d.get(key) or {}`：null/空容器视同缺失，非对象报错）。
fn soft_section<'a>(d: &'a Value, key: &str) -> Result<Option<&'a Map<String, Value>>, ConfigError> {
    match d.as_object().and_then(|o| o.get(key)) {
        None => Ok(None),
        Some(v) if !py_truthy(v) => Ok(None),
        Some(v) => v
            .as_object()
            .map(Some)
            .ok_or_else(|| ConfigError(format!("配置字段 {key} 必须是对象"))),
    }
}

/// 设备音频标定条目（按 device.label 查；见 parse 处的优先级说明）。
struct DeviceAudioProfile {
    echo_max_delay_ms: f64,
    mic_duck_while_playing: f64,
}

/// 已标定机器的差异参数。新增机器实测出回声特征后往这里加一行即可，
/// 不用改设备上的 json。02 走全局默认（回声延迟 ~312ms，窗口 1000ms 富余）。
fn device_audio_profile(label: &str) -> Option<DeviceAudioProfile> {
    match label {
        // 03：回声延迟 ~975ms（贴默认窗口顶，AEC 锁不住）；近场耦合强，
        // duck 压到 0.2（02 用默认 0.4 一直稳定）。
        "03" => Some(DeviceAudioProfile {
            echo_max_delay_ms: 1600.0,
            mic_duck_while_playing: 0.2,
        }),
        _ => None,
    }
}

/// Python 真值语义（bool(x)：None/false/0/""/空容器为假）。
fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(x or default)`：假值回退 default；数字转十进制文本；字符串原样。
fn py_str(v: &Value, key: &str, default: &str) -> Result<String, ConfigError> {
    if !py_truthy(v) {
        return Ok(default.to_string());
    }
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(if *b { "true" } else { "false" }.to_string()),
        _ => Err(ConfigError(format!("配置字段 {key} 必须是字符串"))),
    }
}

/// `py_str` + `strip()`（Python 里字符串字段大多带 `.strip()`）。
fn s_trim(v: &Value, key: &str, default: &str) -> Result<String, ConfigError> {
    Ok(py_str(v, key, default)?.trim().to_string())
}

/// Python `int(x or default)`：假值回退；数字向零截断；字符串去空白解析。
fn py_int(v: &Value, key: &str, default: i64) -> Result<i64, ConfigError> {
    if !py_truthy(v) {
        return Ok(default);
    }
    py_int_parse(v).ok_or_else(|| ConfigError(format!("配置字段 {key} 不是有效整数: {v}")))
}

/// Python `float(x or default)`：假值回退；字符串去空白解析。
fn py_float(v: &Value, key: &str, default: f64) -> Result<f64, ConfigError> {
    if !py_truthy(v) {
        return Ok(default);
    }
    py_float_parse(v).ok_or_else(|| ConfigError(format!("配置字段 {key} 不是有效数字: {v}")))
}

/// Python `int(x)`（无回退；失败返回 None 供 try/except 语义使用）。
fn py_int_parse(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

/// Python `float(x)`（无回退；失败返回 None 供 try/except 语义使用）。
fn py_float_parse(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// 整数入目标类型（Python 不限范围；超界在这里报配置错误）。
fn fit<T>(key: &str, v: i64) -> Result<T, ConfigError>
where
    T: TryFrom<i64>,
{
    T::try_from(v).map_err(|_| ConfigError(format!("配置字段 {key} 的值 {v} 超出可表示范围")))
}

/// Python `fg.get("initPayload") or {}`：假值 / 非对象 → `{}`，其余原样保留。
fn payload_or_empty(v: &Value) -> Value {
    if py_truthy(v) {
        v.clone()
    } else {
        json!({})
    }
}

/// Python `rp if isinstance(rp, dict) else {}`。
fn object_or_empty(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(o.clone()),
        _ => json!({}),
    }
}

/// Python `str(fg.get("protocolVersion") or "4").strip()` → (u8, auto)。
/// "auto" → (4, true)（4 是协商失败时的兜底）；"3"/"4"/"5"（字符串或数字）
/// → (n, false)。
fn parse_protocol_version(v: &Value) -> Result<(u8, bool), ConfigError> {
    let s = py_str(v, "foreground.protocolVersion", "4")?;
    let s = s.trim();
    if s.eq_ignore_ascii_case("auto") {
        return Ok((4, true));
    }
    let n = s
        .parse::<u8>()
        .map_err(|_| ConfigError(format!("foreground.protocolVersion 不是有效版本号: {s}")))?;
    Ok((n, false))
}

fn resolve_feeder_path(configured: &str, default_name: &str) -> PathBuf {
    if configured.is_empty() {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|d| d.join(default_name)))
            .unwrap_or_else(|| PathBuf::from(default_name))
    } else {
        PathBuf::from(configured)
    }
}

/// 浮点 JSON 数值：整数值写成整数（对齐 DEFAULTS 的数字形态）。
fn num(x: f64) -> Value {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

/// 小写 hex。
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Python 项目同位置的 config.example.json（已拷贝到本仓库根）。
    const EXAMPLE_JSON: &str = include_str!("../../../config.example.json");
    /// 16k mono PCM16 直通 fixture 的 sha256（Python 实算）。
    const PASSTHROUGH_SHA256: &str =
        "9eb648761aa8586852cc9ea75680dbfc96fa634443fd51dec810f92cc81d82be";

    /// 触及进程环境变量的测试串行化（cargo test 多线程并行）。
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_clean_env(f: impl FnOnce()) {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("O5_HARNESS_CONFIG");
        f();
        std::env::remove_var("O5_HARNESS_CONFIG");
    }

    fn tmp_path(name: &str) -> String {
        let p = std::env::temp_dir().join(format!("o5cfg-{}-{name}", std::process::id()));
        p.to_string_lossy().into_owned()
    }

    fn fixture_bytes(name: &str) -> Vec<u8> {
        let p = format!("{}/tests/fixtures/voice/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&p).unwrap_or_else(|e| panic!("读取 fixture 失败 {p}: {e}"))
    }

    fn write_bytes(name: &str, bytes: &[u8]) -> String {
        let p = tmp_path(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn write_json(name: &str, text: &str) -> String {
        write_bytes(name, text.as_bytes())
    }

    /// 构造最小 RIFF/WAVE 字节流（fmt 头手写，可指定格式 tag）。
    fn build_wav(format_tag: u16, channels: u16, bits: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let block_align = channels * bits / 8;
        let byte_rate = rate * u32::from(block_align);
        let mut w = Vec::with_capacity(44 + data.len());
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&format_tag.to_le_bytes());
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&byte_rate.to_le_bytes());
        w.extend_from_slice(&block_align.to_le_bytes());
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data.len() as u32).to_le_bytes());
        w.extend_from_slice(data);
        w
    }

    fn i16_bytes(vals: &[i16]) -> Vec<u8> {
        let mut v = Vec::with_capacity(vals.len() * 2);
        for s in vals {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    fn i32_bytes(vals: &[i32]) -> Vec<u8> {
        let mut v = Vec::with_capacity(vals.len() * 4);
        for s in vals {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    /// 规范化输出的 data 块 PCM（44 字节定长头）。
    fn output_pcm(wav: &[u8]) -> Vec<i16> {
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(wav[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1); // PCM
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1); // mono
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16000);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 32000);
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(&wav[36..40], b"data");
        let n = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(wav.len(), 44 + n);
        wav[44..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    fn b64_decode(s: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .expect("base64 解码失败")
    }

    // ---------------- 默认值 ----------------

    #[test]
    fn defaults_match_python_defaults() {
        let c = Config::defaults();
        assert_eq!(c.fg_endpoint, "wss://47.95.219.248:7029/e/merge1006-128k-300step/v1/realtime");
        assert_eq!(c.fg_api_key, "");
        assert_eq!(c.fg_protocol_version, 4);
        assert_eq!(c.fg_mode, "audio");
        assert_eq!(c.fg_omni_mode, "omni");
        assert!(!c.fg_auto_start_session);
        assert!(c.fg_queue_gate);
        assert_eq!(c.fg_progress_timeout_s, 60.0);
        assert_eq!(c.fg_ack_timeout_s, 30.0);
        assert_eq!(c.fg_connect_timeout_s, 15.0);
        assert!(c.fg_tls_insecure_skip_verify);
        assert_eq!(c.fg_shell_prompt, "");
        assert!(c.fg_ref_audio_segment);
        assert_eq!(c.fg_task_prompt, DEFAULT_TASK_PROMPT);
        assert_eq!(c.fg_init_payload, json!({}));
        assert_eq!(c.fg_voice, "builtin:ref_minicpm_signature_fix.wav");

        assert!(!c.bg_enabled);
        assert_eq!(c.bg_endpoint, "");
        assert_eq!(c.bg_timeout_s, 120.0);
        assert_eq!(c.bg_max_concurrent, 2);
        assert_eq!(c.bg_system_prompt, DEFAULT_BACKGROUND_SYSTEM_PROMPT);
        assert_eq!(c.bg_max_result_chars, 1200);
        assert_eq!(c.bg_request_parameters, json!({}));

        assert_eq!(c.audio_input_volume, 100);
        assert_eq!(c.audio_output_volume, 80);
        assert_eq!(c.audio_output_sample_rate, 24000);
        assert!(!c.audio_mic_gate_while_playing);
        assert_eq!(c.audio_gate_tail_ms, 200);
        assert!(c.audio_echo_cancel);
        assert_eq!(c.audio_echo_mode, "");
        assert_eq!(c.audio_echo_max_delay_ms, 1000.0);
        assert!(!c.audio_echo_debug_dump);
        assert_eq!(c.audio_mic_duck_while_playing, 0.4);
        assert!(!c.audio_mic_debug_dump);
        assert_eq!(c.audio_mic_dump_path, "/root/o5_harness/o5_mic_debug.pcm");
        assert_eq!(c.audio_max_session_minutes, 0.0);

        assert!(c.video_enabled);
        assert_eq!(c.video_jpeg_quality, 60);
        assert_eq!(c.video_width, 512);
        assert_eq!(c.video_height, 384);
        assert_eq!(c.video_fps, 15);
        assert!(!c.video_hmirror);
        assert!(!c.video_vflip);

        assert_eq!(c.log_file, "");
        assert_eq!(c.log_level, "info");

        assert_eq!(c.tools_tavily_api_key, "");
        assert_eq!(c.tools_amap_service_host, "");

        assert!(!c.asr_enabled);
        assert_eq!(c.asr_endpoint, "wss://tts-center.ea.modelbest.co/api/v1/asr/realtime");
        assert_eq!(c.asr_service_id, 2);
        assert_eq!(c.asr_max_reconnects, 3);
        assert!(c.asr_gate_while_playing);

        assert_eq!(c.net_mode, "wifi");
        assert_eq!(c.net_usb_relay_host, "");
        assert!(c.net_usb_relay_port_map.is_empty());

        assert_eq!(c.device_label, "");
        assert_eq!(c.device_touch_variant, 2);
        assert_eq!(c.device_display_feeder_path, "");
        assert_eq!(c.device_camera_feeder_path, "");

        assert!(c.validate().is_ok());
        assert!(c.voice().is_none());
        assert_eq!(c.source_path(), None);
    }

    #[test]
    fn default_trait_matches_defaults() {
        assert_eq!(Config::default(), Config::defaults());
    }

    // ---------------- JSON 解析（fixture：Python 项目 config.example.json） --------

    #[test]
    fn protocol_auto_and_public_network_parse_roundtrip() {
        // "auto"（大小写不敏感）→ auto=true + 兜底 4
        let cfg = Config::from_json_str(
            r#"{"foreground":{"endpoint":"ws://x","protocolVersion":"auto"},"network":{"publicNetwork":true}}"#,
        )
        .unwrap();
        assert!(cfg.fg_protocol_auto);
        assert_eq!(cfg.fg_protocol_version, 4); // 协商失败兜底
        assert!(cfg.net_public);
        assert!(cfg.validate().is_ok(), "auto 不走数字范围校验");
        // 序列化往返：auto 原样写回
        let v = cfg.to_json_value();
        assert_eq!(v["foreground"]["protocolVersion"], "auto");
        assert_eq!(v["network"]["publicNetwork"], true);
        let cfg2 = Config::from_json_str(&v.to_string()).unwrap();
        assert!(cfg2.fg_protocol_auto && cfg2.net_public);
        // AUTO 大写同样接受；显式 5 不置 auto
        let cfg3 = Config::from_json_str(
            r#"{"foreground":{"endpoint":"ws://x","protocolVersion":"AUTO"}}"#,
        )
        .unwrap();
        assert!(cfg3.fg_protocol_auto);
        let cfg4 = Config::from_json_str(
            r#"{"foreground":{"endpoint":"ws://x","protocolVersion":"5"}}"#,
        )
        .unwrap();
        assert!(!cfg4.fg_protocol_auto);
        assert_eq!(cfg4.fg_protocol_version, 5);
    }

    #[test]
    fn parse_python_example_json() {
        let cfg = Config::from_json_str(EXAMPLE_JSON).unwrap();
        assert_eq!(cfg.fg_endpoint, "wss://api.ea.modelbest.co/v1/realtime");
        // 仓库里的示例配置不得带凭证
        assert!(cfg.fg_api_key.is_empty());
        assert!(cfg.vacuum_url.is_empty());
        assert_eq!(cfg.vacuum_entity_id, "vacuum.zhui_mi_w10s_pro");
        assert!(cfg.vacuum_token.is_empty());
        // 扫地机工具默认关闭：url+token 齐备但没设 enabled=true 也不注入
        assert!(!cfg.vacuum_enabled);
        let mut onv: serde_json::Value =
            serde_json::from_str(EXAMPLE_JSON).unwrap();
        onv["vacuum"]["enabled"] = serde_json::Value::Bool(true);
        let on = Config::from_json_str(&onv.to_string()).unwrap();
        assert!(on.vacuum_enabled);
        assert_eq!(cfg.fg_protocol_version, 4);
        assert_eq!(cfg.fg_mode, "audio");
        assert_eq!(cfg.fg_omni_mode, "omni");
        assert_eq!(cfg.fg_voice, "builtin:ref_minicpm_signature_fix.wav");
        assert!(cfg.fg_tls_insecure_skip_verify);
        assert!(cfg.fg_queue_gate);
        assert!(cfg.fg_ref_audio_segment);
        assert_eq!(cfg.fg_progress_timeout_s, 60.0);
        // taskPrompt 为空 → 默认（Python `or DEFAULT_TASK_PROMPT`）
        assert_eq!(cfg.fg_task_prompt, DEFAULT_TASK_PROMPT);
        // background.systemPrompt 为空 → 默认
        assert_eq!(cfg.bg_system_prompt, DEFAULT_BACKGROUND_SYSTEM_PROMPT);
        assert_eq!(cfg.bg_request_parameters, json!({}));
        // 显式覆盖：video 640x480（代码默认 512x384）
        assert_eq!(cfg.video_width, 640);
        assert_eq!(cfg.video_height, 480);
        assert_eq!(cfg.video_fps, 15); // 文件未给 → 默认
        assert_eq!(cfg.video_jpeg_quality, 60);
        assert!(!cfg.bg_enabled);
        assert!(!cfg.asr_enabled);
        assert_eq!(cfg.asr_service_id, 2);
        assert!(cfg.asr_gate_while_playing);
        assert_eq!(cfg.audio_output_volume, 80);
        assert_eq!(cfg.audio_gate_tail_ms, 200);
        assert_eq!(cfg.audio_max_session_minutes, 0.0);
        assert_eq!(cfg.net_mode, "wifi");
        assert_eq!(cfg.device_touch_variant, 2); // 文件无 device 节 → 默认
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn partial_json_merges_defaults() {
        let cfg = Config::from_json_str(r#"{"foreground": {"endpoint": "ws://x"}}"#).unwrap();
        assert_eq!(cfg.fg_endpoint, "ws://x");
        assert_eq!(cfg.fg_api_key, "");
        assert_eq!(cfg.fg_protocol_version, 4);
        assert!(!cfg.fg_protocol_auto);
        assert!(!cfg.net_public);
        assert_eq!(cfg.audio_mic_duck_while_playing, 0.4);
        assert_eq!(cfg.video_width, 512);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn python_or_fallback_semantics() {
        // Python `x or default`：0 / null / 空串视同未配置
        let cfg = Config::from_json_str(
            r#"{"foreground":{"progressTimeoutSeconds":0},
                "audio":{"inputVolume":0,"outputVolume":0,"gateTailMs":0},
                "video":{"width":0,"fps":0}}"#,
        )
        .unwrap();
        assert_eq!(cfg.fg_progress_timeout_s, 60.0);
        assert_eq!(cfg.audio_input_volume, 100);
        assert_eq!(cfg.audio_output_volume, 80);
        assert_eq!(cfg.audio_gate_tail_ms, 200);
        assert_eq!(cfg.video_width, 512);
        assert_eq!(cfg.video_fps, 15);

        // micDuck 无 or 语义：0 合法；越界被夹到 [0,1]
        let cfg = Config::from_json_str(r#"{"audio":{"micDuckWhilePlaying":0}}"#).unwrap();
        assert_eq!(cfg.audio_mic_duck_while_playing, 0.0);
        let cfg = Config::from_json_str(r#"{"audio":{"micDuckWhilePlaying":2}}"#).unwrap();
        assert_eq!(cfg.audio_mic_duck_while_playing, 1.0);
        // nearAgc（Rust 版新增）：缺省 true，显式 false 可关
        assert!(cfg.audio_near_agc);
        let cfg = Config::from_json_str(r#"{"audio":{"nearAgc":false}}"#).unwrap();
        assert!(!cfg.audio_near_agc);
        // echoMaxDelayMs：取 max(100, x)
        let cfg = Config::from_json_str(r#"{"audio":{"echoMaxDelayMs":50}}"#).unwrap();
        assert_eq!(cfg.audio_echo_max_delay_ms, 100.0);

        // 字符串形式的数字（Python int()/str() 兼容）
        let cfg = Config::from_json_str(
            r#"{"foreground":{"protocolVersion":"5"},"audio":{"inputVolume":"55"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.fg_protocol_version, 5);
        assert_eq!(cfg.audio_input_volume, 55);
        let cfg = Config::from_json_str(r#"{"foreground":{"protocolVersion":3}}"#).unwrap();
        assert_eq!(cfg.fg_protocol_version, 3);
    }

    #[test]
    fn derived_flags_require_endpoints() {
        // bg_enabled 需要 endpoint
        let cfg =
            Config::from_json_str(r#"{"background":{"enabled":true,"endpoint":""}}"#).unwrap();
        assert!(!cfg.bg_enabled);
        let cfg =
            Config::from_json_str(r#"{"background":{"enabled":true,"endpoint":"https://x"}}"#)
                .unwrap();
        assert!(cfg.bg_enabled);
        // asr_enabled 需要 endpoint + apiKey
        let cfg = Config::from_json_str(
            r#"{"asr":{"enabled":true,"endpoint":"wss://a","apiKey":""}}"#,
        )
        .unwrap();
        assert!(!cfg.asr_enabled);
        let cfg = Config::from_json_str(
            r#"{"asr":{"enabled":true,"endpoint":"wss://a","apiKey":"k"}}"#,
        )
        .unwrap();
        assert!(cfg.asr_enabled);
        // asr_serviceId 解析失败静默回 0（Python try/except）
        let cfg = Config::from_json_str(r#"{"asr":{"serviceId":"abc"}}"#).unwrap();
        assert_eq!(cfg.asr_service_id, 0);
        let cfg = Config::from_json_str(r#"{"asr":{"serviceId":"7"}}"#).unwrap();
        assert_eq!(cfg.asr_service_id, 7);
    }

    #[test]
    fn echo_mode_and_net_mode_validation() {
        let cfg = Config::from_json_str(r#"{"audio":{"echoMode":"bad"}}"#).unwrap();
        assert_eq!(cfg.audio_echo_mode, "");
        let cfg = Config::from_json_str(r#"{"audio":{"echoMode":"NLMS"}}"#).unwrap();
        assert_eq!(cfg.audio_echo_mode, "nlms");
        let cfg = Config::from_json_str(r#"{"audio":{"echoMode":"subband"}}"#).unwrap();
        assert_eq!(cfg.audio_echo_mode, "subband");

        let cfg = Config::from_json_str(r#"{"network":{"mode":"cell"}}"#).unwrap();
        assert_eq!(cfg.net_mode, "wifi");
        let cfg = Config::from_json_str(r#"{"network":{"mode":" USB "}}"#).unwrap();
        assert_eq!(cfg.net_mode, "usb");

        // portMap：键小写、值宽松解析、坏值跳过
        let cfg = Config::from_json_str(
            r#"{"network":{"mode":"usb","usbRelay":{
                "host":"10.239.93.100",
                "portMap":{"API.EA.ModelBest.CO":"17443","bad":"x","Host.Example.COM":20000}}}}"#,
        )
        .unwrap();
        assert_eq!(cfg.net_usb_relay_host, "10.239.93.100");
        assert_eq!(cfg.net_usb_relay_port_map.len(), 2);
        assert_eq!(cfg.net_usb_relay_port_map.get("api.ea.modelbest.co"), Some(&17443));
        assert_eq!(cfg.net_usb_relay_port_map.get("host.example.com"), Some(&20000));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn device_audio_profile_overrides() {
        // 03 无显式 audio 键 → 标定表生效（回声延迟 ~975ms 的窗口 + 深 duck）
        let cfg = Config::from_json_str(r#"{"device":{"label":"03"}}"#).unwrap();
        assert_eq!(cfg.audio_echo_max_delay_ms, 1600.0);
        assert!((cfg.audio_mic_duck_while_playing - 0.2).abs() < 1e-9);
        // 设备 json 显式值 > 标定表
        let cfg = Config::from_json_str(
            r#"{"device":{"label":"03"},"audio":{"echoMaxDelayMs":1200,"micDuckWhilePlaying":0.5}}"#,
        )
        .unwrap();
        assert_eq!(cfg.audio_echo_max_delay_ms, 1200.0);
        assert!((cfg.audio_mic_duck_while_playing - 0.5).abs() < 1e-9);
        // 未标定机器（如 02）→ 全局默认
        let cfg = Config::from_json_str(r#"{"device":{"label":"02"}}"#).unwrap();
        assert_eq!(cfg.audio_echo_max_delay_ms, 1000.0);
        assert!((cfg.audio_mic_duck_while_playing - 0.4).abs() < 1e-9);
    }

    #[test]
    fn device_section_parsed() {
        let cfg = Config::from_json_str(
            r#"{"device":{"label":" 03 ","touchVariant":5,
                "displayFeederPath":"/root/feeder_display.py"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.device_label, "03");
        assert_eq!(cfg.device_touch_variant, 5);
        assert_eq!(cfg.device_display_feeder_path, "/root/feeder_display.py");
        assert_eq!(cfg.device_camera_feeder_path, "");
        // 空配置回默认 2
        let cfg = Config::from_json_str(r#"{"device":{"touchVariant":"bad"}}"#).unwrap();
        assert_eq!(cfg.device_touch_variant, 2);
        // feeder 路径未配置时指向 exe 目录
        let p = cfg.display_feeder_path();
        assert!(p.ends_with("feeder_display.py"));
        let p = cfg.camera_feeder_path();
        assert!(p.ends_with("feeder_camera.py"));
        let cfg = Config::from_json_str(r#"{"device":{"cameraFeederPath":"/x/f.py"}}"#).unwrap();
        assert_eq!(cfg.camera_feeder_path(), PathBuf::from("/x/f.py"));
    }

    // ---------------- legacy taskPrompt 升级 ----------------

    #[test]
    fn legacy_task_prompt_upgrades() {
        for legacy in LEGACY_TASK_PROMPTS {
            let payload = json!({"foreground": {"taskPrompt": legacy}}).to_string();
            let cfg = Config::from_json_str(&payload).unwrap();
            assert_eq!(cfg.fg_task_prompt, DEFAULT_TASK_PROMPT);
        }
        // 自定义提示词保留
        let cfg = Config::from_json_str(r#"{"foreground":{"taskPrompt":"自定义提示词"}}"#).unwrap();
        assert_eq!(cfg.fg_task_prompt, "自定义提示词");
        // 空串回默认（且默认值不在 legacy 集合里，不重复升级）
        let cfg = Config::from_json_str(r#"{"foreground":{"taskPrompt":""}}"#).unwrap();
        assert_eq!(cfg.fg_task_prompt, DEFAULT_TASK_PROMPT);
    }

    // ---------------- validate 错误文案（与 Python 逐字一致） ----------------

    #[test]
    fn validate_error_messages_match_python() {
        let cases: &[(&str, &str)] = &[
            (r#"{"foreground":{"endpoint":""}}"#, "foreground.endpoint 未配置（WebSocket 地址）"),
            (
                r#"{"foreground":{"endpoint":"http://x"}}"#,
                "foreground.endpoint 必须以 ws:// 或 wss:// 开头",
            ),
            (
                r#"{"foreground":{"endpoint":"ws://x","protocolVersion":"6"}}"#,
                "foreground.protocolVersion 只支持 3/4/5/auto，当前 6",
            ),
            (
                r#"{"foreground":{"endpoint":"ws://x","mode":"foo"}}"#,
                "foreground.mode 只支持 audio/omni/video",
            ),
            (
                r#"{"background":{"enabled":true,"endpoint":"ftp://x"}}"#,
                "background.endpoint 必须是 http(s) 地址",
            ),
            (
                r#"{"network":{"mode":"usb"}}"#,
                "network.mode=usb 需要 network.usbRelay.host 和 portMap（电脑中继配置）",
            ),
        ];
        for (payload, want) in cases {
            let cfg = Config::from_json_str(payload).unwrap();
            let err = cfg.validate().unwrap_err();
            assert_eq!(&err.0, want, "payload: {payload}");
        }
    }

    #[test]
    fn invalid_protocol_version_rejected_at_parse() {
        // Python 在 validate 期报「只支持 3/4/5，当前 abc」；
        // Rust 契约要求 u8，非数字文本在解析期报错（见移植说明）。
        let err = Config::from_json_str(r#"{"foreground":{"protocolVersion":"abc"}}"#).unwrap_err();
        assert!(err.0.contains("foreground.protocolVersion"), "got: {}", err.0);
    }

    // ---------------- media mode ----------------

    #[test]
    fn media_mode_param_and_with_video() {
        let cfg = Config::defaults();
        assert_eq!(cfg.media_mode_param(), "audio");
        assert!(!cfg.with_video()); // audio 档即使 video.enabled 也不用相机
        assert_eq!(cfg.effective_media_mode(), "audio");

        let cfg = Config::from_json_str(r#"{"foreground":{"mode":"omni"},"video":{"enabled":false}}"#)
            .unwrap();
        assert_eq!(cfg.media_mode_param(), "omni"); // 默认 omniMode=omni
        assert!(!cfg.with_video());

        let cfg = Config::from_json_str(r#"{"foreground":{"mode":"omni","omniMode":"video"}}"#)
            .unwrap();
        assert_eq!(cfg.media_mode_param(), "video");
        assert!(cfg.with_video());

        let cfg = Config::from_json_str(r#"{"foreground":{"mode":"video"}}"#).unwrap();
        assert_eq!(cfg.media_mode_param(), "video");
        assert!(cfg.with_video());
    }

    // ---------------- 原子保存 ----------------

    #[test]
    fn save_atomic_and_roundtrip() {
        let path = tmp_path("save_roundtrip.json");
        let cfg = Config {
            audio_output_volume: 42,
            device_label: "03".into(),
            net_usb_relay_port_map: BTreeMap::from([("api.ea.modelbest.co".to_string(), 17443)]),
            fg_init_payload: json!({"k": "v"}),
            ..Config::defaults()
        };
        cfg.save(&path).unwrap();
        // 无残留临时文件
        assert!(!Path::new(&format!("{path}.o5tmp")).exists());
        // 读回逐字段一致
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded = Config::from_json_str(&text).unwrap();
        assert_eq!(reloaded, cfg);
        assert_eq!(reloaded.audio_output_volume, 42);
        assert_eq!(reloaded.device_label, "03");
        assert_eq!(
            reloaded.net_usb_relay_port_map.get("api.ea.modelbest.co"),
            Some(&17443)
        );
        // 覆盖已存在文件（rename 替换）
        let cfg2 = Config {
            audio_output_volume: 7,
            ..Config::defaults()
        };
        cfg2.save(&path).unwrap();
        let reloaded2 = Config::from_json_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reloaded2.audio_output_volume, 7);
        assert!(!Path::new(&format!("{path}.o5tmp")).exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_layout_matches_config_file_structure() {
        let path = tmp_path("save_layout.json");
        Config::defaults().save(&path).unwrap();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for key in [
            "version",
            "foreground",
            "background",
            "audio",
            "video",
            "log",
            "tools",
            "asr",
            "network",
            "device",
        ] {
            assert!(v.get(key).is_some(), "缺少节 {key}");
        }
        assert_eq!(v["version"], 1);
        assert_eq!(v["foreground"]["protocolVersion"], "4");
        assert_eq!(v["foreground"]["taskPrompt"], DEFAULT_TASK_PROMPT);
        assert_eq!(v["audio"]["micDuckWhilePlaying"], 0.4);
        assert_eq!(v["audio"]["maxSessionMinutes"], 0);
        assert_eq!(v["video"]["width"], 512);
        assert_eq!(v["network"]["mode"], "wifi");
        let _ = std::fs::remove_file(&path);
    }

    // ---------------- load 搜索顺序 ----------------

    #[test]
    fn load_search_order_cli_env_then_defaults() {
        with_clean_env(|| {
            let a = write_json(
                "load_a.json",
                r#"{"foreground":{"endpoint":"ws://a"}}"#,
            );
            let b = write_json(
                "load_b.json",
                r#"{"foreground":{"endpoint":"ws://b"}}"#,
            );
            // cli 优先于 env
            std::env::set_var("O5_HARNESS_CONFIG", &b);
            let cfg = Config::load(Some(&a)).unwrap();
            assert_eq!(cfg.fg_endpoint, "ws://a");
            assert_eq!(cfg.source_path(), Some(a.as_str()));
            // 无 cli 时用 env
            let cfg = Config::load(None).unwrap();
            assert_eq!(cfg.fg_endpoint, "ws://b");
            assert_eq!(cfg.source_path(), Some(b.as_str()));
            // cli 指向不存在的文件：静默落到 env
            let cfg = Config::load(Some("Z:/no/such/file.json")).unwrap();
            assert_eq!(cfg.fg_endpoint, "ws://b");
            // 全部缺失：内置默认（不报错）
            std::env::remove_var("O5_HARNESS_CONFIG");
            let cfg = Config::load(None).unwrap();
            assert_eq!(cfg, Config::defaults());
            assert_eq!(cfg.source_path(), None);
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
        });
    }

    #[test]
    fn load_validates_file_config() {
        with_clean_env(|| {
            let p = write_json(
                "load_bad.json",
                r#"{"foreground":{"endpoint":"http://bad"}}"#,
            );
            let err = Config::load(Some(&p)).unwrap_err();
            assert_eq!(err.0, "foreground.endpoint 必须以 ws:// 或 wss:// 开头");
            // 坏 JSON 报错
            let p2 = write_json("load_not_json.json", "not json at all");
            let err = Config::load(Some(&p2)).unwrap_err();
            assert!(err.0.contains("不是有效的 JSON"), "got: {}", err.0);
            let _ = std::fs::remove_file(&p);
            let _ = std::fs::remove_file(&p2);
        });
    }

    // ---------------- Voice：Python 生成的 fixture（逐字节一致） ----------------

    fn check_voice_fixture(name: &str) {
        let input = fixture_bytes(&format!("{name}.wav"));
        let expected = fixture_bytes(&format!("{name}.out.wav"));
        let path = write_bytes(&format!("fx_{name}.wav"), &input);
        let v = Voice::load(&path, Path::new("Z:/no-such-assets")).unwrap();
        assert_eq!(v.data, expected, "fixture {name} 与 Python 输出不一致");
        assert_eq!(v.spec, path);
        // sha256 / base64 自洽
        let mut h = Sha256::new();
        h.update(&v.data);
        assert_eq!(hex_lower(&h.finalize()), v.sha256);
        assert_eq!(b64_decode(&v.data_b64), v.data);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn voice_fixture_s16_16k_mono_passthrough() {
        // 合规文件（含 LIST 块）原字节直通
        let input = fixture_bytes("s16_16k_mono.wav");
        let path = write_bytes("fx_pt.wav", &input);
        let v = Voice::load(&path, Path::new("Z:/x")).unwrap();
        assert_eq!(v.data, input);
        assert_eq!(v.sha256, PASSTHROUGH_SHA256); // 与 Python 实算一致
        assert!(!v.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn voice_fixture_s16_16k_stereo() {
        check_voice_fixture("s16_16k_stereo");
    }

    #[test]
    fn voice_fixture_s16_44k1_mono() {
        check_voice_fixture("s16_44k1_mono");
    }

    #[test]
    fn voice_fixture_s16_32k_stereo() {
        check_voice_fixture("s16_32k_stereo");
    }

    #[test]
    fn voice_fixture_u8_16k_mono() {
        check_voice_fixture("u8_16k_mono");
    }

    #[test]
    fn voice_fixture_u8_12k_stereo() {
        check_voice_fixture("u8_12k_stereo");
    }

    #[test]
    fn voice_fixture_s32_16k_mono() {
        check_voice_fixture("s32_16k_mono");
    }

    #[test]
    fn voice_fixture_s32_24k_stereo() {
        check_voice_fixture("s32_24k_stereo");
    }

    // ---------------- Voice：手造小 WAV 的分支行为 ----------------

    #[test]
    fn voice_u8_mono_16k_hand_computed() {
        // (u-128)/128：-1.0→-32767；0→0；(127/128)*32767→32511；-0.5→-16383.5→rint 偶→-16384
        let wav = build_wav(1, 1, 8, 16000, &[0, 128, 255, 64]);
        let path = write_bytes("hand_u8.wav", &wav);
        let v = Voice::load(&path, Path::new("Z:/x")).unwrap();
        assert_eq!(output_pcm(&v.data), vec![-32767, 0, 32511, -16384]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn voice_s16_stereo_16k_hand_computed() {
        // 均值：(16384+16384)/2/32768=0.5 → 16383.5 → ties-even → 16384；
        // (-32768+32767)/2 → -1/65536 → -0.49998…→0；(0+1)/2 → 0.49998…→0
        let data = i16_bytes(&[16384, 16384, -32768, 32767, 0, 1]);
        let wav = build_wav(1, 2, 16, 16000, &data);
        let path = write_bytes("hand_st.wav", &wav);
        let v = Voice::load(&path, Path::new("Z:/x")).unwrap();
        assert_eq!(output_pcm(&v.data), vec![16384, 0, 0]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn voice_s16_resample_32k_to_16k() {
        // n_out = int(4*16000/32000) = 2；x = [0, 3] → [0, -1000]
        let data = i16_bytes(&[0, 1000, 0, -1000]);
        let wav = build_wav(1, 1, 16, 32000, &data);
        let path = write_bytes("hand_rs32.wav", &wav);
        let v = Voice::load(&path, Path::new("Z:/x")).unwrap();
        assert_eq!(output_pcm(&v.data), vec![0, -1000]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn voice_resample_n_out_truncates() {
        // 3 样本 @44100 → n_out = int(1.0884) = 1 → linspace 单点 = [0] → fp[0]
        let data = i16_bytes(&[5000, -6000, 7000]);
        let wav = build_wav(1, 1, 16, 44100, &data);
        let path = write_bytes("hand_rs44.wav", &wav);
        let v = Voice::load(&path, Path::new("Z:/x")).unwrap();
        assert_eq!(output_pcm(&v.data), vec![5000]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn voice_s32_quirk_no_full_scale() {
        // 照抄 Python：S32 不除以满量程 → 非零样本全部 clip 到 ±1
        let data = i32_bytes(&[0, 1, -1, 2000000000]);
        let wav = build_wav(1, 1, 32, 16000, &data);
        let path = write_bytes("hand_s32.wav", &wav);
        let v = Voice::load(&path, Path::new("Z:/x")).unwrap();
        assert_eq!(output_pcm(&v.data), vec![0, 32767, -32767, 32767]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn voice_non_pcm_wav_rejected() {
        // IEEE float（Python wave 报 unknown format: 3）
        let wav = build_wav(3, 1, 32, 16000, &[0, 0, 0, 0]);
        let path = write_bytes("hand_float.wav", &wav);
        let err = Voice::load(&path, Path::new("Z:/x")).unwrap_err();
        assert!(
            err.0.contains("不是可解析的 PCM WAV"),
            "got: {}",
            err.0
        );
        // 非 RIFF 内容
        let path2 = write_bytes("hand_garbage.wav", b"this is not a wav at all");
        let err = Voice::load(&path2, Path::new("Z:/x")).unwrap_err();
        assert!(err.0.contains("不是可解析的 PCM WAV"), "got: {}", err.0);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn voice_24bit_rejected() {
        let wav = build_wav(1, 1, 24, 16000, &[0; 6]);
        let path = write_bytes("hand_24.wav", &wav);
        let err = Voice::load(&path, Path::new("Z:/x")).unwrap_err();
        assert_eq!(err.0, format!("音色 {path} 采样宽度 24bit 不支持"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn voice_missing_file_error() {
        let err = Voice::load("Z:/no/such/file.wav", Path::new("Z:/x")).unwrap_err();
        assert_eq!(err.0, "音色文件不存在: Z:/no/such/file.wav");
    }

    #[test]
    fn voice_builtin_resolution() {
        let dir = std::env::temp_dir().join(format!("o5cfg-assets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = build_wav(1, 1, 16, 16000, &i16_bytes(&[100, -100]));

        // 无后缀自动补 .wav
        std::fs::write(dir.join("myvoice.wav"), &wav).unwrap();
        let v = Voice::load("builtin:myvoice", &dir).unwrap();
        assert_eq!(v.spec, "builtin:myvoice");
        assert_eq!(v.data, wav);

        // 已带后缀
        let v = Voice::load("builtin:myvoice.wav", &dir).unwrap();
        assert_eq!(v.data, wav);

        // 空名 builtin: → 不加载（Python 返回 None）
        let v = Voice::load("builtin:", &dir).unwrap();
        assert!(v.is_empty());
        assert_eq!(v.spec, "builtin:");
        let v = Voice::load("", &dir).unwrap();
        assert!(v.is_empty());
        assert_eq!(v.spec, "");

        // 资产目录没有的音色 → 报不存在
        let err = Voice::load("builtin:nope", &dir).unwrap_err();
        assert!(err.0.starts_with("音色文件不存在: "), "got: {}", err.0);
        assert!(err.0.ends_with("nope.wav"));

        let _ = std::fs::remove_file(dir.join("myvoice.wav"));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn config_load_voice_cache() {
        let dir = std::env::temp_dir().join(format!("o5cfg-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = build_wav(1, 1, 16, 16000, &i16_bytes(&[1, 2, 3]));
        std::fs::write(dir.join("v.wav"), &wav).unwrap();

        let mut cfg = Config {
            fg_voice: "builtin:v".into(),
            ..Config::defaults()
        };
        assert!(cfg.voice().is_none());
        cfg.load_voice(&dir).unwrap();
        let v = cfg.voice().expect("音色应已缓存");
        assert_eq!(v.spec, "builtin:v");
        assert_eq!(v.data, wav);
        assert!(!v.sha256.is_empty());

        // spec 为空：不加载，不报错
        let mut cfg = Config { fg_voice: String::new(), ..Config::defaults() };
        cfg.load_voice(&dir).unwrap();
        assert!(cfg.voice().is_none());

        // 文件缺失：报错
        let mut cfg = Config { fg_voice: "builtin:gone".into(), ..Config::defaults() };
        assert!(cfg.load_voice(&dir).is_err());

        let _ = std::fs::remove_file(dir.join("v.wav"));
        let _ = std::fs::remove_dir(&dir);
    }
}


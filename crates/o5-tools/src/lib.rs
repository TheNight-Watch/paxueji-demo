//! 前台/后台共用本地工具：web_search（多服务商降级链）+ map_search（高德）。
//!
//! 对齐 cpm-harness 的实现口径（packages/core/src/plugins/brain/web-search.ts +
//! services/amap-client.ts），由 Python 版（o5/tools.py，requests）忠实移植为 ureq：
//!
//! - 工具按训练顺序声明（[`TOOL_TRAINING_RANK`]，ui_card 训练数据里的顺序），
//!   delegate_task 等未知工具保持相对顺序排最后——顺序错会让 checkpoint 的
//!   function calling 掉分布（真机实测口径，见 cpm-harness o5-adapter）。
//! - web_search 降级链固定 Tavily -> Serper -> Brave ->（免 key）AnySearch Free：
//!   auto 只把已配 key 的服务商放进链，空结果/报错/限流都继续下一家，第一家非空即停；
//!   免 key 兜底在，工具恒可用。
//! - map_search 走高德 Web 服务：Key 以 sk- 开头走 EA 网关（Bearer），其余直连
//!   restapi.amap.com（?key=）。能力：地点搜索（v3/place/text）+ 驾车/步行路线
//!   （v3/direction/driving|walking）；起终点是地名时先取首个 POI 的坐标。
//! - 结果文案必须"可口播"：短、分条、带关键数字；错误信息里绝不回显 Key。
//! - 全部阻塞 HTTP——前台调用必须放工作线程（main 分流），后台调用在 delegation
//!   的线程池里，都不许碰 session 的 IO 线程。

mod amap;
mod http;
mod pyliteral;
mod schema;
mod search;
mod textutil;
mod vacuum;

#[cfg(test)]
mod tests;

use serde_json::{json, Value};
use std::collections::HashSet;

// 搜索服务商端点（cpm-harness constants.ts 同源）
const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";
const SERPER_ENDPOINT: &str = "https://google.serper.dev/search";
const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const ANYSEARCH_ENDPOINT: &str = "https://api.anysearch.com/v1/search";

// 高德 Web 服务
const AMAP_REST_BASE: &str = "https://restapi.amap.com";
const EA_AMAP_GATEWAY_BASE: &str = "https://llm.ea.modelbest.co/v1/services/amap";
const AMAP_GATEWAY_KEY_PREFIX: &str = "sk-";
const AMAP_PLACE_MAX: i64 = 20;
const AMAP_PLACE_DEFAULT: i64 = 10;
const AMAP_REQUEST_TIMEOUT_S: f64 = 10.0;

// 搜索链预算：单家超时 / 全链总时限（超预算不再发起下一家；第一家不受限）
const PROVIDER_TIMEOUT_S: f64 = 15.0;
const CHAIN_BUDGET_S: f64 = 30.0;
const WEB_SEARCH_MAX_RESULTS: i64 = 10;

/// ui_card 训练数据的工具顺序（cpm-harness o5-adapter TOOL_TRAINING_RANK）：
/// 工具按训练顺序声明；未知工具保持原相对顺序排最后。
pub const TOOL_TRAINING_RANK: &[(&str, u8)] = &[
    ("show_card", 0),
    ("get_client_location", 1),
    ("get_current_time", 2),
    ("web_search", 3),
    ("calculate", 4),
    ("map_search", 5),
    ("amap_weather", 6),
];
const UNKNOWN_TOOL_RANK: usize = TOOL_TRAINING_RANK.len();

/// 工具名 -> 训练顺序序号（未知工具返回表长，即"排最后"）。
pub fn order_for_training(name: &str) -> usize {
    TOOL_TRAINING_RANK
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, rank)| usize::from(*rank))
        .unwrap_or(UNKNOWN_TOOL_RANK)
}

// ---------------------------------------------------------------------------
// 参数解析
// ---------------------------------------------------------------------------

/// 工具参数解析：服务端会下发单引号 Python dict 字面量或 JSON，
/// 先 json 再 Python 字面量（只解字面量不执行代码），都不行返回 `{}`。
pub fn parse_arguments(raw: &str) -> Value {
    let s = raw.trim();
    if s.is_empty() {
        return json!({});
    }
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        if v.is_object() {
            return v;
        }
    }
    if let Ok(v) = pyliteral::parse_literal(s) {
        if v.is_object() {
            return v;
        }
    }
    log::warn!(
        "工具参数无法解析，按空参数处理: {}",
        textutil::truncate_chars(s, 80)
    );
    json!({})
}

// ---------------------------------------------------------------------------
// 工具执行器
// ---------------------------------------------------------------------------

/// 工具执行异常（对齐 Python 的异常类型名，用于 "[工具失败] {name}: {类型}: {e}" 文案）。
#[derive(Debug)]
pub(crate) enum ToolError {
    /// RuntimeError：429/402 限流欠费、AnySearch 业务错误等。
    Runtime(String),
    /// AmapError：高德调用失败，message 是可直接回填的中文说明，不含 Key。
    Amap(String),
    /// requests.exceptions.Timeout。
    Timeout(String),
    /// requests.exceptions.ConnectionError。
    Connection(String),
    /// json.JSONDecodeError。
    JsonDecode(String),
    /// requests.exceptions.HTTPError（raise_for_status）。
    Http(String),
}

impl ToolError {
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            ToolError::Runtime(_) => "RuntimeError",
            ToolError::Amap(_) => "AmapError",
            ToolError::Timeout(_) => "Timeout",
            ToolError::Connection(_) => "ConnectionError",
            ToolError::JsonDecode(_) => "JSONDecodeError",
            ToolError::Http(_) => "HTTPError",
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            ToolError::Runtime(m)
            | ToolError::Amap(m)
            | ToolError::Timeout(m)
            | ToolError::Connection(m)
            | ToolError::JsonDecode(m)
            | ToolError::Http(m) => m,
        };
        f.write_str(msg)
    }
}

/// 请求端点（默认取常量；单测注入本地 stub）。
struct Endpoints {
    tavily: String,
    serper: String,
    brave: String,
    anysearch: String,
    amap_rest: String,
    ea_gateway: String,
}

/// 本地工具执行器（前台工作线程 / 后台委派循环共用一份实现）。
///
/// keys 全部可空：搜索没 key 时走 AnySearch 免 key 兜底（恒可用）；
/// 高德没 key 时 map_search 返回"未配置"的如实说明，不编坐标。
pub struct ToolExecutor {
    tavily_key: String,
    serper_key: String,
    brave_key: String,
    amap_key: String,
    amap_host: String,
    endpoints: Endpoints,
    provider_timeout_s: f64,
    chain_budget_s: f64,
    amap_timeout_s: f64,
    /// Home Assistant 扫地机目标（url+token 都配了才注册 vacuum 工具）。
    vacuum: Option<vacuum::VacuumTarget>,
}

impl ToolExecutor {
    pub fn new(
        tavily_key: &str,
        serper_key: &str,
        brave_key: &str,
        amap_key: &str,
        amap_host: &str,
    ) -> Self {
        ToolExecutor {
            tavily_key: tavily_key.trim().to_string(),
            serper_key: serper_key.trim().to_string(),
            brave_key: brave_key.trim().to_string(),
            amap_key: amap_key.trim().to_string(),
            amap_host: amap_host.trim().to_string(),
            endpoints: Endpoints {
                tavily: TAVILY_ENDPOINT.to_string(),
                serper: SERPER_ENDPOINT.to_string(),
                brave: BRAVE_ENDPOINT.to_string(),
                anysearch: ANYSEARCH_ENDPOINT.to_string(),
                amap_rest: AMAP_REST_BASE.to_string(),
                ea_gateway: EA_AMAP_GATEWAY_BASE.to_string(),
            },
            provider_timeout_s: PROVIDER_TIMEOUT_S,
            chain_budget_s: CHAIN_BUDGET_S,
            amap_timeout_s: AMAP_REQUEST_TIMEOUT_S,
            vacuum: None,
        }
    }

    /// 启用扫地机工具（Home Assistant 直连）。三条件齐一不注入：
    /// enabled 开关（默认关）+ url/token 齐备——普通用户可能用不上。
    pub fn with_vacuum(
        mut self,
        enabled: bool,
        url: &str,
        token: &str,
        entity_id: &str,
    ) -> Self {
        if !enabled {
            return self;
        }
        let target = vacuum::VacuumTarget {
            url: url.trim().to_string(),
            token: token.trim().to_string(),
            entity_id: entity_id.trim().to_string(),
        };
        if target.configured() {
            self.vacuum = Some(target);
        }
        self
    }

    /// 扫地机工具是否已启用（供提示词按需注入）。
    pub fn vacuum_enabled(&self) -> bool {
        self.vacuum.is_some()
    }

    /// 前台工具名集合。
    pub fn foreground_names(&self) -> HashSet<&'static str> {
        let mut names: HashSet<&'static str> = HashSet::from(["web_search", "map_search"]);
        if self.vacuum.is_some() {
            names.extend(["vacuum_status", "vacuum_command", "vacuum_set_fan_speed"]);
        }
        names
    }

    /// 前台工具面（已按训练顺序；delegate_task 由 main 追加在最后）。
    pub fn foreground_definitions(&self) -> Value {
        let mut tools = vec![schema::web_search_tool(), schema::map_search_tool()];
        if self.vacuum.is_some() {
            tools.push(schema::vacuum_status_tool());
            tools.push(schema::vacuum_command_tool());
            tools.push(schema::vacuum_set_fan_speed_tool());
        }
        Value::Array(tools)
    }

    /// 后台委派模型的工具面（OpenAI function calling）。
    pub fn background_definitions(&self) -> Value {
        Value::Array(vec![
            schema::background_web_search_tool(),
            schema::background_map_search_tool(),
        ])
    }

    /// 同步执行一个工具（阻塞 HTTP，最长 ~链预算），返回可口播的文本；永不 panic。
    pub fn execute(&self, name: &str, arguments: &str) -> String {
        let args = parse_arguments(arguments);
        let result: Result<String, ToolError> = match name {
            "web_search" => Ok(search::web_search(self, &args)),
            "map_search" => amap::map_search(self, &args),
            "vacuum_status" => self.vacuum.as_ref().map_or(
                Err(ToolError::Runtime("扫地机未配置（vacuum.url / vacuum.token）".into())),
                |t| vacuum::status(t),
            ),
            "vacuum_command" => self.vacuum.as_ref().map_or(
                Err(ToolError::Runtime("扫地机未配置（vacuum.url / vacuum.token）".into())),
                |t| {
                    vacuum::command(
                        t,
                        args.get("action").and_then(Value::as_str).unwrap_or_default(),
                    )
                },
            ),
            "vacuum_set_fan_speed" => self.vacuum.as_ref().map_or(
                Err(ToolError::Runtime("扫地机未配置（vacuum.url / vacuum.token）".into())),
                |t| {
                    vacuum::set_fan_speed(
                        t,
                        args.get("fan_speed").and_then(Value::as_str).unwrap_or_default(),
                    )
                },
            ),
            _ => return format!("[参数错误] 未知工具: {name}"),
        };
        match result {
            Ok(text) => text,
            Err(e) => {
                log::error!("工具 {name} 执行异常: {}: {}", e.type_name(), e);
                format!("[工具失败] {name}: {}: {}", e.type_name(), e)
            }
        }
    }
}

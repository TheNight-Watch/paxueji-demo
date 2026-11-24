//! Home Assistant 扫地机工具（前台直连）：通过 HA REST API 控制 W10s Pro，
//! 不依赖 cpm-harness / Node MCP——MaixCAM2 上没有 Node 运行时，直连 HA 最短路径。
//!
//! - 工具面对齐 dreame-mcp（cpm-harness 侧的 mcp_dreame_vacuum_*）：status /
//!   command（start|pause|stop|return_home|locate）/ set_fan_speed；
//! - **fan 档位枚举必须与 HA 实体 fan_speed_list 严格一致**
//!   （W10s Pro 实测 silent/standard/strong/turbo；schema 与 HA 两边校验各认一套
//!   会让语音模型被夹在中间反复碰壁——2026-11-19 cpm-harness 侧踩过的坑）；
//! - 结果必须"可口播"：短、中文、带关键数字；
//! - 全部阻塞 HTTP，由 ToolExecutor 的调用方保证在工作线程里跑。
//! - 实体状态异常 / 请求失败时如实返回错误说明，不编造状态。

use crate::{http, ToolError};
use serde_json::{json, Value};

const VACUUM_TIMEOUT_S: f64 = 8.0;

/// HA 目标（vacuum 配置节：url + token + entityId）。
#[derive(Clone)]
pub(crate) struct VacuumTarget {
    /// HA 基地址，如 `http://192.168.55.34:8123`
    pub url: String,
    /// HA 长期访问令牌（Bearer）
    pub token: String,
    /// 实体 id，如 `vacuum.zhui_mi_w10s_pro`（空 = 用默认值）
    pub entity_id: String,
}

impl VacuumTarget {
    pub(crate) fn configured(&self) -> bool {
        !self.url.trim().is_empty() && !self.token.trim().is_empty()
    }

    fn base(&self) -> &str {
        self.url.trim_end_matches('/')
    }

    fn entity(&self) -> &str {
        if self.entity_id.trim().is_empty() {
            "vacuum.zhui_mi_w10s_pro"
        } else {
            self.entity_id.trim()
        }
    }

    fn auth(&self) -> Vec<(&'static str, String)> {
        vec![("Authorization", format!("Bearer {}", self.token.trim()))]
    }

    /// GET /api/states/<entity>：200 返回实体 JSON；非 200 映射成可读错误。
    fn state(&self) -> Result<Value, ToolError> {
        let url = format!("{}/api/states/{}", self.base(), self.entity());
        let resp = http::get(&url, &self.auth(), VACUUM_TIMEOUT_S)?;
        if resp.status != 200 {
            return Err(ToolError::Runtime(format!(
                "Home Assistant {}（{}）",
                resp.status,
                truncate(&resp.body, 120)
            )));
        }
        serde_json::from_str(&resp.body).map_err(|e| ToolError::JsonDecode(format!("{e}")))
    }

    /// POST /api/services/vacuum/<service>：200 即成功（HA 返回执行后的实体列表）。
    fn call(&self, service: &str, body: Value) -> Result<(), ToolError> {
        let url = format!("{}/api/services/vacuum/{}", self.base(), service);
        let resp = http::post_json(&url, &self.auth(), &body.to_string(), VACUUM_TIMEOUT_S)?;
        if resp.status != 200 {
            return Err(ToolError::Runtime(format!(
                "Home Assistant {}（{}）",
                resp.status,
                truncate(&resp.body, 120)
            )));
        }
        Ok(())
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.trim().to_string()
    } else {
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].trim().to_string()
    }
}

/// HA vacuum state → 中文口播。
fn state_cn(state: &str) -> &'static str {
    match state {
        "docked" => "在基站充电",
        "cleaning" => "清扫中",
        "returning" => "正在返回基站",
        "paused" => "已暂停",
        "idle" => "待机",
        "error" => "故障",
        _ => "未知状态",
    }
}

/// fan_speed 档位名 → 中文（枚举与 HA fan_speed_list 一致，见模块注释）。
pub(crate) fn fan_speed_cn(fan: &str) -> &'static str {
    match fan {
        "silent" => "安静",
        "standard" => "标准",
        "strong" => "强力",
        "turbo" => "超强",
        _ => "未知档位",
    }
}

/// 从实体 JSON 提取口播摘要。
fn summary(entity: &Value) -> String {
    let a = &entity["attributes"];
    let state = entity["state"].as_str().unwrap_or("unknown");
    let battery = a["battery_level"].as_i64();
    let fan = a["fan_speed"].as_str().unwrap_or("");
    let error = a["error"].as_str().unwrap_or("");
    let mut parts = vec![format!("状态：{}", state_cn(state))];
    if let Some(b) = battery {
        parts.push(format!("电量 {}%", b));
    }
    if !fan.is_empty() {
        parts.push(format!("吸力 {}", fan_speed_cn(fan)));
    }
    if !error.is_empty() && error != "No error" {
        parts.push(format!("故障：{error}"));
    }
    parts.join("，")
}

/// vacuum_status：读实体状态并口播。
pub(crate) fn status(t: &VacuumTarget) -> Result<String, ToolError> {
    let entity = t.state()?;
    Ok(summary(&entity))
}

/// vacuum_command：start|pause|stop|return_home|locate。
pub(crate) fn command(t: &VacuumTarget, action: &str) -> Result<String, ToolError> {
    let service = match action {
        "start" => "start",
        "pause" => "pause",
        "stop" => "stop",
        "return_home" => "return_to_base",
        "locate" => "locate",
        other => {
            return Err(ToolError::Runtime(format!(
                "未知 action：{other}（有效：start/pause/stop/return_home/locate）"
            )))
        }
    };
    t.call(service, json!({ "entity_id": t.entity() }))?;
    // locate 只是响一声，读回状态即可；其余动作读回确认执行后的状态
    let entity = t.state()?;
    if action == "locate" {
        Ok(format!("已让扫地机应答；当前{}", summary(&entity)))
    } else {
        Ok(summary(&entity))
    }
}

/// vacuum_set_fan_speed：先本地校验枚举（与 HA fan_speed_list 一致），再下发并读回确认。
pub(crate) fn set_fan_speed(t: &VacuumTarget, fan: &str) -> Result<String, ToolError> {
    if !["silent", "standard", "strong", "turbo"].contains(&fan) {
        return Err(ToolError::Runtime(format!(
            "吸力档位无效：{fan}（有效：silent/standard/strong/turbo）"
        )));
    }
    t.call(
        "set_fan_speed",
        json!({ "entity_id": t.entity(), "fan_speed": fan }),
    )?;
    let entity = t.state()?;
    let actual = entity["attributes"]["fan_speed"].as_str().unwrap_or("");
    if actual == fan {
        Ok(format!("吸力已设置为{}", fan_speed_cn(fan)))
    } else {
        Ok(format!(
            "已下发{}档，当前读回：{}（{}）",
            fan_speed_cn(fan),
            fan_speed_cn(actual),
            summary(&entity)
        ))
    }
}

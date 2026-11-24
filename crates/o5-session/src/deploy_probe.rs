//! 前台端点部署自报：GET `<端点>/v1/info`，协商前台协议版本（v3/v4/v5）。
//! （`o5/deploy_probe.py` 的移植）
//!
//! 判断口径对齐 cpm-harness parseV4Info（deployment-v4.ts）：
//! - 顶层 protocol_version 是 EA 池的标签；透明池可以保留旧标签（如 "4"）而把
//!   后端原样转发——模型级 backend_protocol_version="5" 且 init_passthrough=true
//!   时，客户端要说 v5（init 带 protocol_version "5" + input_mode）。
//! - 其余情况按顶层 protocol_version（"3"/"4"/"5"）；读不出按 "4"（历史默认）。
//!
//! 探测失败（网络 / 超时 / 非 JSON）不报错：返回 None，调用方回退配置值。
//! 真实部署的 /v1/info 实测可匿名读（无需 Authorization），带 key 时也一并带上。

use std::time::Duration;

use serde_json::Value;
use ureq::AgentBuilder;

use crate::tls;

/// 探测超时（Python PROBE_TIMEOUT）。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// wss 端点 -> https 的 /v1/info 自报地址（不带 mode 查询串）。
pub fn info_url(endpoint: &str) -> String {
    let mut base = endpoint.trim().to_string();
    if let Some(rest) = base.strip_prefix("wss://") {
        base = format!("https://{rest}");
    } else if let Some(rest) = base.strip_prefix("ws://") {
        base = format!("http://{rest}");
    }
    // /v1/ 后缀截掉（rsplit 语义：取最后一段）
    if let Some(i) = base.rfind("/v1/") {
        base.truncate(i);
    }
    format!("{base}/v1/info")
}

/// 读部署自报面；任何失败记一条日志返回 None（不报错）。
/// TLS 按 `tls_insecure` 决定（设备自签端点要跳过校验）；读上限 1MB。
pub fn fetch_info(endpoint: &str, api_key: &str, tls_insecure: bool) -> Option<Value> {
    let url = info_url(endpoint);
    let agent = AgentBuilder::new()
        .timeout(PROBE_TIMEOUT)
        .tls_config(tls::client_config(tls_insecure))
        .build();
    let mut req = agent.get(&url).set("Accept", "application/json");
    if !api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {api_key}"));
    }
    match req.call() {
        Ok(resp) => match resp.into_string() {
            // /v1/info 实测 < 4KB；into_string 的上限 1MB 防失控
            Ok(body) => match serde_json::from_str::<Value>(&body) {
                Ok(v @ Value::Object(_)) => Some(v),
                _ => {
                    log::warn!("部署自报不是 JSON 对象: {url}");
                    None
                }
            },
            Err(e) => {
                log::warn!("读部署自报失败（{url}: {e}），按配置版本处理");
                None
            }
        },
        Err(e) => {
            log::warn!("读部署自报失败（{url}: {e}），按配置版本处理");
            None
        }
    }
}

/// (生效版本, 一句话说明)。info 为 None 时回退 "4"。
///
/// - 透明池：models[0].model.init_passthrough=true 且 backend_protocol_version="5"
///   -> "5"（顶层标签可能还是 "4"，仅作说明）
/// - 其余：顶层 protocol_version 报了 3/4/5 就用它；没报按 "4"
pub fn negotiate_protocol_version(info: Option<&Value>) -> (u8, String) {
    let Some(info) = info.and_then(Value::as_object) else {
        return (4, "自报不可用，按 v4".to_string());
    };
    let top = info
        .get("protocol_version")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    // models[0] 可能是 {model: {...}} 包装，也可能直接是模型对象（Python 口径）
    let model = info
        .get("models")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_object)
        .map(|m| {
            m.get("model")
                .and_then(Value::as_object)
                .unwrap_or(m)
                .clone()
        });
    if let Some(m) = model {
        let passthrough = m.get("init_passthrough") == Some(&Value::Bool(true));
        let backend = m
            .get("backend_protocol_version")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if passthrough && backend == "5" {
            let label = if !top.is_empty() && top != "5" {
                format!("（池标签 v{top}）")
            } else {
                String::new()
            };
            return (5, format!("backend v5 + init_passthrough{label}"));
        }
    }
    if matches!(top.as_str(), "3" | "4" | "5") {
        return (top.parse().unwrap_or(4), format!("部署自报 v{top}"));
    }
    (4, "部署没报协议版本，按 v4".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn info_url_strips_v1_and_switches_scheme() {
        assert_eq!(
            info_url("wss://api.ea.modelbest.co/e/o5-t25/v1/realtime"),
            "https://api.ea.modelbest.co/e/o5-t25/v1/info"
        );
        assert_eq!(
            info_url("ws://10.0.0.5:7029/v1/realtime"),
            "http://10.0.0.5:7029/v1/info"
        );
        // 无 /v1/ 后缀：直接拼
        assert_eq!(info_url("wss://x.cn/realtime"), "https://x.cn/realtime/v1/info");
        // 尾随空白
        assert_eq!(
            info_url(" wss://x.cn/v1/realtime "),
            "https://x.cn/v1/info"
        );
    }

    #[test]
    fn negotiate_transparent_pool_backend_v5() {
        // 透明池：顶层标签 "4"，但 backend v5 + init_passthrough -> v5
        let info = json!({
            "protocol_version": "4",
            "models": [{"model": {"backend_protocol_version": "5", "init_passthrough": true}}]
        });
        let (v, note) = negotiate_protocol_version(Some(&info));
        assert_eq!(v, 5);
        assert_eq!(note, "backend v5 + init_passthrough（池标签 v4）");

        // models[0] 直接是模型对象（无 model 包装）
        let info2 = json!({
            "protocol_version": "5",
            "models": [{"backend_protocol_version": "5", "init_passthrough": false}]
        });
        let (v2, _) = negotiate_protocol_version(Some(&info2));
        assert_eq!(v2, 5);
    }

    #[test]
    fn negotiate_top_level_versions() {
        for (top, want) in [("3", 3), ("4", 4), ("5", 5)] {
            let info = json!({"protocol_version": top});
            let (v, note) = negotiate_protocol_version(Some(&info));
            assert_eq!(v, want);
            assert_eq!(note, format!("部署自报 v{top}"));
        }
        // 未报 / 非 JSON / 数组
        assert_eq!(
            negotiate_protocol_version(Some(&json!({}))),
            (4, "部署没报协议版本，按 v4".to_string())
        );
        assert_eq!(
            negotiate_protocol_version(None),
            (4, "自报不可用，按 v4".to_string())
        );
        assert_eq!(negotiate_protocol_version(Some(&json!([]))).0, 4);
        // 未知值按 v4
        assert_eq!(negotiate_protocol_version(Some(&json!({"protocol_version": "6"}))).0, 4);
    }
}

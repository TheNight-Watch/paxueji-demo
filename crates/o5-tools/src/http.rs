//! 极简阻塞 HTTP（ureq），传输错误映射到 Python requests 的异常类型名。
//! 只负责发请求/读状态/读 body；429/402 与 raise_for_status 的语义由调用方实现。

use crate::ToolError;
use serde_json::Value;
use std::io;
use std::time::Duration;

pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) status_text: String,
    pub(crate) body: String,
}

pub(crate) fn get(url: &str, headers: &[(&str, String)], timeout_s: f64) -> Result<HttpResponse, ToolError> {
    request("GET", url, headers, None, timeout_s)
}

pub(crate) fn post_json(
    url: &str,
    headers: &[(&str, String)],
    body: &str,
    timeout_s: f64,
) -> Result<HttpResponse, ToolError> {
    request("POST", url, headers, Some(body), timeout_s)
}

fn request(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
    timeout_s: f64,
) -> Result<HttpResponse, ToolError> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs_f64(timeout_s.max(0.001)))
        .build();
    let mut req = if method.eq_ignore_ascii_case("GET") {
        agent.get(url)
    } else {
        agent.post(url)
    };
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let result = match body {
        Some(b) => req.send_string(b),
        None => req.call(),
    };
    match result {
        Ok(resp) => {
            let status = resp.status();
            let status_text = resp.status_text().to_string();
            let body = resp
                .into_string()
                .map_err(|e| ToolError::Connection(format!("{e}")))?;
            Ok(HttpResponse { status, status_text, body })
        }
        // 非 2xx：ureq 以 Err 返回，这里还原成"状态 + 空 body"交给调用方判断
        Err(ureq::Error::Status(code, resp)) => {
            let status_text = resp.status_text().to_string();
            Ok(HttpResponse { status: code, status_text, body: String::new() })
        }
        Err(ureq::Error::Transport(t)) => Err(map_transport(t)),
    }
}

/// DNS/连接/超时等传输层错误 -> Timeout / ConnectionError（类型名与 requests 对齐）。
fn map_transport(t: ureq::Transport) -> ToolError {
    let msg = t.to_string();
    if is_timeout(&t) {
        ToolError::Timeout(msg)
    } else {
        ToolError::Connection(msg)
    }
}

fn is_timeout(t: &ureq::Transport) -> bool {
    use std::error::Error as _;
    if let Some(src) = t.source() {
        if let Some(io_err) = src.downcast_ref::<io::Error>() {
            if matches!(io_err.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
                return true;
            }
        }
    }
    let m = t.to_string().to_lowercase();
    m.contains("timed out") || m.contains("timeout")
}

/// Python `r.json()`：body 解析失败 -> JSONDecodeError。
pub(crate) fn parse_json_body(body: &str) -> Result<Value, ToolError> {
    serde_json::from_str(body).map_err(|e| ToolError::JsonDecode(format!("{e}")))
}

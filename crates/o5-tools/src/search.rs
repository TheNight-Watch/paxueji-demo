//! web_search：多服务商降级链（Tavily -> Serper -> Brave -> 免 key AnySearch Free）。

use crate::http::{self, HttpResponse};
use crate::textutil::{
    json_quote, py_num, py_or_str, py_str, py_truthy, truncate_chars, urlencode,
};
use crate::{ToolError, ToolExecutor, WEB_SEARCH_MAX_RESULTS};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Instant;

#[derive(Debug)]
pub(crate) struct SearchItem {
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) content: String,
}

/// 一家服务商的尝试记录（error 文本只用于日志；最终文案只区分 无结果/出错）。
pub(crate) struct Attempt {
    pub(crate) provider: &'static str,
    pub(crate) status: AttemptStatus,
}

pub(crate) enum AttemptStatus {
    Ok,
    Empty,
    Error(String),
}

impl Attempt {
    fn is_ok(&self) -> bool {
        matches!(self.status, AttemptStatus::Ok)
    }

    fn is_empty(&self) -> bool {
        matches!(self.status, AttemptStatus::Empty)
    }
}

/// query 语种探测 -> AnySearch 的 language 参数（假名 ja、谚文 ko、汉字 zh-CN）。
pub(crate) fn detect_language(query: &str) -> &'static str {
    if query.chars().any(|c| ('\u{3040}'..='\u{30ff}').contains(&c)) {
        return "ja";
    }
    if query.chars().any(|c| ('\u{ac00}'..='\u{d7a3}').contains(&c)) {
        return "ko";
    }
    if query.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) {
        return "zh-CN";
    }
    "en"
}

/// URL 规范化（去重键）：小写协议与主机、去端口/userinfo/query/hash、去尾斜杠。
/// 对齐 Python urllib.parse.urlsplit + hostname 的取值口径。
pub(crate) fn normalize_url(url: &str) -> String {
    let s = url.trim();
    // scheme://authority... ｜ //authority...（协议相对）｜ 其余（host 为空）
    let (scheme, rest): (String, &str) = if let Some(pos) = s.find("://") {
        if is_valid_scheme(&s[..pos]) {
            (s[..pos].to_lowercase(), &s[pos + 3..])
        } else {
            (String::new(), "")
        }
    } else if let Some(r) = s.strip_prefix("//") {
        (String::new(), r)
    } else {
        (String::new(), "")
    };
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let Some(host) = host_of(authority) else {
        return s.to_lowercase(); // urlsplit 抛 ValueError 时的 Python 兜底
    };
    let body = if rest.is_empty() {
        // 没有协议/网络位置：整串按 path 处理（截到 '?'/'#'）
        &s[..s.find(['?', '#']).unwrap_or(s.len())]
    } else {
        let tail = &rest[auth_end..];
        &tail[..tail.find(['?', '#']).unwrap_or(tail.len())]
    };
    let path = if body.is_empty() {
        "/"
    } else if let Some(p) = body.strip_suffix('/') {
        if p.is_empty() {
            "/"
        } else {
            p
        }
    } else {
        body
    };
    format!("{scheme}://{host}{path}")
}

fn is_valid_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
}

/// authority -> 小写主机（去 userinfo 与端口；坏 IPv6 返回 None）。
fn host_of(authority: &str) -> Option<String> {
    let hostport = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    let host = if let Some(after) = hostport.strip_prefix('[') {
        let end = after.find(']')?;
        &after[..end]
    } else if let Some(i) = hostport.rfind(':') {
        &hostport[..i]
    } else {
        hostport
    };
    Some(host.to_lowercase())
}

/// 结果规范化：trim、丢空 URL、按规范化 URL 去重、截到 max_results。
pub(crate) fn normalize_results(items: &[SearchItem], max_results: usize) -> Vec<SearchItem> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<SearchItem> = Vec::new();
    for item in items {
        let url = item.url.trim();
        if url.is_empty() {
            continue;
        }
        let key = normalize_url(url);
        if !seen.insert(key) {
            continue;
        }
        out.push(SearchItem {
            title: item.title.trim().to_string(),
            url: url.to_string(),
            content: item.content.trim().to_string(),
        });
        if out.len() >= max_results {
            break;
        }
    }
    out
}

fn to_items(array: &[Value], title_key: &str, url_key: &str, content_key: &str) -> Vec<SearchItem> {
    array
        .iter()
        .map(|it| SearchItem {
            title: py_or_str(it.get(title_key), ""),
            url: py_or_str(it.get(url_key), ""),
            content: py_or_str(it.get(content_key), ""),
        })
        .collect()
}

fn check_http(resp: &HttpResponse, provider: &str, url: &str) -> Result<(), ToolError> {
    if resp.status == 429 || resp.status == 402 {
        return Err(ToolError::Runtime(format!(
            "{provider} http {}（限流/欠费）",
            resp.status
        )));
    }
    if resp.status >= 400 {
        // requests 的 raise_for_status 文案
        let class = if resp.status < 500 { "Client Error" } else { "Server Error" };
        return Err(ToolError::Http(format!(
            "{} {class}: {} for url: {}",
            resp.status, resp.status_text, url
        )));
    }
    Ok(())
}

impl ToolExecutor {
    fn configured_providers(&self) -> Vec<&'static str> {
        let mut chain = Vec::new();
        if !self.tavily_key.is_empty() {
            chain.push("tavily");
        }
        if !self.serper_key.is_empty() {
            chain.push("serper");
        }
        if !self.brave_key.is_empty() {
            chain.push("brave");
        }
        chain
    }

    pub(crate) fn search_tavily(&self, query: &str, max_results: usize) -> Result<Vec<SearchItem>, ToolError> {
        let body = format!(
            "{{\"query\":{},\"max_results\":{},\"search_depth\":\"basic\"}}",
            json_quote(query),
            max_results.min(20)
        );
        let resp = http::post_json(
            &self.endpoints.tavily,
            &[
                ("Content-Type", "application/json".to_string()),
                ("Authorization", format!("Bearer {}", self.tavily_key)),
            ],
            &body,
            self.provider_timeout_s,
        )?;
        check_http(&resp, "tavily", &self.endpoints.tavily)?;
        let data = http::parse_json_body(&resp.body)?;
        let results = array_of(&data, "results");
        Ok(to_items(results, "title", "url", "content"))
    }

    pub(crate) fn search_serper(&self, query: &str, max_results: usize) -> Result<Vec<SearchItem>, ToolError> {
        let body = format!("{{\"q\":{},\"num\":{}}}", json_quote(query), max_results);
        let resp = http::post_json(
            &self.endpoints.serper,
            &[
                ("Content-Type", "application/json".to_string()),
                ("X-API-KEY", self.serper_key.clone()),
            ],
            &body,
            self.provider_timeout_s,
        )?;
        check_http(&resp, "serper", &self.endpoints.serper)?;
        let data = http::parse_json_body(&resp.body)?;
        let organic = array_of(&data, "organic");
        Ok(to_items(organic, "title", "link", "snippet")
            .into_iter()
            .take(max_results)
            .collect())
    }

    pub(crate) fn search_brave(&self, query: &str, max_results: usize) -> Result<Vec<SearchItem>, ToolError> {
        let url = format!(
            "{}?{}",
            self.endpoints.brave,
            urlencode(&[("q", query), ("count", &max_results.to_string())])
        );
        let resp = http::get(
            &url,
            &[
                ("Accept", "application/json".to_string()),
                ("X-Subscription-Token", self.brave_key.clone()),
            ],
            self.provider_timeout_s,
        )?;
        check_http(&resp, "brave", &url)?;
        let data = http::parse_json_body(&resp.body)?;
        let results = data
            .get("web")
            .and_then(|web| array_of_opt(web, "results"))
            .unwrap_or(&[]);
        Ok(to_items(results, "title", "url", "description")
            .into_iter()
            .take(max_results)
            .collect())
    }

    pub(crate) fn search_anysearch(&self, query: &str, max_results: usize) -> Result<Vec<SearchItem>, ToolError> {
        let body = format!(
            "{{\"query\":{},\"max_results\":{},\"language\":\"{}\"}}",
            json_quote(query),
            max_results.min(100),
            detect_language(query)
        );
        let resp = http::post_json(
            &self.endpoints.anysearch,
            &[("Content-Type", "application/json".to_string())],
            &body,
            self.provider_timeout_s,
        )?;
        check_http(&resp, "anysearch_free", &self.endpoints.anysearch)?;
        let body_val = http::parse_json_body(&resp.body)?;
        if let Some(code) = body_val.get("code") {
            // Python：code is not None and code != 0（False == 0、0.0 == 0 都算过）
            let is_zero = match code {
                Value::Bool(false) => true,
                Value::Number(n) => n.as_f64().map(|f| f == 0.0).unwrap_or(false),
                _ => false,
            };
            if !is_zero {
                let msg = match body_val.get("message") {
                    Some(m) if py_truthy(m) => py_str(m),
                    _ => py_str(code),
                };
                return Err(ToolError::Runtime(format!("anysearch_free: {msg}")));
            }
        }
        // data.results（data 是 dict 时）；data 缺失/非 dict 时 Python 取 {}，即无结果
        let raw = if let Some(d) = body_val.get("data") {
            if d.is_object() {
                d.get("results")
            } else {
                body_val.get("results")
            }
        } else {
            None
        };
        let entries = raw.and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[]);
        let mut out = Vec::new();
        for entry in entries.iter().take(max_results) {
            if !entry.is_object() {
                continue;
            }
            let content = entry
                .get("content")
                .filter(|v| py_truthy(v))
                .or_else(|| entry.get("description").filter(|v| py_truthy(v)))
                .or_else(|| entry.get("snippet").filter(|v| py_truthy(v)));
            out.push(SearchItem {
                title: py_or_str(entry.get("title"), ""),
                url: py_or_str(entry.get("url"), ""),
                content: match content {
                    Some(v) => py_str(v),
                    None => String::new(),
                },
            });
        }
        Ok(out)
    }

    /// 跑降级链，返回 (results, attempts)。各 provider 的请求函数按名取。
    pub(crate) fn run_search_chain(
        &self,
        query: &str,
        max_results: usize,
    ) -> (Vec<SearchItem>, Vec<Attempt>) {
        let mut chain = self.configured_providers();
        chain.push("anysearch_free");
        let started = Instant::now();
        let mut attempts: Vec<Attempt> = Vec::new();
        for provider in chain {
            // 全链总时限：超预算不再发起新请求（第一家不受限，预算只裁降级深度）
            if !attempts.is_empty() && started.elapsed().as_secs_f64() >= self.chain_budget_s {
                attempts.push(Attempt {
                    provider,
                    status: AttemptStatus::Error("链路总时限已用尽，跳过".to_string()),
                });
                continue;
            }
            match self.dispatch_search(provider, query, max_results) {
                Ok(items) => {
                    let results = normalize_results(&items, max_results);
                    if results.is_empty() {
                        attempts.push(Attempt { provider, status: AttemptStatus::Empty });
                        continue;
                    }
                    attempts.push(Attempt { provider, status: AttemptStatus::Ok });
                    return (results, attempts);
                }
                Err(e) => {
                    attempts.push(Attempt {
                        provider,
                        status: AttemptStatus::Error(truncate_chars(&e.to_string(), 80)),
                    });
                }
            }
        }
        (Vec::new(), attempts)
    }

    fn dispatch_search(
        &self,
        provider: &str,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<SearchItem>, ToolError> {
        match provider {
            "tavily" => self.search_tavily(query, max_results),
            "serper" => self.search_serper(query, max_results),
            "brave" => self.search_brave(query, max_results),
            "anysearch_free" => self.search_anysearch(query, max_results),
            other => Err(ToolError::Runtime(format!("未知搜索服务商: {other}"))),
        }
    }
}

fn array_of<'a>(data: &'a Value, key: &str) -> &'a [Value] {
    array_of_opt(data, key).unwrap_or(&[])
}

fn array_of_opt<'a>(data: &'a Value, key: &str) -> Option<&'a [Value]> {
    data.get(key).and_then(|v| v.as_array()).map(|a| a.as_slice())
}

/// web_search 入口：query 截 500 字、maxResults 兼容 snake_case、跑降级链、出口播文案。
pub(crate) fn web_search(ex: &ToolExecutor, args: &Value) -> String {
    let query: String = py_or_str(args.get("query"), "")
        .trim()
        .chars()
        .take(500)
        .collect();
    if query.is_empty() {
        return "[参数错误] web_search：query 不能为空".to_string();
    }
    // 兼容旧 snake_case（max_results）；Python 里 int(nan/inf) 会抛异常，这里按默认处理
    let raw_max = args.get("maxResults").or_else(|| args.get("max_results"));
    let mut v = py_num(raw_max, 5.0);
    if v == 0.0 || !v.is_finite() {
        v = 5.0;
    }
    let max_results = (v.trunc() as i64).clamp(1, WEB_SEARCH_MAX_RESULTS) as usize;

    let (results, attempts) = ex.run_search_chain(&query, max_results);
    if results.is_empty() {
        for a in &attempts {
            if let AttemptStatus::Error(e) = &a.status {
                log::debug!("搜索服务商 {} 失败: {}", a.provider, e);
            }
        }
        let detail = attempts
            .iter()
            .map(|a| {
                let word = if a.is_empty() { "无结果" } else { "出错" };
                format!("{} {word}", a.provider)
            })
            .collect::<Vec<_>>()
            .join("；");
        let detail = if detail.is_empty() { "无可用服务商".to_string() } else { detail };
        return format!("没有搜到\"{query}\"的相关结果（{detail}）。");
    }
    let lines: Vec<String> = results
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let content: String = it.content.chars().take(120).collect();
            format!("{}. {}（{}）：{}", i + 1, it.title, it.url, content)
        })
        .collect();
    let provider = attempts
        .iter()
        .rev()
        .find(|a| a.is_ok())
        .map(|a| a.provider)
        .unwrap_or("none");
    format!(
        "搜到 {} 条（来源 {provider}，外部网页内容未核实，引用附 URL）：\n{}",
        results.len(),
        lines.join("\n")
    )
}

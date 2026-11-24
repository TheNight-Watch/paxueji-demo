//! 网络模式：WiFi 直连（默认）/ USB 中继（经电脑转发）。（`o5/netmode.py` 的忠实移植）
//!
//! 配置里的端点永远写标准 443 地址；运行时按 `net_mode` 决定实际连接：
//! - wifi：端点原样使用，并清掉 /etc/hosts 里的中继映射块
//! - usb ：URL 域名命中中继 portMap 时改写为中继端口，同时在
//!   /etc/hosts 写入标记块（域名 -> 电脑 USB 网卡 IP）。TLS 的
//!   SNI / 证书校验仍按真实域名走（PC 中继只透传 TCP，见 tools/relay.py）。
//!
//! /etc/hosts 用 BEGIN/END 标记块管理，幂等，切回 wifi 自动移除；
//! 非 root / 文件不可写时降级为提示（不影响进程运行）。
//!
//! 本模块用显式参数（`mode` / `relay_host` / `port_map`）而非直接依赖
//! `o5-config::Config`，避免 crate 间耦合；`Config` -> [`NetConfig`] 的转换由
//! app 层完成（需要的字段：`net_mode` / `net_usb_relay_host` /
//! `net_usb_relay_port_map`，端点用 `fg_endpoint` 等原始 URL）。

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::timing::Timing;

pub const HOSTS_PATH: &str = "/etc/hosts";
pub const MARK_BEGIN: &str = "# BEGIN o5-usb-relay";
pub const MARK_END: &str = "# END o5-usb-relay";

/// 公网调用（更多菜单开关）：前台端点域名换成公网网关（Python `PUBLIC_GATEWAY_HOST`）。
pub const PUBLIC_GATEWAY_HOST: &str = "o5gw.modelbest.cn";

/// DNS 缓存：host -> (ip, 过期时刻)。
type DnsCache = HashMap<String, (String, Instant)>;

/// 网络模式相关配置（`o5-config::Config` 的 net_* 字段子集；app 层负责转换）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetConfig {
    /// "wifi"（默认）| "usb"
    pub net_mode: String,
    /// USB 中继：电脑 USB 网卡 IP（写入 /etc/hosts 的映射目标）
    pub net_usb_relay_host: String,
    /// USB 中继：真实域名 -> 中继端口
    pub net_usb_relay_port_map: BTreeMap<String, u16>,
    /// 公网调用：前台端点域名换公网网关 `o5gw.modelbest.cn`
    pub net_public: bool,
}

impl NetConfig {
    pub fn wifi() -> Self {
        Self::default()
    }

    pub fn usb(host: impl Into<String>, port_map: BTreeMap<String, u16>) -> Self {
        Self {
            net_mode: "usb".to_string(),
            net_usb_relay_host: host.into(),
            net_usb_relay_port_map: port_map,
            net_public: false,
        }
    }

    pub fn is_usb(&self) -> bool {
        self.net_mode == "usb"
    }
}

/// 最小 urlsplit（scheme://netloc/path?query#fragment），scheme 小写化。
/// 无 "://" 的输入返回 None（无 host 可匹配，调用方原样透传）。
#[derive(Debug, Clone, PartialEq)]
struct UrlSplit {
    scheme: String,
    netloc: String,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
}

impl UrlSplit {
    fn split(url: &str) -> Option<UrlSplit> {
        let i = url.find("://")?;
        let scheme = url[..i].to_ascii_lowercase();
        if scheme.is_empty() || !scheme.chars().next()?.is_ascii_alphabetic() {
            return None;
        }
        let rest = &url[i + 3..];
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let netloc = rest[..end].to_string();
        let after = &rest[end..];
        let (before_frag, fragment) = match after.find('#') {
            Some(f) => (&after[..f], Some(after[f + 1..].to_string())),
            None => (after, None),
        };
        let (path, query) = match before_frag.find('?') {
            Some(q) => (
                before_frag[..q].to_string(),
                Some(before_frag[q + 1..].to_string()),
            ),
            None => (before_frag.to_string(), None),
        };
        Some(UrlSplit {
            scheme,
            netloc,
            path,
            query,
            fragment,
        })
    }

    /// urlunsplit：换 netloc 重组（netloc 为空时也保持 scheme:// 前缀形态）。
    fn rebuild(&self, netloc: &str) -> String {
        let mut out = String::with_capacity(self.scheme.len() + netloc.len() + self.path.len() + 8);
        out.push_str(&self.scheme);
        out.push_str("://");
        out.push_str(netloc);
        out.push_str(&self.path);
        if let Some(q) = &self.query {
            out.push('?');
            out.push_str(q);
        }
        if let Some(f) = &self.fragment {
            out.push('#');
            out.push_str(f);
        }
        out
    }
}

/// netloc 解析结果（Python urlsplit 的 username/password/hostname/port 口径）。
#[derive(Debug, Clone, PartialEq)]
struct EndpointParts {
    username: Option<String>,
    password: Option<String>,
    /// 小写 host（IPv6 去方括号）
    host: String,
    /// URL 里的显式端口（0 或无法解析视为无）
    port: Option<u16>,
    /// IPv6 方括号形式（重写端口时保留方括号）
    bracketed: bool,
}

impl EndpointParts {
    fn parse(netloc: &str) -> EndpointParts {
        // userinfo 取最后一个 '@' 之前；username:password 在第一个 ':' 处分
        let (userinfo, hostinfo) = match netloc.rfind('@') {
            Some(i) => (Some(&netloc[..i]), &netloc[i + 1..]),
            None => (None, netloc),
        };
        let (username, password) = match userinfo {
            Some(ui) => match ui.find(':') {
                Some(c) => (Some(&ui[..c]), Some(&ui[c + 1..])),
                None => (Some(ui), None),
            },
            None => (None, None),
        };
        // host[:port]；IPv6 方括号形式单独处理
        let (host_raw, port_raw, bracketed) = if hostinfo.contains('[') {
            match (hostinfo.find('['), hostinfo.find(']')) {
                (Some(b), Some(e)) if e > b => {
                    let tail = &hostinfo[e + 1..];
                    (&hostinfo[b + 1..e], tail.strip_prefix(':'), true)
                }
                _ => (hostinfo, None, false),
            }
        } else {
            match hostinfo.find(':') {
                Some(c) => (&hostinfo[..c], Some(&hostinfo[c + 1..]), false),
                None => (hostinfo, None, false),
            }
        };
        EndpointParts {
            username: username.map(str::to_string),
            password: password.map(str::to_string),
            host: host_raw.to_ascii_lowercase(),
            port: port_raw
                .and_then(|p| p.parse::<u16>().ok())
                .filter(|p| *p != 0),
            bracketed,
        }
    }
}

/// usb 模式且 URL 域名命中中继 portMap 时替换端口；其余原样返回。
/// 显式参数版（对应 Python `effective_url(url, cfg)`；Config -> 参数转换由 app 层做）。
pub fn effective_url(url: &str, mode: &str, port_map: &BTreeMap<String, u16>) -> String {
    if url.is_empty() || mode != "usb" || port_map.is_empty() {
        return url.to_string();
    }
    let Some(parts) = UrlSplit::split(url) else {
        return url.to_string();
    };
    let ep = EndpointParts::parse(&parts.netloc);
    let Some(&port) = port_map.get(&ep.host).filter(|p| **p != 0) else {
        return url.to_string();
    };
    let mut netloc = String::new();
    // 端点一般不带认证信息，保守保留
    if let Some(user) = ep.username.as_deref().filter(|s| !s.is_empty()) {
        netloc.push_str(user);
        if let Some(pw) = ep.password.as_deref().filter(|s| !s.is_empty()) {
            netloc.push(':');
            netloc.push_str(pw);
        }
        netloc.push('@');
    }
    if ep.bracketed {
        netloc.push('[');
    }
    netloc.push_str(&ep.host);
    if ep.bracketed {
        netloc.push(']');
    }
    netloc.push(':');
    netloc.push_str(&port.to_string());
    parts.rebuild(&netloc)
}

/// [`effective_url`] 的 NetConfig 版。
pub fn effective_url_cfg(url: &str, cfg: &NetConfig) -> String {
    effective_url(url, &cfg.net_mode, &cfg.net_usb_relay_port_map)
}

/// 公网调用：URL 域名换成公网网关 `o5gw.modelbest.cn`（Python `public_gateway_url`）。
///
/// 协议 / 路径 / 查询串原样，端口清空（网关走标准 443）；
/// 空串 / 解析不出域名时原样返回；userinfo 保守保留。
pub fn public_gateway_url(url: &str) -> String {
    if url.is_empty() {
        return url.to_string();
    }
    let Some(parts) = UrlSplit::split(url) else {
        return url.to_string();
    };
    let ep = EndpointParts::parse(&parts.netloc);
    if ep.host.is_empty() {
        return url.to_string();
    }
    let mut netloc = String::new();
    if let Some(user) = ep.username.as_deref().filter(|s| !s.is_empty()) {
        netloc.push_str(user);
        if let Some(pw) = ep.password.as_deref().filter(|s| !s.is_empty()) {
            netloc.push(':');
            netloc.push_str(pw);
        }
        netloc.push('@');
    }
    netloc.push_str(PUBLIC_GATEWAY_HOST);
    parts.rebuild(&netloc)
}

/// 前台端点的完整解析（Python `effective_fg_url`）：公网调用开时域名先换
/// 公网网关，再按网络模式改写（usb 中继端口）。后台 / ASR 端点不走这里。
pub fn effective_fg_url(url: &str, public: bool, mode: &str, port_map: &BTreeMap<String, u16>) -> String {
    let url = if public { public_gateway_url(url) } else { url.to_string() };
    effective_url(&url, mode, port_map)
}

/// [`effective_fg_url`] 的 NetConfig 版。
pub fn effective_fg_url_cfg(url: &str, cfg: &NetConfig) -> String {
    effective_fg_url(url, cfg.net_public, &cfg.net_mode, &cfg.net_usb_relay_port_map)
}

/// usb 模式下应写入 /etc/hosts 的映射行（"中继IP 域名"，按域名排序）。
fn desired_lines(mode: &str, relay_host: &str, port_map: &BTreeMap<String, u16>) -> Vec<String> {
    if mode == "usb" && !relay_host.is_empty() && !port_map.is_empty() {
        port_map
            .keys()
            .map(|h| format!("{relay_host} {h}"))
            .collect()
    } else {
        Vec::new()
    }
}

/// 标记块合并（纯函数）：`Some(新内容)` = 需要写回，`None` = 块已一致，无事可做。
/// 残缺块（只有 BEGIN 或只有 END）按“无块”处理，残行随后统一清掉。
pub fn merge_hosts_content(content: &str, desired: &[String]) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let (existing, rest): (Option<Vec<String>>, Vec<&str>) =
        match lines.iter().position(|l| *l == MARK_BEGIN) {
            Some(b) => match lines[b + 1..]
                .iter()
                .position(|l| *l == MARK_END)
                .map(|i| i + b + 1)
            {
                Some(e) => (
                    Some(lines[b + 1..e].iter().map(|s| s.to_string()).collect()),
                    [lines[..b].to_vec(), lines[e + 1..].to_vec()].concat(),
                ),
                None => (None, lines.clone()),
            },
            None => (None, lines.clone()),
        };
    if existing.as_deref() == Some(desired) {
        return None;
    }
    let mut new_lines: Vec<&str> = rest
        .into_iter()
        .filter(|ln| {
            let t = ln.trim();
            !t.is_empty() && t != MARK_BEGIN && t != MARK_END
        })
        .collect();
    if !desired.is_empty() {
        new_lines.push("");
        new_lines.push(MARK_BEGIN);
        new_lines.extend(desired.iter().map(|s| s.as_str()));
        new_lines.push(MARK_END);
    }
    Some(new_lines.join("\n") + "\n")
}

/// 按网络模式同步指定 hosts 文件的中继标记块（幂等；写 `<path>.o5tmp` 后原子 rename）。
/// 读写失败时返回 Err（Python 降级为提示文案，这里由调用方记日志）。
pub fn sync_hosts_file(
    path: &str,
    mode: &str,
    relay_host: &str,
    port_map: &BTreeMap<String, u16>,
) -> io::Result<()> {
    let desired = desired_lines(mode, relay_host, port_map);
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            if desired.is_empty() {
                return Ok(()); // 无事可做（wifi 模式下 hosts 不可读也不影响运行）
            }
            return Err(io::Error::new(
                e.kind(),
                format!("无法读取 {path}，中继 hosts 未同步"),
            ));
        }
    };
    let Some(new_content) = merge_hosts_content(&content, &desired) else {
        return Ok(()); // 块已一致
    };
    let tmp = format!("{path}.o5tmp");
    if let Err(e) = std::fs::write(&tmp, new_content).and_then(|_| std::fs::rename(&tmp, path)) {
        return Err(io::Error::new(
            e.kind(),
            format!("hosts 写入失败（{e}）"),
        ));
    }
    let what = if desired.is_empty() {
        "(已清除)".to_string()
    } else {
        format!("{desired:?}")
    };
    log::info!("hosts 中继块已同步: {what}");
    Ok(())
}

/// 同步 `/etc/hosts`（linux）；其它平台为 no-op（Ok(())）。
pub fn sync_hosts(mode: &str, relay_host: &str, port_map: &BTreeMap<String, u16>) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        sync_hosts_file(HOSTS_PATH, mode, relay_host, port_map)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // PC 联测环境：/etc/hosts 不存在，无事可做
        let _ = (mode, relay_host, port_map);
        Ok(())
    }
}

/// [`sync_hosts`] 的 NetConfig 版。
pub fn sync_hosts_cfg(cfg: &NetConfig) -> io::Result<()> {
    sync_hosts(
        &cfg.net_mode,
        &cfg.net_usb_relay_host,
        &cfg.net_usb_relay_port_map,
    )
}

/// url -> (小写 host, 端口)。端口缺省：wss/https -> 443，其余 -> 80。
fn endpoint_of(url: &str) -> Option<(String, u16)> {
    let parts = UrlSplit::split(url)?;
    let ep = EndpointParts::parse(&parts.netloc);
    if ep.host.is_empty() {
        return None;
    }
    let port = ep
        .port
        .unwrap_or(match parts.scheme.as_str() {
            "wss" | "https" => 443,
            _ => 80,
        });
    Some((ep.host, port))
}

/// DNS 缓存（60s）：未过期直接用；过期重解析；解析失败沿用旧地址。
fn resolve_cached(
    dns: &mut DnsCache,
    host: &str,
    now: Instant,
    resolve: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    if let Some((ip, expiry)) = dns.get(host) {
        if *expiry > now {
            return Some(ip.clone());
        }
    }
    match resolve(host) {
        Some(ip) => {
            dns.insert(host.to_string(), (ip.clone(), now + Duration::from_secs(60)));
            Some(ip)
        }
        None => dns.get(host).map(|(ip, _)| ip.clone()),
    }
}

/// gethostbyname 等价：取第一个地址（IP 字面量不经 DNS）。
fn resolve_host(host: &str) -> Option<String> {
    (host, 0)
        .to_socket_addrs()
        .ok()?
        .next()
        .map(|addr| addr.ip().to_string())
}

/// 一轮探测：解析当前端点 -> 查 DNS 缓存 -> TCP connect-only 握手（2s 超时）
/// -> 喂 [`Timing::net_rtt`]。网络抖动/端点不可达：静默等下一轮（debug 日志）。
fn probe_round(url_fn: &dyn Fn() -> String, dns: &mut DnsCache, timing: &Timing) {
    let Some((host, port)) = endpoint_of(&url_fn()) else {
        return;
    };
    let Some(ip) = resolve_cached(dns, &host, Instant::now(), resolve_host) else {
        return;
    };
    let Ok(ip) = ip.parse::<IpAddr>() else {
        return;
    };
    let t0 = Instant::now();
    match TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs_f64(2.0)) {
        Ok(_sock) => {
            // 只握手不发数据：量的是网络往返 + 对端内核应答，drop 即关
            timing.net_rtt(t0.elapsed().as_secs_f64() * 1000.0);
        }
        Err(e) => log::debug!("RTT 探测失败: {e}"),
    }
}

/// 周期测纯网络 RTT：TCP 握手到前台端点（SYN -> SYN/ACK）。（Python `NetRttProbe`）
///
/// 只握手不发数据，量的是网络往返 + 对端内核应答，不含任何应用层
/// 处理——用来把 ACK（网络 + 服务端确认处理）拆成 网络 / 服务端 两块，
/// 判断“反应慢”该找网络还是找服务端。daemon 线程，随主进程退出。
pub struct NetRttProbe {
    stop: Arc<AtomicBool>,
}

impl Default for NetRttProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl NetRttProbe {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 请求退出（线程在一个 interval 内结束）。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// 启动探测线程（线程名 o5-net-rtt）。`url_fn` 每轮取当前生效端点
    /// （wifi/usb 切换跟随）；`interval_s` 默认口径 5s。
    pub fn spawn<F>(&self, url_fn: F, timing: Arc<Timing>, interval_s: f64) -> JoinHandle<()>
    where
        F: Fn() -> String + Send + Sync + 'static,
    {
        let stop = Arc::clone(&self.stop);
        thread::spawn(move || {
            crate::util::set_thread_name("o5-net-rtt");
            let interval = Duration::from_secs_f64(crate::util::sanitize_interval_s(interval_s));
            let mut dns: DnsCache = HashMap::new();
            loop {
                if wait_stop(&stop, interval) {
                    break;
                }
                probe_round(&url_fn, &mut dns, &timing);
            }
        })
    }
}

/// 等一个 interval（期间每 20ms 查一次 stop）；true = 已请求停止。
fn wait_stop(stop: &AtomicBool, interval: Duration) -> bool {
    let mut left = interval;
    while left > Duration::ZERO {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        let step = left.min(Duration::from_millis(20));
        thread::sleep(step);
        left -= step;
    }
    stop.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_url_rewrites_port() {
        let mut map = BTreeMap::new();
        map.insert("api.foreign.com".to_string(), 9443u16);
        // wifi：原样
        assert_eq!(
            effective_url("wss://api.foreign.com/v1/ws", "wifi", &map),
            "wss://api.foreign.com/v1/ws"
        );
        // usb 命中：host 小写 + 端口改写
        assert_eq!(
            effective_url("wss://API.Foreign.COM/v1/ws", "usb", &map),
            "wss://api.foreign.com:9443/v1/ws"
        );
        // usb 未命中
        assert_eq!(
            effective_url("wss://other.com/v1/ws", "usb", &map),
            "wss://other.com/v1/ws"
        );
        // 空串 / 空表
        assert_eq!(effective_url("", "usb", &map), "");
        assert_eq!(
            effective_url("wss://api.foreign.com/x", "usb", &BTreeMap::new()),
            "wss://api.foreign.com/x"
        );
        // 端口 0 视为未映射（Python `if not port` 口径）
        let mut m0 = BTreeMap::new();
        m0.insert("h.cn".to_string(), 0u16);
        assert_eq!(effective_url("wss://h.cn/x", "usb", &m0), "wss://h.cn/x");
        // 原端口被替换 + query/fragment 保留
        assert_eq!(
            effective_url("wss://api.foreign.com:443/p?k=v#f", "usb", &map),
            "wss://api.foreign.com:9443/p?k=v#f"
        );
        // userinfo 保守保留
        assert_eq!(
            effective_url("wss://u:p@api.foreign.com/x", "usb", &map),
            "wss://u:p@api.foreign.com:9443/x"
        );
        // 无 scheme：透传
        assert_eq!(
            effective_url("api.foreign.com:443/x", "usb", &map),
            "api.foreign.com:443/x"
        );
        // 无 path / 无 port
        assert_eq!(
            effective_url("wss://api.foreign.com", "usb", &map),
            "wss://api.foreign.com:9443"
        );
        // IPv6 方括号保留（Python 会丢方括号，这里修掉）
        let mut m6 = BTreeMap::new();
        m6.insert("::1".to_string(), 8443u16);
        assert_eq!(
            effective_url("wss://[::1]:443/x", "usb", &m6),
            "wss://[::1]:8443/x"
        );
    }

    #[test]
    fn effective_url_with_netconfig() {
        let mut port_map = BTreeMap::new();
        port_map.insert("a.cn".to_string(), 8443u16);
        let cfg = NetConfig::usb("192.168.7.1", port_map);
        assert!(cfg.is_usb());
        assert_eq!(
            effective_url_cfg("wss://a.cn/x", &cfg),
            "wss://a.cn:8443/x"
        );
        let wifi = NetConfig::wifi();
        assert!(!wifi.is_usb());
        assert_eq!(effective_url_cfg("wss://a.cn/x", &wifi), "wss://a.cn/x");
    }

    #[test]
    fn public_gateway_url_swaps_host_drops_port() {
        // 域名换网关：端口清空，path/query/fragment 原样
        assert_eq!(
            public_gateway_url("wss://api.ea.modelbest.co/e/o5/v1/realtime?mode=audio"),
            "wss://o5gw.modelbest.cn/e/o5/v1/realtime?mode=audio"
        );
        assert_eq!(
            public_gateway_url("wss://api.ea.modelbest.co:7029/v1/realtime"),
            "wss://o5gw.modelbest.cn/v1/realtime"
        );
        // userinfo 保守保留
        assert_eq!(
            public_gateway_url("wss://u:p@api.ea.modelbest.co/x"),
            "wss://u:p@o5gw.modelbest.cn/x"
        );
        // 空串 / 无 scheme / 无 host：原样
        assert_eq!(public_gateway_url(""), "");
        assert_eq!(public_gateway_url("not a url"), "not a url");
        assert_eq!(public_gateway_url("wss:///path"), "wss:///path");
    }

    #[test]
    fn effective_fg_url_public_then_usb() {
        let mut port_map = BTreeMap::new();
        port_map.insert("api.ea.modelbest.co".to_string(), 9443u16);
        // wifi + 公网关：换域名，端口清空
        assert_eq!(
            effective_fg_url("wss://api.ea.modelbest.co/x", true, "wifi", &port_map),
            "wss://o5gw.modelbest.cn/x"
        );
        // usb + 公网关：先换域名再查 portMap（网关域名不在表内 -> 原样端口）
        assert_eq!(
            effective_fg_url("wss://api.ea.modelbest.co:443/x", true, "usb", &port_map),
            "wss://o5gw.modelbest.cn/x"
        );
        // 公网关 + 网关域名也在 portMap：usb 端口改写仍生效
        let mut map2 = BTreeMap::new();
        map2.insert("o5gw.modelbest.cn".to_string(), 8443u16);
        assert_eq!(
            effective_fg_url("wss://api.ea.modelbest.co/x", true, "usb", &map2),
            "wss://o5gw.modelbest.cn:8443/x"
        );
        // 关公网：原样（wifi）或仅 usb 改写
        assert_eq!(
            effective_fg_url("wss://api.ea.modelbest.co/x", false, "wifi", &port_map),
            "wss://api.ea.modelbest.co/x"
        );
        assert_eq!(
            effective_fg_url("wss://api.ea.modelbest.co/x", false, "usb", &port_map),
            "wss://api.ea.modelbest.co:9443/x"
        );
        // NetConfig 版
        let cfg = NetConfig {
            net_public: true,
            ..NetConfig::usb("192.168.7.1", BTreeMap::new())
        };
        assert_eq!(
            effective_fg_url_cfg("wss://api.ea.modelbest.co/x", &cfg),
            "wss://o5gw.modelbest.cn/x"
        );
    }

    #[test]
    fn hosts_merge_add_remove_idempotent() {
        let base = "127.0.0.1 localhost\n\n192.168.1.23 maix\n";
        let desired = vec![
            "192.168.7.1 a.cn".to_string(),
            "192.168.7.1 b.cn".to_string(),
        ];
        // 新增块（顺带清掉原有空行——Python 口径）
        let out = merge_hosts_content(base, &desired).unwrap();
        assert_eq!(
            out,
            "127.0.0.1 localhost\n192.168.1.23 maix\n\n\
             # BEGIN o5-usb-relay\n192.168.7.1 a.cn\n192.168.7.1 b.cn\n# END o5-usb-relay\n"
        );
        // 幂等：块已一致，无事可做
        assert_eq!(merge_hosts_content(&out, &desired), None);
        // 移除块（desired 空）
        let cleared = merge_hosts_content(&out, &[]).unwrap();
        assert_eq!(cleared, "127.0.0.1 localhost\n192.168.1.23 maix\n");
        // 残块（只有 BEGIN）：按无块处理，残行统一清掉（映射行保留——Python 口径）
        let broken = "127.0.0.1 localhost\n# BEGIN o5-usb-relay\n192.168.7.1 a.cn\n";
        let out2 = merge_hosts_content(broken, &[]).unwrap();
        assert_eq!(out2, "127.0.0.1 localhost\n192.168.7.1 a.cn\n");
    }

    #[test]
    fn sync_hosts_file_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "o5-perf-hosts-{}-{}.test",
            std::process::id(),
            line!()
        ));
        let path = path.to_str().unwrap();
        std::fs::write(path, "127.0.0.1 localhost\n\n192.168.1.23 maix\n").unwrap();
        let mut port_map = BTreeMap::new();
        port_map.insert("a.cn".to_string(), 8443u16);
        port_map.insert("b.cn".to_string(), 9443u16);
        // usb：加块（按域名排序）
        sync_hosts_file(path, "usb", "192.168.7.1", &port_map).unwrap();
        let c1 = std::fs::read_to_string(path).unwrap();
        assert!(c1.contains("# BEGIN o5-usb-relay\n192.168.7.1 a.cn\n192.168.7.1 b.cn\n# END o5-usb-relay"));
        // 幂等
        sync_hosts_file(path, "usb", "192.168.7.1", &port_map).unwrap();
        let c2 = std::fs::read_to_string(path).unwrap();
        assert_eq!(c1, c2);
        // wifi：清块
        sync_hosts_file(path, "wifi", "192.168.7.1", &port_map).unwrap();
        let c3 = std::fs::read_to_string(path).unwrap();
        assert!(!c3.contains("o5-usb-relay"));
        assert!(c3.contains("192.168.1.23 maix"));
        std::fs::remove_file(path).ok();
        // 读失败 + desired 为空 -> Ok（不影响运行）
        assert!(sync_hosts_file(&format!("{path}.missing"), "wifi", "", &BTreeMap::new()).is_ok());
        // 读失败 + desired 非空 -> Err
        assert!(
            sync_hosts_file(&format!("{path}.missing"), "usb", "1.2.3.4", &port_map).is_err()
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn sync_hosts_noop_off_linux() {
        let mut port_map = BTreeMap::new();
        port_map.insert("a.cn".to_string(), 1u16);
        assert!(sync_hosts("usb", "1.2.3.4", &port_map).is_ok());
        assert!(sync_hosts_cfg(&NetConfig::usb("1.2.3.4", port_map)).is_ok());
    }

    #[test]
    fn endpoint_of_default_ports() {
        assert_eq!(endpoint_of("wss://h.cn/x"), Some(("h.cn".into(), 443)));
        assert_eq!(endpoint_of("https://h.cn"), Some(("h.cn".into(), 443)));
        assert_eq!(endpoint_of("ws://h.cn:123/x"), Some(("h.cn".into(), 123)));
        assert_eq!(endpoint_of("http://H.CN"), Some(("h.cn".into(), 80)));
        assert_eq!(endpoint_of("wss://[::1]:8443/x"), Some(("::1".into(), 8443)));
        assert_eq!(endpoint_of("not a url"), None);
        assert_eq!(endpoint_of("wss:///path"), None); // 空 host
        assert_eq!(endpoint_of(""), None);
    }

    #[test]
    fn dns_cache_expiry_and_fallback() {
        let now = Instant::now();
        let mut dns: DnsCache = HashMap::new();
        // 空 + 解析失败 -> None
        assert_eq!(resolve_cached(&mut dns, "h.cn", now, |_| None), None);
        // 解析成功 -> 缓存 60s
        assert_eq!(
            resolve_cached(&mut dns, "h.cn", now, |_| Some("1.1.1.1".into())),
            Some("1.1.1.1".into())
        );
        // 未过期：不再解析
        assert_eq!(
            resolve_cached(
                &mut dns,
                "h.cn",
                now + Duration::from_secs(30),
                |_| Some("2.2.2.2".into())
            ),
            Some("1.1.1.1".into())
        );
        // 过期：重解析
        assert_eq!(
            resolve_cached(
                &mut dns,
                "h.cn",
                now + Duration::from_secs(61),
                |_| Some("3.3.3.3".into())
            ),
            Some("3.3.3.3".into())
        );
        // 过期 + 解析失败：沿用旧地址
        assert_eq!(
            resolve_cached(&mut dns, "h.cn", now + Duration::from_secs(122), |_| None),
            Some("3.3.3.3".into())
        );
    }

    #[test]
    fn probe_round_measures_local_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("ws://127.0.0.1:{port}/x");
        let timing = Timing::new();
        let mut dns: DnsCache = HashMap::new();
        probe_round(&|| url.clone(), &mut dns, &timing);
        let rtt = timing.snapshot().net_rtt.expect("rtt sample");
        assert!(rtt.last_ms >= 0.0);
        assert!(rtt.last_ms < 1000.0);
    }

    #[test]
    fn probe_stops_within_interval() {
        let probe = NetRttProbe::new();
        let timing = Arc::new(Timing::new());
        let handle = probe.spawn(String::new, timing, 0.05);
        thread::sleep(Duration::from_millis(60));
        probe.stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() {
            assert!(Instant::now() < deadline, "probe 线程未退出");
            thread::sleep(Duration::from_millis(5));
        }
        handle.join().unwrap();
    }

    #[test]
    fn wait_stop_waits_and_interrupts() {
        let stop = AtomicBool::new(false);
        let t0 = Instant::now();
        assert!(!wait_stop(&stop, Duration::from_millis(40)));
        assert!(t0.elapsed() >= Duration::from_millis(30));
        stop.store(true, Ordering::Relaxed);
        assert!(wait_stop(&stop, Duration::from_secs(10)));
    }
}

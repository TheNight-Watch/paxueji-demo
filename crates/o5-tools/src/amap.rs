//! map_search（高德 Web 服务：v3/place/text 地点搜索 + v3/direction 驾车/步行路线）。

use crate::http;
use crate::textutil::{
    fmt_distance, fmt_duration, py_num, py_num_opt, py_or_str, py_str, py_truthy, truncate_chars,
    urlencode,
};
use crate::{ToolError, ToolExecutor, AMAP_GATEWAY_KEY_PREFIX, AMAP_PLACE_DEFAULT, AMAP_PLACE_MAX};
use serde_json::Value;

pub(crate) struct Place {
    pub(crate) name: String,
    pub(crate) address: String,
    pub(crate) lng: f64,
    pub(crate) lat: f64,
    pub(crate) distance: Option<i64>,
}

struct Route {
    distance_m: i64,
    duration_s: i64,
    roads: Vec<String>,
    traffic_lights: Option<i64>,
    taxi_cost: Option<f64>,
}

enum AmapMode {
    Gateway,
    Direct,
}

impl ToolExecutor {
    /// (mode, base)：sk- Key 走 EA 网关 Bearer；其余直连 restapi.amap.com。
    /// 没配 Key 返回 None（调用方回「未配置」）。
    fn amap_endpoint(&self) -> Option<(AmapMode, String)> {
        if self.amap_key.is_empty() {
            return None;
        }
        if self.amap_key.starts_with(AMAP_GATEWAY_KEY_PREFIX) {
            let host = self.amap_host.trim_end_matches('/');
            // amapServiceHost 仅当它是网关服务地址（路径以 /services/amap 结尾）才当 base
            if (host.starts_with("http://") || host.starts_with("https://"))
                && host.ends_with("/services/amap")
            {
                return Some((AmapMode::Gateway, host.to_string()));
            }
            return Some((AmapMode::Gateway, self.endpoints.ea_gateway.clone()));
        }
        Some((AmapMode::Direct, self.endpoints.amap_rest.clone()))
    }

    /// GET {base}{path}?{params}：直连带 ?key=，网关带 Bearer；status!=1 -> AmapError（Key 遮蔽）。
    fn amap_get(&self, path: &str, params: Vec<(&str, String)>) -> Result<Value, ToolError> {
        let (mode, base) = self
            .amap_endpoint()
            .ok_or_else(|| ToolError::Amap("未配置高德 Web 服务 API Key".to_string()))?;
        let mut params = params;
        params.push(("output", "JSON".to_string()));
        let mut headers: Vec<(&str, String)> = vec![("Accept", "application/json".to_string())];
        match mode {
            AmapMode::Gateway => headers.push(("Authorization", format!("Bearer {}", self.amap_key))),
            AmapMode::Direct => params.push(("key", self.amap_key.clone())),
        }
        let pairs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let url = format!("{base}{path}?{}", urlencode(&pairs));
        let resp = http::get(&url, &headers, self.amap_timeout_s)?;
        if resp.status >= 400 {
            return Err(ToolError::Amap(format!("高德接口 HTTP {}", resp.status)));
        }
        let data = http::parse_json_body(&resp.body)?;
        let status_str = match data.get("status") {
            Some(v) => py_str(v),
            None => "None".to_string(),
        };
        if status_str != "1" {
            let mut info = match data.get("info") {
                Some(v) if py_truthy(v) => py_str(v),
                _ => "未知错误".to_string(),
            };
            info = truncate_chars(&info, 100);
            if !self.amap_key.is_empty() {
                info = info.replace(&self.amap_key, "***"); // 报错原文里可能出现 Key，抹掉
            }
            return Err(ToolError::Amap(format!("高德返回：{info}")));
        }
        Ok(data)
    }

    /// v3/place/text：city+citylimit、types、offset=max。POI 缺坐标的丢弃。
    pub(crate) fn search_places(
        &self,
        query: &str,
        city: &str,
        types: &str,
        max_n: usize,
        city_limit: bool,
    ) -> Result<Vec<Place>, ToolError> {
        let mut params: Vec<(&str, String)> = vec![
            ("keywords", query.to_string()),
            ("offset", max_n.to_string()),
            ("page", "1".to_string()),
            ("extensions", "base".to_string()),
        ];
        if !city.is_empty() {
            params.push(("city", city.to_string()));
            if city_limit {
                params.push(("citylimit", "true".to_string()));
            }
        }
        if !types.is_empty() {
            params.push(("types", types.to_string()));
        }
        let data = self.amap_get("/v3/place/text", params)?;
        let pois = data
            .get("pois")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        let mut places = Vec::new();
        for poi in pois.iter().take(max_n) {
            let Some((lng, lat)) = parse_location(poi.get("location")) else {
                continue;
            };
            let distance = py_num_opt(poi.get("distance")).filter(|d| d.is_finite());
            places.push(Place {
                name: py_or_str(poi.get("name"), query).trim().to_string(),
                address: py_or_str(poi.get("address"), "").trim().to_string(),
                lng,
                lat,
                distance: distance.map(|d| d.trunc() as i64),
            });
        }
        Ok(places)
    }

    /// 起终点解析：坐标串直接用；地名取首个 POI（先城市内，找不到放开）。
    fn resolve_place(&self, text: &str, city: &str) -> Result<Option<Place>, ToolError> {
        let t = text.trim();
        if let Some((lng, lat)) = parse_coordinate_text(t) {
            return Ok(Some(Place {
                name: t.to_string(),
                address: String::new(),
                lng,
                lat,
                distance: None,
            }));
        }
        let scoped = self.search_places(t, city, "", 1, !city.is_empty())?;
        if !scoped.is_empty() || city.is_empty() {
            return Ok(scoped.into_iter().next());
        }
        // 城市内找不到：放开城市限制再试一次（「北京西站到天津站」）
        let opened = self.search_places(t, "", "", 1, false)?;
        Ok(opened.into_iter().next())
    }

    /// v3/direction/driving|walking：距离/时长/途经路名（≤4）/红绿灯/打车费。
    fn plan_route(&self, mode: &str, origin: &Place, destination: &Place) -> Result<Route, ToolError> {
        let path = if mode == "driving" {
            "/v3/direction/driving"
        } else {
            "/v3/direction/walking"
        };
        let params = vec![
            ("origin", format!("{:.6},{:.6}", origin.lng, origin.lat)),
            ("destination", format!("{:.6},{:.6}", destination.lng, destination.lat)),
        ];
        let data = self.amap_get(path, params)?;
        let route = data.get("route").filter(|v| py_truthy(v)); // data.get("route") or {}
        let paths = route.and_then(|r| r.get("paths")).and_then(|v| v.as_array());
        let first = paths
            .and_then(|p| p.first())
            .ok_or_else(|| ToolError::Amap("高德没有找到可用路线".to_string()))?;
        let distance = py_num_opt(first.get("distance"))
            .filter(|d| d.is_finite())
            .ok_or_else(|| ToolError::Amap("高德路线缺少距离或用时".to_string()))?;
        let duration = py_num_opt(first.get("duration"))
            .filter(|d| d.is_finite())
            .ok_or_else(|| ToolError::Amap("高德路线缺少距离或用时".to_string()))?;
        // 途经路名：按 instruction 非空的步骤收集前 4 个不重复 road（steps 文本不参与输出）
        let mut roads: Vec<String> = Vec::new();
        let steps = first
            .get("steps")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        for step in steps {
            if py_or_str(step.get("instruction"), "").trim().is_empty() {
                continue;
            }
            let road = py_or_str(step.get("road"), "").trim().to_string();
            if !road.is_empty() && !roads.contains(&road) && roads.len() < 4 {
                roads.push(road);
            }
        }
        let lights = py_num_opt(first.get("traffic_lights")).filter(|f| f.is_finite());
        let taxi = route.and_then(|r| py_num_opt(r.get("taxi_cost")));
        Ok(Route {
            distance_m: distance.trunc() as i64,
            duration_s: duration.trunc() as i64,
            roads,
            traffic_lights: lights.map(|l| l.trunc() as i64),
            taxi_cost: taxi,
        })
    }
}

/// 高德 location 串（"lng,lat"）-> (lng, lat)；坏值 None。
pub(crate) fn parse_location(v: Option<&Value>) -> Option<(f64, f64)> {
    let s = v?.as_str()?;
    let (lng_s, lat_s) = s.split_once(',')?;
    let lng = crate::textutil::py_float_str(lng_s)?;
    let lat = crate::textutil::py_float_str(lat_s)?;
    if lng.abs() > 180.0 || lat.abs() > 90.0 {
        return None;
    }
    Some((lng, lat))
}

/// 模型给的坐标串（"116.34,39.96"，经度在前）直接用；写反了按取值范围纠正。
/// 等价正则：^\s*(-?\d{1,3}(?:\.\d+)?)\s*[,，]\s*(-?\d{1,3}(?:\.\d+)?)\s*$（含全角逗号）。
pub(crate) fn parse_coordinate_text(text: &str) -> Option<(f64, f64)> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    fn skip_ws(chars: &[char], i: &mut usize) {
        while *i < chars.len() && chars[*i].is_whitespace() {
            *i += 1;
        }
    }
    fn parse_num(chars: &[char], i: &mut usize) -> Option<f64> {
        skip_ws(chars, i);
        if *i < chars.len() && chars[*i] == '-' {
            *i += 1;
        }
        let start = *i;
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            *i += 1;
        }
        let digits = *i - start;
        if digits == 0 || digits > 3 {
            return None;
        }
        let mut text: String = chars[start..*i].iter().collect();
        if *i < chars.len() && chars[*i] == '.' {
            let frac_start = *i + 1;
            let mut j = frac_start;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j == frac_start {
                return None; // 点后必须跟数字（"116." 不匹配）
            }
            text.push('.');
            text.extend(chars[frac_start..j].iter());
            *i = j;
        }
        text.parse().ok()
    }
    let first = parse_num(&chars, &mut i)?;
    skip_ws(&chars, &mut i);
    match chars.get(i) {
        Some(',') | Some('，') => i += 1,
        _ => return None,
    }
    let second = parse_num(&chars, &mut i)?;
    skip_ws(&chars, &mut i);
    if i != chars.len() {
        return None;
    }
    if second.abs() <= 90.0 {
        return Some((first, second));
    }
    if first.abs() <= 90.0 {
        return Some((second, first)); // 写反了：lat 在前
    }
    None
}

/// map_search 入口：未配置直说；起终点齐全查路线，否则按 query 搜地点。
pub(crate) fn map_search(ex: &ToolExecutor, args: &Value) -> Result<String, ToolError> {
    if ex.amap_endpoint().is_none() {
        return Ok("[未配置] 高德地图需要 Web 服务 API Key（配置 tools.amapApiKey）。\
                   如实告诉用户地图服务还没配置；不要编造坐标、不要估算距离。"
            .to_string());
    }
    let arg_text = |key: &str, limit: usize| -> String {
        py_or_str(args.get(key), "")
            .trim()
            .chars()
            .take(limit)
            .collect()
    };
    let query = arg_text("query", 200);
    let city = arg_text("city", 50);
    let types = arg_text("types", 100);
    let origin = arg_text("origin", 200);
    let destination = arg_text("destination", 200);
    let mut mode = arg_text("mode", 20);
    if mode != "driving" && mode != "walking" {
        mode = "driving".to_string();
    }
    let mut raw_max = py_num(args.get("max"), AMAP_PLACE_DEFAULT as f64);
    if raw_max == 0.0 || !raw_max.is_finite() {
        raw_max = AMAP_PLACE_DEFAULT as f64;
    }
    let max_n = (raw_max.trunc() as i64).clamp(1, AMAP_PLACE_MAX) as usize;

    if !origin.is_empty() || !destination.is_empty() {
        if origin.is_empty() || destination.is_empty() {
            return Ok(
                "[参数错误] map_search：查路线要同时给 origin（起点）和 destination（终点）"
                    .to_string(),
            );
        }
        let o = ex.resolve_place(&origin, &city)?;
        let d = ex.resolve_place(&destination, &city)?;
        let scope = if city.is_empty() { String::new() } else { format!("（{city}）") };
        let Some(o) = o else {
            return Ok(format!(
                "[无结果] 高德没有找到起点「{origin}」{scope}。换个更具体的叫法再查，不要编造位置。"
            ));
        };
        let Some(d) = d else {
            return Ok(format!(
                "[无结果] 高德没有找到终点「{destination}」{scope}。换个更具体的叫法再查，不要编造位置。"
            ));
        };
        let route = ex.plan_route(&mode, &o, &d)?;
        let mode_label = if mode == "driving" { "驾车" } else { "步行" };
        let mut extras: Vec<String> = Vec::new();
        if let Some(l) = route.traffic_lights {
            if l != 0 {
                extras.push(format!("红绿灯约 {l} 个"));
            }
        }
        if let Some(t) = route.taxi_cost {
            if t != 0.0 {
                extras.push(format!("打车约 {:.0} 元", t));
            }
        }
        let mut out = format!(
            "{} → {}：{mode_label}约 {}，约 {}。",
            o.name,
            d.name,
            fmt_distance(route.distance_m),
            fmt_duration(route.duration_s)
        );
        if !route.roads.is_empty() {
            out += &format!("途经{}。", route.roads.join("、"));
        }
        if !extras.is_empty() {
            out += &format!("{}。", extras.join("，"));
        }
        out += "（按这里的数字回答，不要另行估算）";
        return Ok(out);
    }

    if query.is_empty() {
        return Ok(
            "[参数错误] map_search：搜地点要给非空 query；查路线 / 距离要给 origin 和 destination"
                .to_string(),
        );
    }
    let places = ex.search_places(&query, &city, &types, max_n, false)?;
    let scope = if city.is_empty() { String::new() } else { format!("（{city}）") };
    if places.is_empty() {
        return Ok(format!(
            "[无结果] 高德没有找到「{query}」{scope}相关地点。换个更具体的叫法（带上城市 / 区）再查，不要编造位置。"
        ));
    }
    let mut lines: Vec<String> = Vec::new();
    for (i, p) in places.iter().enumerate() {
        let mut seg = format!("{}. {}", i + 1, p.name);
        if !p.address.is_empty() {
            seg += &format!("｜{}", p.address);
        }
        if let Some(dist) = p.distance {
            seg += &format!("｜距离 {}", fmt_distance(dist));
        }
        lines.push(seg);
    }
    Ok(format!(
        "高德找到 {} 个「{query}」相关地点{scope}：\n{}\n（结论里一句话说结果即可；坐标可当 origin/destination 查路线）",
        places.len(),
        lines.join("\n")
    ))
}

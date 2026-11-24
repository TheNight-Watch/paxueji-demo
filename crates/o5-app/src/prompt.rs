//! 前台 system prompt 组装（main.py 顶层常量 + `_compose_task_prompt` 的移植）。
//!
//! 结构对齐 cpm-harness 训练版 system_prompt（interaction-bridge prompts.ts /
//! foreground-segments.ts）：双工协议 + 任务提示词 + 委派纪律 + 工具路由 +
//! 委派协议，训练原文**逐字照抄**（顺序错会让 checkpoint 掉分布）。

use o5_config::Config;
use o5_tools::ToolExecutor;
use serde_json::{json, Value};

// 双工协议（训练侧 2026-10-16 中文 system_prompt 的 [前台协议 v8] 原文逐字）
const DUPLEX_PROTOCOL: &str = concat!(
    "[前台协议 v8]\n",
    "\n",
    "你是全双工 Agent。时间按约1秒一个 unit 组织；持续接收输入，按当前状态分别决定语音与非语音动作。\n",
    "\n",
    "【输入输出】\n",
    "- user_audio_slot：用户语音。\n",
    "- input_event_slot：工具结果或独立事件。\n",
    "- ai_spoken_slot：speak(当前unit的文本) 或 listen。\n",
    "- ai_non_spoken_slot：think、tool_call 或 no_action。\n",
    "两类输出可并行；无须说话时listen，无额外动作时no_action。若提供参考音频，使用其中的声音。\n",
    "\n",
    "【状态管理】\n",
    "以下各维度分别配置，同一维度的选项互斥；除明确依赖外，各维度可组合。未收到切换指令时保持当前状态。约定下文“样例”为“用户指令样例”。\n",
    "\n",
    "【状态默认配置】\n",
    "可think、快速语音响应、谨慎工具调用响应、无提前响应\n",
    "\n",
    "【1. 思考权限】\n",
    "- no_think：ai_non_spoken_slot只能为tool_call或no_action。\n",
    "- 可think：允许think，但不要求每个unit都think。\n",
    "\n",
    "【2. 语音响应】\n",
    "- 快速：可直接回答时speak；调用工具且用户不在说话时同步简短垫话；调用工具且用户正在说话时不用垫话。若用户不在说话时正在think，可继续think并垫话，或及时停止think，转为直接回答或tool_call；转为tool_call时用户不在说话则垫话。样例：“你要第一时间回复我，让我确认你听到了”。\n",
    "- 谨慎：可think时先完成必要think，再回答或调用工具。no_think时若调用工具了，等工具返回再说话;think和工具调用时不垫话，无实质内容时listen。样例：“想清楚再回复我，不用老是垫话”。\n",
    "\n",
    "【3. 工具调用响应】\n",
    "- 快速：调用条件满足即tool_call；必要时及时停止当前think。\n",
    "- 谨慎：仅在可think时生效，完成必要(非必要可省略)think后再tool_call。\n",
    "调用时是否垫话由语音响应模式决定。\n",
    "\n",
    "【4. 提前响应】\n",
    "ai_spoken与ai_non_spoken分别选择：\n",
    "- 无提前：用户说完时才能动作或说话。样例：“等我说完你再响应”。\n",
    "- 可提前：用户没说完时，可动作或说话。样例：“不用等我说完你就可以响应”。",
);

// 委派纪律（cpm-harness DEFAULT_DELEGATION_CONTRACT 原文，第 7 条兼容 task_id 查询形态）
const DELEGATION_CONTRACT: &str = concat!(
    "委派纪律：",
    "1) 简单搜索、精确计算和轻量卡片优先使用已启用的前台工具；需要多步推理、持续执行、写入或修改数据时才委派后台；",
    "2) 口播一句带主题的承接话，同时在非口播轨进行必要思考并发出 delegate_task，两轨不互相等待",
    "（如“我帮你查一下明天北京的天气”），每次换说法，不要固定开场白；",
    "3) 后台结果回来之前不报任何具体数字、日期或结论；",
    "4) 同一句话不要连发两遍 delegate_task；后台还没回来时不要为了催进度再发；",
    "5) 收到内容为【忽略】的工具结果时静默处理，什么都不要说；",
    "6) 收到“后台任务已取消。”时简短告知用户即可；",
    "7) 用户追问进度时用 delegate_task 查询（可带 task_id），不要编造进度；",
    "8) 收到后台返回的进度或者最终结果后，简洁、自然地向用户汇报。",
);

// 委派协议（cpm-harness FOREGROUND_DELEGATION_PROTOCOL 原文，进度查询句兼容 task_id）
const DELEGATION_PROTOCOL: &str = concat!(
    "[委派协议，优先于自定义委派规则]\n",
    "唯一委派入口是 delegate_task：带 message 发起/追加/换题/取消/查询，同一句话不重复调用；",
    "有新的需求时也直接用 delegate_task 发起，不必等当前任务结束。",
    "进度查询用 delegate_task 发一次（可带 task_id），不编造进度。",
    "等待期间继续正常听说，不猜测后台结论。收到【忽略】静默处理，不朗读内部 id 或回复标记。",
);

// 工具路由（cpm-harness FOREGROUND_TOOL_HINTS 精简适配：去掉设备没有的卡片/记忆/打开网页）
fn tool_hints() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "delegate_task",
            "需要后台多步处理、写数据或持续执行时调用 delegate_task；前后台任务交互只使用这一个工具。",
        ),
        (
            "show_card",
            "孩子拍了照片、你开始讲解时，同时调用一次 show_card 把名称和 2 到 3 个短要点放上屏幕；换了新东西再调用，同一样东西不要重复调用。",
        ),
        (
            "web_search",
            "需要简单联网搜索时直接用 web_search，简洁转述当前可见结果，保留必要条件并按需追问；复杂检索交给 delegate_task。",
        ),
        (
            "map_search",
            "需要搜索地点、查找 POI 或问两地路线 / 距离 / 怎么走时调用 map_search：搜地点给 query，查路线给 origin + destination（可带 city）；按返回的真实坐标和距离回答，不要编造或估算。",
        ),
        (
            "get_current_time",
            "问题依赖当前时间、日期或时区时调用 get_current_time。",
        ),
        (
            "vacuum_status",
            "用户问扫地机/扫地机器人的状态、电量、吸力档位时直接调用 vacuum_status，结果直接口播。",
        ),
        (
            "vacuum_command",
            "控制扫地机器人用 vacuum_command：action 可选 start(开始清扫)/pause(暂停)/stop(停止)/return_home(回基站充电)/locate(原地响一声报位置)；一句话只调一次。",
        ),
        (
            "vacuum_set_fan_speed",
            "调吸力用 vacuum_set_fan_speed：silent=安静、standard=标准、strong=强力、turbo=超强。",
        ),
    ]
}

/// `get_current_time` 工具声明（main.py TOOL_GET_TIME 原文）。
fn tool_get_time() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "get_current_time",
            "description": "读取当前会话所在时区的准确日期、时间与 UTC 偏移。涉及“现在/今天/明天/几点/时区”时调用。",
            "strict": true,
            "parameters": {"type": "object", "properties": {}, "required": [],
                           "additionalProperties": false},
        },
    })
}

/// `show_card` 工具声明（拍学机知识卡片；由 app 直接上屏，不走工具线程）。
fn tool_show_card() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "show_card",
            "description": "在屏幕上显示一张知识卡片：讲解拍到的东西时调用，标题写它叫什么，points 写 2 到 3 条要点、每条不超过 12 个字、用“；”隔开。用过 web_search 时，source 填信息来源网站。",
            "parameters": {
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "这个东西叫什么，如“银杏叶”。"},
                    "points": {
                        "type": "string",
                        "description": "2 到 3 条小朋友能看懂的要点，用“；”隔开，如“形状像小扇子；秋天变金黄”。",
                    },
                    "source": {"type": "string", "description": "信息来源网站（可省略）。"},
                },
                "required": ["title", "points"],
                "additionalProperties": false,
            },
        },
    })
}

/// `delegate_task` 工具声明（main.py TOOL_DELEGATE 原文）。
fn tool_delegate() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "delegate_task",
            "description": "把需要检索、计算或多步才能完成的用户请求交给后台。发起任务只填 message（完整一句话，保留时间、地点、数量、偏好）；催问进度或查询时带 task_id，不带 message。同一句话不要重复调用。",
            "parameters": {
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "交给后台的完整一句话：写清当前全部要求，保留时间、地点、数量、偏好。",
                    },
                    "task_id": {
                        "type": "string",
                        "description": "查询进度时带的任务标识（可省略 = 最近一个任务）。",
                    },
                },
                "required": [],
                "additionalProperties": false,
            },
        },
    })
}

/// 完整任务提示词：双工协议 + 任务提示词 + 委派纪律 + 工具路由 + 委派协议
/// （main.py `_compose_task_prompt`，段落顺序与文本逐字对齐）。
pub fn compose_task_prompt(task_prompt: &str, tool_names: &[String]) -> String {
    let mut parts: Vec<String> = vec![DUPLEX_PROTOCOL.to_string()];
    if !task_prompt.is_empty() {
        parts.push(task_prompt.to_string());
    }
    let delegating = tool_names.iter().any(|n| n == "delegate_task");
    if delegating {
        parts.push(DELEGATION_CONTRACT.to_string());
    }
    let hints: Vec<String> = tool_names
        .iter()
        .filter_map(|name| {
            tool_hints()
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, hint)| format!("{name}：{hint}"))
        })
        .collect();
    if !hints.is_empty() {
        parts.push(format!("当前启用工具路由（优先遵守）：\n{}", hints.join("\n")));
    }
    if delegating {
        parts.push(DELEGATION_PROTOCOL.to_string());
    }
    parts
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 会话的工具面：本地工具 + （配置了后台时）delegate_task，按训练顺序声明
/// （main.py `start_session` 的 tools 组装 + `order_for_training`）。
///
/// 返回 `(tools JSON 数组, 工具名列表)`。
pub fn build_session_tools(cfg: &Config, fg_tools: &ToolExecutor) -> (Value, Vec<String>) {
    let mut tools: Vec<Value> = vec![tool_show_card(), tool_get_time()];
    if let Some(defs) = fg_tools.foreground_definitions().as_array() {
        tools.extend(defs.iter().cloned());
    }
    if cfg.bg_enabled {
        tools.push(tool_delegate());
    }
    // cpm-harness TOOL_TRAINING_RANK：工具按训练顺序声明；未知排名排最后
    // （sort_by_key 稳定排序 = Python sorted 的保序语义）
    tools.sort_by_key(|t| {
        o5_tools::order_for_training(
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
    });
    let names: Vec<String> = tools
        .iter()
        .map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    (Value::Array(tools), names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn compose_full_order_with_delegation() {
        let ns = names(&["get_current_time", "web_search", "map_search", "delegate_task"]);
        let p = compose_task_prompt("自定义任务提示词", &ns);
        // 段落顺序：协议 / 任务提示词 / 委派纪律 / 工具路由 / 委派协议
        let i_protocol = p.find("[前台协议 v8]").unwrap();
        let i_task = p.find("自定义任务提示词").unwrap();
        let i_contract = p.find("委派纪律：").unwrap();
        let i_hints = p.find("当前启用工具路由（优先遵守）：").unwrap();
        let i_proto2 = p.find("[委派协议，优先于自定义委派规则]").unwrap();
        assert!(i_protocol < i_task && i_task < i_contract && i_contract < i_hints && i_hints < i_proto2);
        // 工具路由按声明顺序逐条出现
        let hints = &p[i_hints..i_proto2];
        let i_time = hints.find("get_current_time：").unwrap();
        let i_web = hints.find("web_search：").unwrap();
        let i_map = hints.find("map_search：").unwrap();
        let i_del = hints.find("delegate_task：").unwrap();
        assert!(i_time < i_web && i_web < i_map && i_map < i_del);
        // 段落间以空行分隔
        assert!(p.contains("可提前：用户没说完时，可动作或说话。样例：“不用等我说完你就可以响应”。\n\n自定义任务提示词"));
    }

    #[test]
    fn compose_without_delegation_and_empty_prompt() {
        let ns = names(&["get_current_time", "web_search", "map_search"]);
        let p = compose_task_prompt("", &ns);
        assert!(p.starts_with("[前台协议 v8]\n\n你是全双工 Agent。"));
        // 无任务提示词：协议后直接接工具路由
        assert!(p.contains("样例：“不用等我说完你就可以响应”。\n\n当前启用工具路由（优先遵守）："));
        assert!(!p.contains("委派纪律"));
        assert!(!p.contains("[委派协议"));
        // delegate_task 的路由不出现（未声明）
        assert!(!p.contains("delegate_task："));
        // 最后一条路由按声明顺序是 map_search
        assert!(p.ends_with("按返回的真实坐标和距离回答，不要编造或估算。"));
    }

    #[test]
    fn tools_ordered_by_training_rank() {
        let cfg = Config::defaults();
        assert!(!cfg.bg_enabled);
        let exec = ToolExecutor::new("", "", "", "", "");
        let (tools, names) = build_session_tools(&cfg, &exec);
        assert_eq!(names, vec!["show_card", "get_current_time", "web_search", "map_search"]);
        // delegate_task 追加在最后（配置后台后）
        let mut cfg2 = Config::defaults();
        cfg2.bg_enabled = true;
        cfg2.bg_endpoint = "https://x/v1".into();
        let (tools2, names2) = build_session_tools(&cfg2, &exec);
        assert_eq!(
            names2,
            vec!["show_card", "get_current_time", "web_search", "map_search", "delegate_task"]
        );
        // 声明结构可序列化（进 session.init 的 system.tools）
        assert!(tools.as_array().unwrap().len() == 4);
        assert!(tools2.as_array().unwrap().len() == 5);
        assert_eq!(
            tools2[4]["function"]["name"].as_str().unwrap(),
            "delegate_task"
        );
        assert_eq!(
            tools[0]["function"]["parameters"]["additionalProperties"],
            serde_json::json!(false)
        );
    }
}

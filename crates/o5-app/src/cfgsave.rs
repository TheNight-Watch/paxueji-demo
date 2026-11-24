//! 配置字段级写回（main.py 用的 o5/config.py `save_audio_volume` 等的 app 侧实现）。
//!
//! Python 侧这几个 helper 在 config.py 里；Rust 的 o5-config 契约只有整体
//! `save`，按「不改其他 crate」的纪律把字段级深合并写回放在 app 里实现
//! （语义一致：读 JSON -> 深合并 patch -> 原子替换，无文件/失败返回 false）。

use std::path::Path;

use serde_json::Value;

/// 把 `audio.outputVolume` 写回配置文件（设置页音量调节持久化）。
/// 无文件 / 失败返回 false。
pub fn save_audio_volume(path: Option<&str>, volume: u8) -> bool {
    save_config_field(
        path,
        &serde_json::json!({"audio": {"outputVolume": volume}}),
    )
}

/// 把 `network.publicNetwork` 写回配置文件（更多菜单公网调用开关持久化）。
/// 无文件 / 失败返回 false。
pub fn save_public_network(path: Option<&str>, enabled: bool) -> bool {
    save_config_field(
        path,
        &serde_json::json!({"network": {"publicNetwork": enabled}}),
    )
}

/// 把 `asr.enabled` 写回配置文件（更多菜单 ASR 转写开关持久化）。
/// 无文件 / 失败返回 false。
pub fn save_asr_enabled(path: Option<&str>, enabled: bool) -> bool {
    save_config_field(path, &serde_json::json!({"asr": {"enabled": enabled}}))
}

/// 深合并写回配置文件的指定字段（Python `_save_config_field`）。
fn save_config_field(path: Option<&str>, patch: &Value) -> bool {
    let Some(p) = path else { return false };
    if !Path::new(p).is_file() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(p) else { return false };
    let Ok(mut root) = serde_json::from_str::<Value>(&text) else { return false };
    deep_merge(&mut root, patch);
    let Ok(out) = serde_json::to_string_pretty(&root) else { return false };
    let tmp = format!("{p}.o5tmp");
    std::fs::write(&tmp, out)
        .and_then(|_| std::fs::rename(&tmp, p))
        .is_ok()
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_cfg(name: &str) -> String {
        let p = std::env::temp_dir().join(format!("o5app-cfg-{}-{name}", std::process::id()));
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn save_volume_roundtrip_and_missing_file() {
        let p = tmp_cfg("vol.json");
        std::fs::write(
            &p,
            r#"{"audio": {"outputVolume": 80, "gateTailMs": 200}, "foreground": {"mode": "audio"}}"#,
        )
        .unwrap();
        assert!(save_audio_volume(Some(&p), 37));
        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(root["audio"]["outputVolume"], json!(37));
        // 其他字段不受影响
        assert_eq!(root["audio"]["gateTailMs"], json!(200));
        assert_eq!(root["foreground"]["mode"], json!("audio"));
        assert!(!std::path::Path::new(&format!("{p}.o5tmp")).exists());

        assert!(!save_audio_volume(None, 50));
        assert!(!save_audio_volume(Some("Z:/不存在/none.json"), 50));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_public_network_merges_nested_section() {
        let p = tmp_cfg("pubnet.json");
        std::fs::write(
            &p,
            r#"{"network": {"mode": "wifi", "usbRelay": {"host": "h", "portMap": {}}}}"#,
        )
        .unwrap();
        assert!(save_public_network(Some(&p), true));
        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(root["network"]["publicNetwork"], json!(true));
        // 同节其他字段不受影响（深合并，不是整节替换）
        assert_eq!(root["network"]["mode"], json!("wifi"));
        assert_eq!(root["network"]["usbRelay"]["host"], json!("h"));
        assert!(!save_public_network(None, false));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_asr_enabled_merges_nested_section() {
        let p = tmp_cfg("asr_switch.json");
        std::fs::write(
            &p,
            r#"{"asr": {"endpoint": "wss://x/", "serviceId": 2, "enabled": true}}"#,
        )
        .unwrap();
        assert!(save_asr_enabled(Some(&p), false));
        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(root["asr"]["enabled"], json!(false));
        // 同节其他字段不受影响
        assert_eq!(root["asr"]["endpoint"], json!("wss://x/"));
        assert_eq!(root["asr"]["serviceId"], json!(2));
        assert!(!save_asr_enabled(None, true));
        let _ = std::fs::remove_file(&p);
    }
}

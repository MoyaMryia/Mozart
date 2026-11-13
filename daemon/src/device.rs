//! 音频设备枚举（SPEC-04）：ALSA `snd_device_name_hint` 按需查询，不缓存
//! （USB 插拔）。pre 只打开配置的设备，不做枚举。

use alsa::device_name::HintIter;
use alsa::Direction;
use mozart_core::ctl;
use mozart_core::ctl::schema::{self, DeviceEntry};
use serde_json::json;

/// 枚举采集/播放设备：id 为 ALSA 名（配置用），label 为人读标签。
/// IOID 缺省的双向设备各出一条 capture 与 playback。
/// 枚举失败带原因返回；无声卡主机通常得到空清单。
pub fn list() -> Result<Vec<DeviceEntry>, (String, String)> {
    let hints = HintIter::new_str(None, "pcm").map_err(|err| {
        (
            ctl::CODE_INTERNAL.to_string(),
            format!("snd_device_name_hint: {err}"),
        )
    })?;
    let mut devices = Vec::new();
    for hint in hints {
        let Some(id) = hint.name else { continue };
        // DESC 可能多行（卡名 + 设备名），压成单行人读标签。
        let label = hint
            .desc
            .map(|desc| desc.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| id.clone());
        match hint.direction {
            Some(Direction::Capture) => devices.push(entry(&id, &label, "capture")),
            Some(Direction::Playback) => devices.push(entry(&id, &label, "playback")),
            None => {
                devices.push(entry(&id, &label, "capture"));
                devices.push(entry(&id, &label, "playback"));
            }
        }
    }
    Ok(devices)
}

fn entry(id: &str, label: &str, kind: &str) -> DeviceEntry {
    DeviceEntry {
        id: id.to_string(),
        label: label.to_string(),
        kind: kind.to_string(),
    }
}

/// 默认清单只保留配置首选：plughw（带软件转换的硬件）与 default。
/// 其余 ALSA 别名（hw/dmix/dsnoop/surround 等）经 `device list --all` 查看。
fn is_default_visible(id: &str) -> bool {
    id == "default" || id.starts_with("plughw:")
}

/// 枚举并按 all 筛选，组装 DAEMON_DEVICE_LIST 应答。
/// enumerate 注入真实 ALSA 枚举或测试夹具；筛选规则见 is_default_visible。
pub fn list_reply(
    all: bool,
    enumerate: impl Fn() -> Result<Vec<DeviceEntry>, (String, String)>,
) -> (Vec<u8>, bool) {
    match enumerate() {
        Ok(devices) => {
            let devices = if all {
                devices
            } else {
                devices
                    .into_iter()
                    .filter(|device| is_default_visible(&device.id))
                    .collect()
            };
            (schema::ok_body(json!({"devices": devices})), false)
        }
        Err((code, reason)) => (schema::error_body(&code, &reason), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 主机可能无声卡：容忍空清单；有条目则字段必须完整、方向合法。
    #[test]
    fn lists_devices_with_valid_fields() {
        let devices = list().expect("ALSA 枚举应成功（无声卡时为空清单）");
        for device in &devices {
            assert!(!device.id.is_empty());
            assert!(!device.label.is_empty());
            assert!(
                device.kind == "capture" || device.kind == "playback",
                "非法方向：{}",
                device.kind
            );
        }
    }

    #[test]
    fn default_visibility_keeps_plughw_and_default() {
        assert!(is_default_visible("default"));
        assert!(is_default_visible("plughw:CARD=Loopback,DEV=1"));
        assert!(!is_default_visible("hw:CARD=Loopback,DEV=1"));
        assert!(!is_default_visible("dmix:CARD=APE,DEV=0"));
        assert!(!is_default_visible("sysdefault:CARD=APE"));
        assert!(!is_default_visible("null"));
    }

    /// 接线验收：默认只留 plughw/default，--all 留全量。
    /// 删掉筛选或忽略 all 都会使本测试失败。
    #[test]
    fn list_reply_filters_default_and_keeps_all() {
        let enumerate = || {
            Ok(vec![
                entry("default", "Default", "capture"),
                entry("plughw:CARD=X,DEV=0", "X", "capture"),
                entry("hw:CARD=X,DEV=0", "X", "capture"),
                entry("dmix:CARD=X,DEV=0", "X", "playback"),
            ])
        };
        let ids = |body: Vec<u8>| -> Vec<String> {
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            value["data"]["devices"]
                .as_array()
                .unwrap()
                .iter()
                .map(|device| device["id"].as_str().unwrap().to_string())
                .collect()
        };
        let (body, shutdown) = list_reply(false, enumerate);
        assert!(!shutdown);
        assert_eq!(ids(body), vec!["default", "plughw:CARD=X,DEV=0"]);
        let (body, _) = list_reply(true, enumerate);
        assert_eq!(ids(body).len(), 4);
    }

    #[test]
    fn list_reply_propagates_enumerate_error() {
        let enumerate = || Err((ctl::CODE_INTERNAL.to_string(), "boom".to_string()));
        let (body, shutdown) = list_reply(false, enumerate);
        assert!(!shutdown);
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["status"], "error");
        assert_eq!(value["reason"], "boom");
    }
}
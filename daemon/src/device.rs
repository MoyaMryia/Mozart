//! 音频设备枚举（SPEC-04）：ALSA `snd_device_name_hint` 按需查询，不缓存
//! （USB 插拔）。pre 只打开配置的设备，不做枚举。

use alsa::device_name::HintIter;
use alsa::Direction;
use mozart_core::ctl;
use mozart_core::ctl::schema::DeviceEntry;

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
}
//! ALSA 采集硬件混音器封装（SPEC-02）：按声卡控制端读取/调节硬件采集增益与 AGC 开关。
//! 绝不进入实时音频线程；只在启动探测、热切采集设备成功或响应控制命令时按需开关 Mixer。

use alsa::mixer::{Mixer, Selem, SelemChannelId};
use alsa::pcm::PCM;
use mozart_core::ctl::schema::{MicMixerSetRequest, PreMicMixerStatus};

const CAPTURE_SELEM_PRIORITY: &[&str] = &[
    "Mic",
    "Capture",
    "Internal Mic",
    "Headset Mic",
    "Digital",
];

fn extract_card_token(device: &str) -> Option<String> {
    if let Some(pos) = device.find("CARD=") {
        let rest = &device[pos + 5..];
        let name: String = rest
            .chars()
            .take_while(|&ch| ch != ',' && ch != ':' && ch != '\0')
            .collect();
        if !name.is_empty() {
            return Some(format!("CARD={name}"));
        }
    }
    for prefix in ["hw:", "plughw:"] {
        if let Some(rest) = device.strip_prefix(prefix) {
            let head = rest.split(',').next().unwrap_or("");
            if !head.is_empty() && head.chars().all(|ch| ch.is_ascii_digit()) {
                return Some(head.to_string());
            }
        }
    }
    None
}

/// 从已打开的 PCM 句柄或配置设备名推导 ALSA Mixer 控制入口（`hw:<id>` / `hw:CARD=<name>`）。
/// 虚拟设备（如 `null`）无物理声卡时返回 `None`。
pub fn card_ctl_from_device(pcm: Option<&PCM>, device: &str) -> Option<(String, String)> {
    if let Some(pcm) = pcm {
        if let Ok(info) = pcm.info() {
            let card = info.get_card();
            if card >= 0 {
                let label = info
                    .get_name()
                    .ok()
                    .map(str::to_string)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("card {card}"));
                return Some((format!("hw:{card}"), label));
            }
        }
    }
    extract_card_token(device).map(|tok| {
        let label = tok.strip_prefix("CARD=").unwrap_or(&tok).to_string();
        (format!("hw:{tok}"), label)
    })
}

pub fn linear_gain(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

fn find_capture_volume_selem<'a>(mixer: &'a Mixer) -> Option<Selem<'a>> {
    let mut best: Option<(usize, Selem<'a>)> = None;
    for elem in mixer.iter() {
        let Some(selem) = Selem::new(elem) else {
            continue;
        };
        if !selem.has_capture_volume() {
            continue;
        }
        let id = selem.get_id();
        let name = id.get_name().unwrap_or("");
        let rank = CAPTURE_SELEM_PRIORITY
            .iter()
            .position(|&candidate| candidate.eq_ignore_ascii_case(name))
            .unwrap_or(CAPTURE_SELEM_PRIORITY.len());
        match &best {
            Some((best_rank, _)) if *best_rank <= rank => {}
            _ => best = Some((rank, selem)),
        }
    }
    best.map(|(_, s)| s)
}

fn find_agc_selem<'a>(mixer: &'a Mixer) -> Option<Selem<'a>> {
    for elem in mixer.iter() {
        let Some(selem) = Selem::new(elem) else {
            continue;
        };
        if selem
            .get_id()
            .get_name()
            .is_ok_and(|name| name.eq_ignore_ascii_case("Auto Gain Control"))
        {
            return Some(selem);
        }
    }
    None
}

fn capture_channel(selem: &Selem<'_>) -> SelemChannelId {
    for ch in [
        SelemChannelId::FrontLeft,
        SelemChannelId::FrontRight,
        SelemChannelId::Unknown,
    ] {
        if selem.has_capture_channel(ch) {
            return ch;
        }
    }
    SelemChannelId::FrontLeft
}

fn read_agc(selem: &Selem<'_>) -> (bool, Option<bool>, Option<String>) {
    if selem.has_capture_switch() {
        let ch = capture_channel(selem);
        return match selem.get_capture_switch(ch) {
            Ok(v) => (true, Some(v != 0), None),
            Err(err) => (true, None, Some(format!("read capture AGC: {err}"))),
        };
    }
    if selem.has_playback_switch() {
        return match selem.get_playback_switch(SelemChannelId::FrontLeft) {
            Ok(v) => (true, Some(v != 0), None),
            Err(err) => (true, None, Some(format!("read capture AGC: {err}"))),
        };
    }
    (false, None, None)
}

fn set_error_if_empty(target: &mut Option<String>, msg: String) {
    if target.is_none() {
        *target = Some(msg);
    }
}

fn read_status_from_mixer(mixer: &Mixer, card_label: Option<&str>) -> PreMicMixerStatus {
    let mut status = PreMicMixerStatus {
        card_name: card_label.map(str::to_string),
        ..Default::default()
    };
    if let Some(selem) = find_capture_volume_selem(mixer) {
        let (min, max) = selem.get_capture_volume_range();
        let ch = capture_channel(&selem);
        if max > min {
            match selem.get_capture_volume(ch) {
                Ok(raw) => {
                    let pct = (((raw - min) * 100 + (max - min) / 2) / (max - min)).clamp(0, 100);
                    status.supported = true;
                    status.volume_percent = Some(pct as u8);
                }
                Err(err) => set_error_if_empty(&mut status.error, format!("read capture volume: {err}")),
            }
        }
        let (min_db, max_db) = selem.get_capture_db_range();
        if max_db.0 >= min_db.0 && (min_db.0 != 0 || max_db.0 != 0) {
            match selem.get_capture_vol_db(ch) {
                Ok(cur_db) => {
                    status.min_db = Some(min_db.to_db());
                    status.max_db = Some(max_db.to_db());
                    status.gain_db = Some(cur_db.to_db());
                }
                Err(err) => set_error_if_empty(&mut status.error, format!("read capture dB: {err}")),
            }
        }
    }
    if let Some(agc) = find_agc_selem(mixer) {
        let (supported, enabled, err) = read_agc(&agc);
        status.agc_supported = supported;
        status.agc_enabled = enabled;
        if let Some(msg) = err {
            set_error_if_empty(&mut status.error, msg);
        }
    }
    status
}

/// 读取采集卡混音器快照；无物理声卡时返回 `supported = false`。
pub fn probe(ctl_name: Option<&str>, card_label: Option<&str>) -> PreMicMixerStatus {
    let Some(ctl) = ctl_name else {
        return PreMicMixerStatus::default();
    };
    match Mixer::new(ctl, false) {
        Ok(mixer) => read_status_from_mixer(&mixer, card_label),
        Err(err) => PreMicMixerStatus {
            supported: false,
            card_name: card_label.map(str::to_string),
            error: Some(format!("mixer open ({ctl}): {err}")),
            ..Default::default()
        },
    }
}

/// 调节当前采集卡硬件增益与 AGC 开关，并返回更新后快照（调用方须先校验请求合法性）。
pub fn apply(
    ctl_name: &str,
    card_label: Option<&str>,
    request: &MicMixerSetRequest,
) -> Result<PreMicMixerStatus, String> {
    let mixer = Mixer::new(ctl_name, false).map_err(|err| format!("mixer open ({ctl_name}): {err}"))?;
    if let Some(pct) = request.volume_percent {
        let selem = find_capture_volume_selem(&mixer)
            .ok_or_else(|| format!("capture device {ctl_name} has no hardware capture volume"))?;
        let (min, max) = selem.get_capture_volume_range();
        if max <= min {
            return Err(format!("capture device {ctl_name} has fixed capture volume"));
        }
        let target = min + ((max - min) * i64::from(pct) + 50) / 100;
        selem
            .set_capture_volume_all(target)
            .map_err(|err| format!("set capture volume: {err}"))?;
    }
    if let Some(agc_on) = request.agc_enabled {
        let selem = find_agc_selem(&mixer)
            .ok_or_else(|| format!("capture device {ctl_name} has no hardware AGC switch"))?;
        let val = i32::from(agc_on);
        if selem.has_capture_switch() {
            selem
                .set_capture_switch_all(val)
                .map_err(|err| format!("set capture AGC: {err}"))?;
        } else if selem.has_playback_switch() {
            selem
                .set_playback_switch_all(val)
                .map_err(|err| format!("set capture AGC: {err}"))?;
        } else {
            return Err(format!("capture device {ctl_name} AGC switch is read-only"));
        }
    }
    Ok(read_status_from_mixer(&mixer, card_label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_ctl_parses_alsa_card_names_and_skips_virtual() {
        assert_eq!(
            card_ctl_from_device(None, "plughw:CARD=Audio,DEV=0"),
            Some(("hw:CARD=Audio".to_string(), "Audio".to_string()))
        );
        assert_eq!(
            card_ctl_from_device(None, "hw:3,0"),
            Some(("hw:3".to_string(), "3".to_string()))
        );
        assert_eq!(card_ctl_from_device(None, "null"), None);
        assert_eq!(card_ctl_from_device(None, "default"), None);
    }

    #[test]
    fn probe_none_reports_unsupported_and_linear_gain_scales_db() {
        assert!(!probe(None, None).supported);
        assert!((linear_gain(0.0) - 1.0).abs() < 1e-6);
        assert!((linear_gain(-6.0) - 0.501187).abs() < 1e-4);
        assert!((linear_gain(6.0) - 1.995262).abs() < 1e-4);
    }
}

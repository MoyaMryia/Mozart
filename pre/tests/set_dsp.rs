//! 进程级集成测试：SetDsp 运行中切换 DSP 级（SPEC-02）。
//! 文件采集源注入「低噪声 + 间歇正弦」信号，消费者支路收 InputFrame：
//! denoise 启用时突发段置信度非零；SetDsp 禁用后置信度恒 0（SPEC-02 口径）；
//! 重新启用后 WOLA 状态重置、置信度恢复。验证命令 → 目标原子 → 处理线程
//! 应用的端到端链路，切换期间帧流不停。

mod common;
use common::*;

use mozart_core::ctl::{self, Seqpacket};
use mozart_core::dgram::FrameSocket;
use mozart_core::InputFrame;
use std::path::Path;
use std::time::Duration;

/// 测试采集文件（1 s 循环，48 kHz 单声道 PCM16）：
/// 前 0.5 s 仅低噪声（NS 初始化攒噪声底），0.5–0.7 s 叠加 440 Hz 正弦
/// （post-SNR 高 → 置信度非零），其余仅低噪声。
fn write_capture_wav(path: &Path) {
    let mut samples = Vec::with_capacity(48_000);
    let mut x = 0x4D5A_5254u32 | 1;
    for i in 0..48_000u32 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let noise = ((x >> 9) as f32 / 8_388_608.0 - 0.5) * 0.02;
        let burst = (0.5..0.7).contains(&(i as f32 / 48_000.0));
        let sine = if burst {
            0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin()
        } else {
            0.0
        };
        samples.push(((noise + sine) * 32_767.0) as i16);
    }

    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes()); // PCM
    fmt.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    fmt.extend_from_slice(&48_000u32.to_le_bytes());
    fmt.extend_from_slice(&96_000u32.to_le_bytes()); // byte rate
    fmt.extend_from_slice(&2u16.to_le_bytes()); // block align
    fmt.extend_from_slice(&16u16.to_le_bytes());

    let mut body = Vec::new();
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    body.extend_from_slice(&fmt);
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(&data);

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(&body);
    std::fs::write(path, wav).unwrap();
}

/// 收一帧取置信度字节；帧流必须持续（20 ms 节拍）。
fn recv_conf(rx: &FrameSocket<InputFrame>) -> u8 {
    let frame = rx
        .recv(Some(Duration::from_secs(2)))
        .expect("消费者支路必须持续送达");
    frame.meta.conf
}

#[test]
fn set_dsp_toggles_denoise_at_runtime() {
    let dir = run_dir("set-dsp");
    let capture_path = dir.join("capture.wav");
    write_capture_wav(&capture_path);
    let config = format!(
        "[daemon]\nrun_dir = \"{}\"\n[pre]\ncapture_device = \"null\"\nplayback_device = \"null\"\ncapture_file = \"{}\"\n",
        dir.display(),
        capture_path.display()
    );
    // 默认消费者列表的 asr 支路作接收端：先绑定，pre 惰性建链直接接通。
    let rx = FrameSocket::<InputFrame>::bind(&dir.join("asr-in.sock"), 8).unwrap();
    let mut child = spawn_pre(&dir, &config);
    wait_sockets(&dir);
    let control = Seqpacket::connect(&dir.join("pre.sock")).unwrap();

    // 默认 denoise 启用：突发段置信度非零。
    wait_until("denoise 启用置信度非零", Duration::from_secs(5), || {
        recv_conf(&rx) > 0
    });

    // 运行中禁用：应答回显开关。
    let data = call_ok(
        &control,
        ctl::PRE_SET_DSP,
        2,
        br#"{"denoise":false,"gate":false,"agc":false,"limiter":false}"#,
    );
    assert_eq!(data["denoise"], false);
    assert_eq!(data["gate"], false);
    assert_eq!(data["agc"], false);
    assert_eq!(data["limiter"], false);

    // 切换在一帧内应用：先排空 3 帧过渡，再验证完整循环周期（60 帧 = 1.2 s，
    // 覆盖至少一次正弦突发）置信度恒 0。denoise 未真正禁用则突发段必非零。
    for _ in 0..3 {
        recv_conf(&rx);
    }
    for _ in 0..60 {
        assert_eq!(recv_conf(&rx), 0, "禁用 denoise 后置信度必须恒 0");
    }

    // 运行中重新启用：WOLA 状态重置后重新攒初始化帧，置信度恢复非零。
    let data = call_ok(
        &control,
        ctl::PRE_SET_DSP,
        3,
        br#"{"denoise":true,"gate":false,"agc":false,"limiter":false}"#,
    );
    assert_eq!(data["denoise"], true);
    wait_until("denoise 重新启用置信度恢复", Duration::from_secs(5), || {
        recv_conf(&rx) > 0
    });

    // 状态快照：默认消费者列表的键齐全，链路仍在跑。
    let status = get_status(&control);
    let drops = &status["consumer_drops"];
    for key in ["rvc", "rvc.skipped", "asr", "tts"] {
        assert!(drops.get(key).is_some(), "consumer_drops 缺键 {key}：{drops}");
    }
    assert!(status["frames_captured"].as_u64().unwrap() > 0);

    sigterm(&child.0);
    assert!(wait_exit(&mut child.0, Duration::from_secs(2)).success());
    let _ = std::fs::remove_dir_all(&dir);
}
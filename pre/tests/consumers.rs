//! 进程级集成测试：消费者列表配置化（SPEC-02 分发）。
//! 自定义 [[pre.consumers]] 声明额外支路：配置 → 发送端接线 → 帧送达 →
//! GetStatus consumer_drops 动态键的端到端链路；file_mode_affected=false
//! 的支路在文件接管期间恒发。

mod common;
use common::*;

use mozart_core::ctl::{self, Seqpacket};
use mozart_core::dgram::FrameSocket;
use mozart_core::InputFrame;
use std::time::Duration;

#[test]
fn configured_consumer_receives_frames_and_reports_drops() {
    let dir = run_dir("consumers");
    let config = format!(
        "[daemon]\nrun_dir = \"{}\"\n[pre]\ncapture_device = \"null\"\nplayback_device = \"null\"\n\
         [[pre.consumers]]\nname = \"rvc\"\nsocket = \"rvc-in.sock\"\nfile_mode_affected = true\n\
         [[pre.consumers]]\nname = \"extra\"\nsocket = \"extra-in.sock\"\n",
        dir.display()
    );
    // 自定义支路接收端：先绑定，pre 惰性建链直接接通。
    let rx = FrameSocket::<InputFrame>::bind(&dir.join("extra-in.sock"), 8).unwrap();
    let mut child = spawn_pre(&dir, &config);
    wait_sockets(&dir);
    let control = Seqpacket::connect(&dir.join("pre.sock")).unwrap();

    // 自定义消费者收到契约帧（null 设备采集恒跑）。
    let frame = rx
        .recv(Some(Duration::from_secs(5)))
        .expect("自定义消费者支路必须送达");
    assert!(frame.meta.frame_idx > 0, "打包序号从 1 起");

    // consumer_drops 键随配置列表：有 extra/rvc，无默认列表的 asr。
    // null 采集无实时节拍，发送快于本用例收帧时 extra 计丢弃属正常，
    // 只断言键存在与计数可读。
    let status = get_status(&control);
    let drops = &status["consumer_drops"];
    assert!(drops.get("extra").is_some(), "缺自定义消费者键：{drops}");
    assert!(drops.get("rvc").is_some(), "缺 rvc 键：{drops}");
    assert!(drops.get("rvc.skipped").is_some(), "rvc 口径子键保留：{drops}");
    assert!(drops.get("asr").is_none(), "未配置的消费者不得出现：{drops}");
    assert!(drops["extra"].as_u64().is_some(), "extra 计数必须可读：{drops}");

    // 文件接管：file_mode_affected 缺省 false 的 extra 支路恒发。
    let data = call_ok(&control, ctl::PRE_SET_FILE_MODE, 2, br#"{"active":true}"#);
    assert_eq!(data["active"], true);
    for _ in 0..3 {
        rx.recv(Some(Duration::from_secs(2)))
            .expect("文件接管期间 extra 支路必须恒发");
    }

    sigterm(&child.0);
    assert!(wait_exit(&mut child.0, Duration::from_secs(2)).success());
    let _ = std::fs::remove_dir_all(&dir);
}
//! 生成帧字节共享夹具到 tests/fixtures/frame/，C++ 与 Rust 测试读同一组
//! 字节对拍（SPEC-01）。再生命令：
//!   cargo run -p mozart-core --example gen_frame_fixture
//! 夹具确定性：固定 meta、递增 PCM（输入 i×0.5、输出 i×0.25，f32 精确表示，
//! 不依赖 libm 与随机数）、DAEMON_HELLO 请求信封（体 {}）。
//! 消费方：core/tests/frame_fixture.rs、tests/test_rvc_transport.cpp、
//! tests/test_control_contract.cpp。

use mozart_core::ctl::{self, Header};
use mozart_core::dgram::WirePacket;
use mozart_core::{FrameMeta, InputFrame, OutputFrame, INPUT_SAMPLES, OUTPUT_SAMPLES};
use std::fs;
use std::path::Path;

/// 夹具目录：仓库根 tests/fixtures/frame/（core/ 上一级）。
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/frame");

/// 金标 meta：与两侧硬编码字节断言同值（dgram.rs / test_rvc_transport.cpp）。
fn golden_meta() -> FrameMeta {
    FrameMeta {
        pts_ns: 0x8877_6655_4433_2211,
        frame_idx: 0xD4D3_D2D1,
        vad_flag: 1,
        energy_db: 2,
        conf: 3,
        segment_id: 4,
    }
}

fn golden_input() -> InputFrame {
    let mut pcm = [0f32; INPUT_SAMPLES];
    for (i, slot) in pcm.iter_mut().enumerate() {
        *slot = i as f32 * 0.5;
    }
    InputFrame {
        meta: golden_meta(),
        pcm,
    }
}

fn golden_output() -> OutputFrame {
    let mut pcm = [0f32; OUTPUT_SAMPLES];
    for (i, slot) in pcm.iter_mut().enumerate() {
        *slot = i as f32 * 0.25;
    }
    OutputFrame::new_48k(golden_meta(), pcm)
}

fn main() {
    let dir = Path::new(FIXTURE_DIR);
    fs::create_dir_all(dir).expect("创建夹具目录失败");

    let mut input = [0u8; <InputFrame as WirePacket>::PACKET_BYTES];
    golden_input().encode(&mut input);
    fs::write(dir.join("input.bin"), input).expect("写 input.bin 失败");

    let mut output = [0u8; <OutputFrame as WirePacket>::PACKET_BYTES];
    golden_output().encode(&mut output);
    fs::write(dir.join("output.bin"), output).expect("写 output.bin 失败");

    // Hello 请求：与 ctl CLI 实际发送一致（体 {}），request_id 取已知金标值。
    let body = b"{}";
    let header = Header::new(ctl::DAEMON_HELLO, 0x0102_0304, body.len() as u32);
    let mut envelope = header.to_bytes().to_vec();
    envelope.extend_from_slice(body);
    fs::write(dir.join("ctl_hello.bin"), &envelope).expect("写 ctl_hello.bin 失败");

    println!(
        "mozart-gen-frame-fixture: {} (input {} B, output {} B, ctl_hello {} B)",
        dir.display(),
        input.len(),
        output.len(),
        envelope.len()
    );
}
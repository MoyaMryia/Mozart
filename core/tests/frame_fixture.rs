//! 帧字节共享夹具对拍（SPEC-01）：读 tests/fixtures/frame/ 的二进制夹具，
//! 断言 Rust 序列化与夹具字节逐字节一致、解码还原已知值。C++ 侧读同一组
//! 文件（tests/test_rvc_transport.cpp、tests/test_control_contract.cpp）。
//! 夹具再生命令：cargo run -p mozart-core --example gen_frame_fixture

use mozart_core::ctl::{self, Header, HEADER_BYTES};
use mozart_core::dgram::WirePacket;
use mozart_core::{FrameMeta, InputFrame, OutputFrame, INPUT_SAMPLES, OUTPUT_SAMPLES};
use std::fs;

/// 夹具目录：仓库根 tests/fixtures/frame/（core/ 上一级）。
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/frame");

fn read_fixture(name: &str) -> Vec<u8> {
    fs::read(format!("{FIXTURE_DIR}/{name}"))
        .unwrap_or_else(|err| panic!("读夹具 {name} 失败：{err}（先运行 gen_frame_fixture）"))
}

/// 金标 meta：与生成器及两侧硬编码字节断言同值。
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

/// meta 比较按值传参；packed 字段只能经解构按值读取，不能取引用。
fn assert_same_meta(actual: FrameMeta, expect: FrameMeta) {
    let FrameMeta {
        pts_ns,
        frame_idx,
        vad_flag,
        energy_db,
        conf,
        segment_id,
    } = actual;
    let FrameMeta {
        pts_ns: expect_pts_ns,
        frame_idx: expect_frame_idx,
        vad_flag: expect_vad_flag,
        energy_db: expect_energy_db,
        conf: expect_conf,
        segment_id: expect_segment_id,
    } = expect;
    assert_eq!(pts_ns, expect_pts_ns);
    assert_eq!(frame_idx, expect_frame_idx);
    assert_eq!(vad_flag, expect_vad_flag);
    assert_eq!(energy_db, expect_energy_db);
    assert_eq!(conf, expect_conf);
    assert_eq!(segment_id, expect_segment_id);
}

#[test]
fn input_fixture_matches_rust_serialization() {
    let bytes = read_fixture("input.bin");
    let mut pcm = [0f32; INPUT_SAMPLES];
    for (i, slot) in pcm.iter_mut().enumerate() {
        *slot = i as f32 * 0.5;
    }
    let expect = InputFrame {
        meta: golden_meta(),
        pcm,
    };
    let mut packet = [0u8; <InputFrame as WirePacket>::PACKET_BYTES];
    expect.encode(&mut packet);
    assert_eq!(bytes.len(), 1300, "input.bin 必须是 1300 B 输入线包");
    assert_eq!(&bytes[..], &packet[..], "input.bin 与 Rust 序列化不一致");

    // 解码方向：夹具字节解出的帧与已知值一致。
    let decoded = <InputFrame as WirePacket>::decode(&bytes);
    assert_same_meta(decoded.meta, expect.meta);
    let decoded_pcm = decoded.pcm;
    assert_eq!(decoded_pcm, pcm);
}

#[test]
fn output_fixture_matches_rust_serialization() {
    let bytes = read_fixture("output.bin");
    let mut pcm = [0f32; OUTPUT_SAMPLES];
    for (i, slot) in pcm.iter_mut().enumerate() {
        *slot = i as f32 * 0.25;
    }
    let expect = OutputFrame::new_48k(golden_meta(), pcm);
    let mut packet = [0u8; <OutputFrame as WirePacket>::PACKET_BYTES];
    expect.encode(&mut packet);
    assert_eq!(bytes.len(), 3868, "output.bin 必须是 3868 B 输出线包");
    assert_eq!(&bytes[..], &packet[..], "output.bin 与 Rust 序列化不一致");

    let decoded = <OutputFrame as WirePacket>::decode(&bytes);
    assert_same_meta(decoded.meta, expect.meta);
    let decoded_pcm = decoded.pcm;
    assert_eq!(decoded_pcm, pcm);
}

#[test]
fn ctl_hello_fixture_matches_rust_serialization() {
    let bytes = read_fixture("ctl_hello.bin");
    // Hello 请求：DAEMON_HELLO、已知 request_id、体 {}（与 ctl CLI 实际发送一致）。
    let body = b"{}";
    let expect = Header::new(ctl::DAEMON_HELLO, 0x0102_0304, body.len() as u32);
    let mut packet = expect.to_bytes().to_vec();
    packet.extend_from_slice(body);
    assert_eq!(bytes, packet, "ctl_hello.bin 与 Rust 序列化不一致");

    // 解码方向：头字段与体长度还原一致。
    let decoded = Header::from_bytes(&bytes)
        .expect("夹具头解码失败")
        .ensure_version()
        .expect("夹具版本不符");
    assert_eq!(decoded, expect);
    assert_eq!(decoded.body_len as usize, bytes.len() - HEADER_BYTES);
    assert_eq!(&bytes[HEADER_BYTES..], body);
}
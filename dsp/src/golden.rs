//! 旧 C 核心对拍夹具测试。夹具由 Task 3/4 的一次性 harness 用
//! Mozart-Old/preprocessor 的 C 核心离线生成（来源、编译与再生命令见
//! Task 3/4 报告），文件位于 `tests/fixtures/dsp/`，`.gitignore` 白名单管理。
//! 内容：1 s / 48 kHz 单声道 f32 小端——纯噪声 0–0.2 s、双音 0.2–0.6 s、
//! 纯噪声 0.6–0.8 s、单音 0.8–1.0 s（噪声用固定种子 xorshift32 合成）。
//! Task 4 新增：整形级发射流（gate/agc/limiter 单开与全开；input.f32 电平低于
//! -1 dB 上限，limiter 冷参考与 wola 同流，另有 ×4 热输入参考 limiter_hot.f32
//! 专拍介入态）、16 kHz 打包 PCM（dec.f32，逐位对拍）与每帧 meta（meta.txt）。

use super::highpass::Highpass80;
use super::noise_suppressor::NoiseSuppressor;
use super::Dsp;
use mozart_core::{FrameMeta, RawFrame, RAW_SAMPLES};
use std::fs;

/// WOLA 容差（最大绝对差）。依据：Rust 与旧 C 同算法、同运算顺序、同 f64
/// 内部精度，差异只剩跨平台 libm cos/sin 的 1~2 ulp（夹具在 x86 生成，测试
/// 还要在 aarch64 Jetson 上跑），经 FFT 旋转因子递推放大后输出相对差 ~1e-13，
/// f32 落盘只在跨舍入边界的稀疏样本上翻转 1 个 f32 ulp（|x|≤1 时 ≤1.2e-7）。
/// 门限 1e-6 = 单 ulp 上界的约 8 倍，也是旧仓 C↔Python 对拍门限先例
/// （旧 tests/test_noise_suppressor.c：实测残差 3.7e-9、留三个数量级余量）；
/// 参数或流程错配（如 lookahead 漂移）会显著越限，不会混过关。
const MAX_ABS_DIFF: f64 = 1e-6;
/// WOLA 容差（RMS）。依据：稀疏单 ulp 翻移的 RMS 量级 ~1e-10；1e-8 用于排除
/// 系统性漂移——真实算法错配的 RMS 会大几个数量级。
const MAX_RMS_DIFF: f64 = 1e-8;

/// 能量字节容差 ±1：energy_db 由 log10f 量化到 0..255，跨平台 libm 的 1 ulp
/// 差可能跨过截断边界；frame_idx/vad_flag/conf/segment_id 为整数量化前的
/// f32 比较（阈值远离夹具信号的实际取值），按位（整数）断言。
const ENERGY_TOLERANCE: u8 = 1;

/// 夹具目录：仓库根 tests/fixtures/dsp/（dsp/ 上一级）。
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/dsp");

fn read_f32(name: &str) -> Vec<f32> {
    let bytes = fs::read(format!("{FIXTURE_DIR}/{name}")).expect("夹具读取失败");
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

fn read_counts() -> Vec<usize> {
    fs::read_to_string(format!("{FIXTURE_DIR}/counts.txt"))
        .expect("counts.txt 读取失败")
        .lines()
        .map(|line| line.trim().parse().expect("counts.txt 含非整数行"))
        .collect()
}

/// 逐样本比对并断言容差；返回 (最大绝对差, RMS 差)。
fn assert_close(actual: &[f32], expected: &[f32], context: &str) -> (f64, f64) {
    assert_eq!(actual.len(), expected.len(), "{context}：流长度不一致");
    let mut max_diff = 0.0f64;
    let mut sq_sum = 0.0f64;
    let mut worst = 0usize;
    for (i, (a, b)) in actual.iter().zip(expected.iter()).enumerate() {
        let diff = (*a as f64 - *b as f64).abs();
        if diff > max_diff {
            max_diff = diff;
            worst = i;
        }
        sq_sum += diff * diff;
    }
    let rms = (sq_sum / actual.len() as f64).sqrt();
    assert!(
        max_diff < MAX_ABS_DIFF,
        "{context}：最大绝对差 {max_diff:.3e}（样本 {worst}）超出容差 {MAX_ABS_DIFF:.0e}"
    );
    assert!(
        rms < MAX_RMS_DIFF,
        "{context}：RMS 差 {rms:.3e} 超出容差 {MAX_RMS_DIFF:.0e}"
    );
    (max_diff, rms)
}

/// 960 定块喂入（与主干消费方式一致），返回发射流。
fn run_ns_blocks(input: &[f32]) -> Vec<f32> {
    let mut ns = NoiseSuppressor::new();
    let mut stream = Vec::new();
    for block in input.chunks(RAW_SAMPLES) {
        let mut out = [0f32; RAW_SAMPLES];
        let written = ns.push(block, &mut out);
        stream.extend_from_slice(&out[..written]);
    }
    stream
}

/// 全链 960 定块喂入，按旧 C 每块发射数（counts.txt）截取发射样本。
/// 整形级按参数开闭；返回发射流与逐帧置信度。
fn run_dsp_chain(
    input: &[f32],
    counts: &[usize],
    gate: bool,
    agc: bool,
    limiter: bool,
) -> (Vec<f32>, Vec<f32>) {
    let mut dsp = Dsp::new(true, gate, agc, limiter);
    let mut stream = Vec::new();
    let mut confs = Vec::new();
    for (block, count) in input.chunks(RAW_SAMPLES).zip(counts) {
        // packed 字段按值整块搬入搬出，不取引用（同 playback.rs 的约定）
        let mut pcm = [0f32; RAW_SAMPLES];
        pcm.copy_from_slice(block);
        let mut frame = RawFrame {
            meta: FrameMeta {
                pts_ns: 0,
                frame_idx: 0,
                vad_flag: 0,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            pcm,
        };
        confs.push(dsp.process_frame(&mut frame));
        // counts.txt 的每块发射数与 wola 流同源；越界即 panic，不静默截断。
        let count = *count;
        // packed 字段按值搬出再切片，不取引用。
        let pcm = frame.pcm;
        stream.extend_from_slice(&pcm[..count]);
    }
    (stream, confs)
}

#[test]
fn highpass_is_bitwise_identical_to_c() {
    // biquad 高通无 FFT、无跨平台 libm 依赖：f32 同序运算要求逐位一致
    let mut actual = read_f32("input.f32");
    let expected = read_f32("hp.f32");
    let mut hp = Highpass80::new();
    hp.process(&mut actual);
    assert_eq!(actual.len(), expected.len());
    for (i, (a, b)) in actual.iter().zip(expected.iter()).enumerate() {
        assert_eq!(a.to_bits(), b.to_bits(), "样本 {i}：高通必须与旧 C 逐位一致");
    }
}

#[test]
fn wola_stream_matches_c_within_tolerance() {
    let expected = read_f32("wola.f32");
    let actual = run_ns_blocks(&read_f32("hp.f32"));
    let (max_diff, rms) = assert_close(&actual, &expected, "WOLA 960 定块对拍");
    // 夹具 47936 样本（1 s 输入 - 64 发射滞后），记录实测供报告核对
    eprintln!("wola_stream_matches_c: max={max_diff:.3e} rms={rms:.3e} n={}", actual.len());
}

#[test]
fn wola_streaming_chunks_match_c_within_tolerance() {
    // 流式合同：任意分块与 960 定块逐位一致，且都在容差内命中旧 C 参考
    let input = read_f32("hp.f32");
    let block_stream = run_ns_blocks(&input);
    let mut ns = NoiseSuppressor::new();
    let mut chunked = Vec::with_capacity(input.len());
    let chunks = [1usize, 7, 64, 128, 333, 960, 5, 4096];
    let mut pos = 0usize;
    let mut ci = 0usize;
    while pos < input.len() {
        let take = chunks[ci % chunks.len()].min(input.len() - pos);
        let mut sink = [0f32; 8192];
        let written = ns.push(&input[pos..pos + take], &mut sink);
        chunked.extend_from_slice(&sink[..written]);
        pos += take;
        ci += 1;
    }
    assert_eq!(chunked, block_stream, "任意分块必须与定块逐位一致");
    assert_close(&chunked, &read_f32("wola.f32"), "WOLA 变长分块对拍");
}

#[test]
fn dsp_frame_stream_matches_c_wrapper_reference() {
    // 全链：Dsp::process_frame 逐帧处理 input.f32；期望流按旧 C 的每块发射数
    // （counts.txt）拼接 wola.f32 并补零到 960——即旧 mozart_dsp_process 的
    // wrapper 语义（不足 960 补零保持节拍）。
    let input = read_f32("input.f32");
    let wola = read_f32("wola.f32");
    let counts = read_counts();
    let mut dsp = Dsp::new(true, false, false, false);

    let mut base = 0usize;
    let mut confs = Vec::new();
    for (frame_no, block) in input.chunks(RAW_SAMPLES).enumerate() {
        let count = counts[frame_no];
        // packed 字段按值整块搬入搬出，不取引用（同 playback.rs 的约定）
        let mut pcm = [0f32; RAW_SAMPLES];
        pcm.copy_from_slice(block);
        let mut frame = RawFrame {
            meta: FrameMeta {
                pts_ns: 0,
                frame_idx: frame_no as u32,
                vad_flag: 0,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            pcm,
        };
        confs.push(dsp.process_frame(&mut frame));
        let actual = frame.pcm;

        let mut expected = [0f32; RAW_SAMPLES];
        expected[..count].copy_from_slice(&wola[base..base + count]);
        assert_close(&actual[..count], &expected[..count], "全链帧 {frame_no}");
        assert_eq!(actual[count..], expected[count..], "帧 {frame_no} 补零段必须恒为零");
        base += count;
    }
    assert_eq!(base, wola.len(), "发射总数应与 wola 流一致");

    // 统计特性：有声段（0.2–0.6 s = 帧 10–29）置信度必须显著高于纯噪声段
    //（0.6–0.8 s = 帧 30–39）。两段来自同一参考验证过的流，非自我比较。
    let tone_mean: f64 = confs[10..30].iter().map(|c| *c as f64).sum::<f64>() / 20.0;
    let noise_mean: f64 = confs[30..40].iter().map(|c| *c as f64).sum::<f64>() / 10.0;
    assert!(
        tone_mean > 0.5 && noise_mean < tone_mean - 0.2,
        "置信度未区分有声/噪声：tone={tone_mean:.3} noise={noise_mean:.3}"
    );
}

#[test]
fn gate_stream_matches_c_within_tolerance() {
    let input = read_f32("input.f32");
    let counts = read_counts();
    let (actual, _) = run_dsp_chain(&input, &counts, true, false, false);
    let (max_diff, rms) =
        assert_close(&actual, &read_f32("gate.f32"), "gate 单开对拍");
    eprintln!("gate_stream: max={max_diff:.3e} rms={rms:.3e} n={}", actual.len());
}

#[test]
fn agc_stream_matches_c_within_tolerance() {
    let input = read_f32("input.f32");
    let counts = read_counts();
    let (actual, _) = run_dsp_chain(&input, &counts, false, true, false);
    let (max_diff, rms) = assert_close(&actual, &read_f32("agc.f32"), "agc 单开对拍");
    eprintln!("agc_stream: max={max_diff:.3e} rms={rms:.3e} n={}", actual.len());
}

#[test]
fn limiter_stream_matches_c_within_tolerance() {
    // 冷输入对拍：input.f32 峰值 0.532 低于 -1 dB 上限 0.891，限幅器全程透明，
    // 该流与 wola.f32 逐位相同——本用例验证的是「夹具电平下限幅器不介入」，
    // 介入路径由下方热输入用例覆盖。
    let input = read_f32("input.f32");
    let counts = read_counts();
    let (actual, _) = run_dsp_chain(&input, &counts, false, false, true);
    let (max_diff, rms) =
        assert_close(&actual, &read_f32("limiter.f32"), "limiter 单开对拍");
    eprintln!(
        "limiter_stream: max={max_diff:.3e} rms={rms:.3e} n={}",
        actual.len()
    );
}

#[test]
fn limiter_hot_stream_engages_and_matches_c_within_tolerance() {
    // 介入态对拍：输入 ×4 驱动限幅器实际压峰。断言介入前提与结果（非自比）：
    // 不限幅链峰值显著超上限、限幅链峰值钳到上限附近且低于不限幅链。
    // 夹具 limiter_hot.f32 由同一 ×4 输入经旧 C 链生成（harness gen_pack_fixture.c）。
    let input = read_f32("input.f32");
    let counts = read_counts();
    let hot: Vec<f32> = input.iter().map(|&x| x * 4.0).collect();
    let (bypassed, _) = run_dsp_chain(&hot, &counts, false, false, false);
    let (actual, _) = run_dsp_chain(&hot, &counts, false, false, true);
    let ceiling = 10.0f64.powf(-1.0 / 20.0);
    let peak_bypassed = bypassed.iter().fold(0.0f32, |acc, &v| acc.max(v.abs())) as f64;
    let peak_limited = actual.iter().fold(0.0f32, |acc, &v| acc.max(v.abs())) as f64;
    assert!(
        peak_bypassed > ceiling * 1.2,
        "热输入不限幅链峰值 {peak_bypassed} 必须显著超上限 {ceiling}，否则夹具已退化"
    );
    assert!(
        peak_limited <= ceiling * 1.0001,
        "限幅链峰值 {peak_limited} 必须钳到上限 {ceiling}"
    );
    assert!(
        peak_limited >= ceiling * 0.99,
        "限幅链峰值 {peak_limited} 必须贴住上限（介入态），否则对拍退化为空转"
    );
    let (max_diff, rms) =
        assert_close(&actual, &read_f32("limiter_hot.f32"), "limiter 热输入对拍");
    eprintln!(
        "limiter_hot_stream: max={max_diff:.3e} rms={rms:.3e} n={} peak={peak_limited:.6}",
        actual.len()
    );
}

#[test]
fn stages_stream_matches_c_within_tolerance() {
    // 全开：同帧内 gate → AGC → limiter 级联次序（旧 enhance L117-119）。
    let input = read_f32("input.f32");
    let counts = read_counts();
    let (actual, _) = run_dsp_chain(&input, &counts, true, true, true);
    let (max_diff, rms) =
        assert_close(&actual, &read_f32("stages.f32"), "整形级全开对拍");
    eprintln!(
        "stages_stream: max={max_diff:.3e} rms={rms:.3e} n={}",
        actual.len()
    );
}

#[test]
fn pack_pcm_is_bitwise_identical_to_c() {
    // 打包链（denoise 开、整形级全关）：process_frame 对齐 + pack_frame。
    // dec.f32 为旧 wrapper + pack_frame 输出；FIR 为纯 f32 乘加无超越函数，
    // 其输入（WOLA 发射流）在两端实测逐位一致（Task 3），故断言按位相等。
    let input = read_f32("input.f32");
    let expected = read_f32("dec.f32");
    let mut dsp = Dsp::new(true, false, false, false);
    let mut base = 0usize;
    for block in input.chunks(RAW_SAMPLES) {
        let mut pcm = [0f32; RAW_SAMPLES];
        pcm.copy_from_slice(block);
        let mut frame = RawFrame {
            meta: FrameMeta {
                pts_ns: 0,
                frame_idx: 0,
                vad_flag: 0,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            pcm,
        };
        let conf = dsp.process_frame(&mut frame);
        let packed = dsp.pack_frame(&frame, conf);
        let actual = packed.pcm;
        for (i, (&a, &b)) in actual.iter().zip(expected[base..base + actual.len()].iter()).enumerate()
        {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "帧 {} 样本 {}：降采样必须与旧 C 逐位一致",
                base / mozart_core::INPUT_SAMPLES,
                i
            );
        }
        base += actual.len();
    }
    assert_eq!(base, expected.len(), "打包帧数应与 dec 流一致");
}

#[test]
fn pack_meta_matches_c() {
    // meta.txt 每行：frame_idx vad_flag energy_db conf segment_id。
    let input = read_f32("input.f32");
    let meta_lines: Vec<(u32, u8, u8, u8, u8)> = fs::read_to_string(format!(
        "{FIXTURE_DIR}/meta.txt"
    ))
    .expect("meta.txt 读取失败")
    .lines()
    .map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(
            fields.len(),
            5,
            "meta.txt 行格式：frame_idx vad_flag energy_db conf segment_id"
        );
        (
            fields[0].parse().expect("frame_idx 整数"),
            fields[1].parse().expect("vad_flag 整数"),
            fields[2].parse().expect("energy_db 整数"),
            fields[3].parse().expect("conf 整数"),
            fields[4].parse().expect("segment_id 整数"),
        )
    })
    .collect();

    let mut dsp = Dsp::new(true, false, false, false);
    let mut energy_deviations = 0u32;
    for (frame_no, block) in input.chunks(RAW_SAMPLES).enumerate() {
        let mut pcm = [0f32; RAW_SAMPLES];
        pcm.copy_from_slice(block);
        let mut frame = RawFrame {
            meta: FrameMeta {
                pts_ns: 0,
                frame_idx: 0,
                vad_flag: 0,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            pcm,
        };
        let conf = dsp.process_frame(&mut frame);
        let packed = dsp.pack_frame(&frame, conf);
        // packed 字段经解构按值读取（dgram.rs 测试同款约定）。
        let FrameMeta {
            frame_idx,
            vad_flag,
            energy_db,
            conf: got_conf,
            segment_id,
            ..
        } = packed.meta;
        let (expect_idx, expect_vad, expect_energy, expect_conf, expect_segment) =
            meta_lines[frame_no];
        assert_eq!(frame_idx, expect_idx, "帧 {frame_no}：frame_idx");
        assert_eq!(vad_flag, expect_vad, "帧 {frame_no}：vad_flag");
        assert_eq!(got_conf, expect_conf, "帧 {frame_no}：conf 字节");
        assert_eq!(segment_id, expect_segment, "帧 {frame_no}：segment_id");
        let diff = energy_db.abs_diff(expect_energy);
        assert!(
            diff <= ENERGY_TOLERANCE,
            "帧 {frame_no}：energy_db {energy_db} 与 {expect_energy} 差 {diff} 超出 ±{ENERGY_TOLERANCE}"
        );
        if diff > 0 {
            energy_deviations += 1;
        }
    }
    // frame_idx 从 1 起单调推进（旧 ++frame_idx 语义，u32 环内 1..=50）。
    assert_eq!(meta_lines.len(), input.len() / RAW_SAMPLES);
    assert_eq!(meta_lines[0].0, 1);
    eprintln!("pack_meta: energy 字节偏差帧数 {energy_deviations}/50");
}

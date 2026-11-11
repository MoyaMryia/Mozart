//! 旧 C 核心对拍夹具测试。夹具由 Task 3 的一次性 harness 用
//! Mozart-Old/preprocessor 的 C 核心离线生成（来源、编译与再生命令见
//! Task 3 报告），文件位于 `tests/fixtures/dsp/`，`.gitignore` 白名单管理。
//! 内容：1 s / 48 kHz 单声道 f32 小端——纯噪声 0–0.2 s、双音 0.2–0.6 s、
//! 纯噪声 0.6–0.8 s、单音 0.8–1.0 s（噪声用固定种子 xorshift32 合成）。

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

/// 夹具目录：仓库根 tests/fixtures/dsp/（pre/ 上一级）。
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
    let mut dsp = Dsp::new(true);

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

//! 文件解码：ffmpeg 解出裸 f32（plain 16 kHz / denoise 48 kHz），校验时长上限，
//! 再打包成 16 kHz 契约帧。denoise 走与 pre 同一套 mozart-dsp（SPEC-02/04），
//! vad_flag==0 的帧补零（沿用旧 file_denoise 语义）。

use super::{ffmpeg, StepError};
use mozart_core::config::Config;
use mozart_core::{
    FrameMeta, RawFrame, FRAME_MS, INPUT_SAMPLES, INPUT_SAMPLE_RATE, RAW_SAMPLES, RAW_SAMPLE_RATE,
};
use mozart_dsp::Dsp;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::time::Duration;

/// decode 阶段内进度切分：ffmpeg 解码占前半，逐块打包占后半。
const FFMPEG_PERCENT: u8 = 50;

/// 一帧的时间轴步长（契约帧长 20 ms），取 core 的 FRAME_MS。
const FRAME_NS: u64 = FRAME_MS as u64 * 1_000_000;

/// 16 kHz 契约帧内容。线包 meta 的序号/时间戳由喂帧阶段统一填。
#[derive(Debug)]
pub(super) struct ContractFrame {
    pub pcm: [f32; INPUT_SAMPLES],
    /// plain 恒 1；denoise 取 DSP 的 VAD 判定。
    pub vad_flag: u8,
}

/// 解码源文件并打包为契约帧。`dest` 是裸流临时文件，调用方负责清理。
/// `percent` 报告 decode 阶段内进度（0..100），`cancel` 是取消检查点。
pub(super) fn decode(
    config: &Config,
    source: &Path,
    dest: &Path,
    cancel: &dyn Fn() -> bool,
    percent: &mut dyn FnMut(u8),
) -> Result<Vec<ContractFrame>, StepError> {
    let denoise = config.file.denoise;
    let sample_rate = if denoise {
        RAW_SAMPLE_RATE
    } else {
        INPUT_SAMPLE_RATE
    };
    // 时长上限只在此处换算成字节上限：裸流字节数 / (采样率 × 4) 即时长。
    let max_bytes = config.file.max_duration_seconds * u64::from(sample_rate) * 4;
    let timeout = Duration::from_millis(config.file.ffmpeg_timeout_ms);
    let limit = (config.file.max_duration_seconds + 1).to_string();
    let rate = sample_rate.to_string();
    let mut args: Vec<OsString> = ["-nostdin", "-y", "-v", "error", "-i"]
        .iter()
        .map(OsString::from)
        .collect();
    args.push(source.as_os_str().to_os_string());
    args.extend(
        ["-t", &limit, "-ac", "1", "-ar", &rate, "-f", "f32le"]
            .iter()
            .map(OsString::from),
    );
    args.push(dest.as_os_str().to_os_string());
    percent(0);
    ffmpeg::run(&config.file.ffmpeg_path, &args, timeout, cancel)?;

    let bytes = std::fs::metadata(dest)
        .map_err(|err| StepError::Failed(format!("stat decoded audio: {err}")))?
        .len();
    if bytes == 0 {
        return Err(StepError::Failed("decoded audio is empty".to_string()));
    }
    if bytes % 4 != 0 {
        return Err(StepError::Failed(
            "decoded audio is not float-aligned".to_string(),
        ));
    }
    if bytes > max_bytes {
        return Err(StepError::Failed(format!(
            "audio exceeds duration limit ({} s)",
            config.file.max_duration_seconds
        )));
    }
    percent(FFMPEG_PERCENT);
    pack(dest, bytes, denoise, cancel, percent)
}

/// 裸流逐块打包：plain 直接切 320 样本，denoise 按 960 样本主干帧过 DSP。
/// 尾块不足一帧补零，保持 20 ms 帧节拍（同旧 wrapper 语义）。
fn pack(
    dest: &Path,
    bytes: u64,
    denoise: bool,
    cancel: &dyn Fn() -> bool,
    percent: &mut dyn FnMut(u8),
) -> Result<Vec<ContractFrame>, StepError> {
    let file = File::open(dest)
        .map_err(|err| StepError::Failed(format!("open decoded audio: {err}")))?;
    let mut reader = BufReader::new(file);
    let chunk_samples = if denoise { RAW_SAMPLES } else { INPUT_SAMPLES };
    let mut raw = vec![0u8; chunk_samples * 4];
    // 整形级默认关（旧 file_denoise 只开降噪）。
    let mut dsp = denoise.then(|| Dsp::new(true, false, false, false));
    let mut frames = Vec::new();
    let mut processed = 0u64;
    let mut block = 0u64;
    loop {
        let filled = fill(&mut reader, &mut raw)
            .map_err(|err| StepError::Failed(format!("read decoded audio: {err}")))?;
        if filled == 0 {
            break;
        }
        if cancel() {
            return Err(StepError::Cancelled);
        }
        raw[filled..].fill(0);
        // 主干缓冲按 960 样本定长；plain 只用前 320 个。
        let mut samples = [0f32; RAW_SAMPLES];
        for (slot, chunk) in samples[..chunk_samples].iter_mut().zip(raw.chunks_exact(4)) {
            *slot = f32::from_le_bytes(chunk.try_into().expect("4 字节一块"));
        }
        let frame = match dsp.as_mut() {
            Some(dsp) => {
                let mut trunk = RawFrame {
                    meta: FrameMeta {
                        pts_ns: block * FRAME_NS,
                        frame_idx: 0,
                        vad_flag: 0,
                        energy_db: 0,
                        conf: 0,
                        segment_id: 0,
                    },
                    pcm: samples,
                };
                let conf = dsp.process_frame(&mut trunk);
                let packed = dsp.pack_frame(&trunk, conf);
                // packed 字段按值搬出，不取引用（同 dsp/pre 约定）。
                let mut pcm = packed.pcm;
                let vad_flag = packed.meta.vad_flag;
                if vad_flag == 0 {
                    pcm.fill(0.0);
                }
                ContractFrame { pcm, vad_flag }
            }
            None => ContractFrame {
                pcm: samples[..INPUT_SAMPLES]
                    .try_into()
                    .expect("契约帧长 320 样本"),
                vad_flag: 1,
            },
        };
        frames.push(frame);
        processed += filled as u64;
        block += 1;
        let done = FFMPEG_PERCENT as u64 + (100 - FFMPEG_PERCENT) as u64 * processed / bytes;
        percent(done as u8);
    }
    Ok(frames)
}

/// 读满一块；EOF 时返回已读字节数（可能为 0）。
fn fill(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::testutil::temp_dir;
    use std::fs;
    use std::path::PathBuf;

    /// 写裸 f32 流（小端正弦），返回路径与字节数。
    fn raw_file(dir: &Path, samples: usize) -> (PathBuf, u64) {
        let path = dir.join("raw.f32");
        let mut bytes = Vec::with_capacity(samples * 4);
        for i in 0..samples {
            let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).sin();
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        fs::write(&path, &bytes).unwrap();
        (path, bytes.len() as u64)
    }

    #[test]
    fn plain_packing_splits_contract_frames_and_pads_tail() {
        let dir = temp_dir("decode-plain");
        // 16001 样本 → 51 帧，尾帧只有 1 个有效样本，其余补零。
        let (path, bytes) = raw_file(&dir, 16001);
        let mut last = 0u8;
        let frames = pack(&path, bytes, false, &|| false, &mut |p| last = p).unwrap();
        assert_eq!(frames.len(), 51);
        assert!(frames.iter().all(|frame| frame.vad_flag == 1));
        assert_eq!(frames[50].pcm[1..], [0.0; INPUT_SAMPLES - 1]);
        assert_eq!(last, 100, "打包结束报满阶段进度");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn denoise_packing_emits_one_contract_frame_per_trunk_frame() {
        let dir = temp_dir("decode-denoise");
        // 48000 个 48 kHz 样本 → 50 个 960 样本主干帧 → 50 个契约帧。
        let (path, bytes) = raw_file(&dir, 48000);
        let frames = pack(&path, bytes, true, &|| false, &mut |_| {}).unwrap();
        assert_eq!(frames.len(), 50);
        // 旧 file_denoise 语义：vad_flag==0 的帧补零。
        for frame in &frames {
            if frame.vad_flag == 0 {
                assert!(frame.pcm.iter().all(|v| *v == 0.0));
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn packing_stops_at_cancel_checkpoint() {
        let dir = temp_dir("decode-cancel");
        let (path, bytes) = raw_file(&dir, 48000);
        let err = pack(&path, bytes, true, &|| true, &mut |_| {}).unwrap_err();
        assert!(matches!(err, StepError::Cancelled));
        fs::remove_dir_all(&dir).unwrap();
    }
}
//! 结果编码发布：48 kHz 裸 f32 → ffmpeg WAV（pcm_f32le）→ 原子 rename。
//! 半成品（`.part`）与裸流临时文件由 worker 在收尾统一清理，此处不重复删。

use super::{ffmpeg, StepError};
use mozart_core::config::Config;
use mozart_core::OUTPUT_SAMPLE_RATE;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

/// 裸流落盘的分块样本数：栈缓冲 4 KiB，不整段再复制一份字节。
const WRITE_CHUNK: usize = 1024;

/// 编码并发布结果。成功时 `.part` 改名为 `<job_id>.output.wav`。
pub(super) fn publish(
    config: &Config,
    samples: &[f32],
    raw_path: &Path,
    part_path: &Path,
    result_path: &Path,
    cancel: &dyn Fn() -> bool,
) -> Result<(), StepError> {
    write_raw(raw_path, samples)?;
    if cancel() {
        return Err(StepError::Cancelled);
    }
    let rate = OUTPUT_SAMPLE_RATE.to_string();
    let mut args: Vec<OsString> = ["-nostdin", "-y", "-v", "error", "-f", "f32le", "-ar", &rate]
        .iter()
        .map(OsString::from)
        .collect();
    args.extend(["-ac", "1", "-i"].iter().map(OsString::from));
    args.push(raw_path.as_os_str().to_os_string());
    args.extend(["-f", "wav", "-c:a", "pcm_f32le"].iter().map(OsString::from));
    args.push(part_path.as_os_str().to_os_string());
    ffmpeg::run(
        &config.file.ffmpeg_path,
        &args,
        Duration::from_millis(config.file.ffmpeg_timeout_ms),
        cancel,
    )?;
    if cancel() {
        return Err(StepError::Cancelled);
    }
    fs::rename(part_path, result_path)
        .map_err(|err| StepError::Failed(format!("publish result: {err}")))
}

fn write_raw(path: &Path, samples: &[f32]) -> Result<(), StepError> {
    let file =
        File::create(path).map_err(|err| StepError::Failed(format!("create output raw: {err}")))?;
    let mut writer = BufWriter::new(file);
    let mut bytes = [0u8; WRITE_CHUNK * 4];
    for chunk in samples.chunks(WRITE_CHUNK) {
        for (slot, sample) in bytes.chunks_exact_mut(4).zip(chunk) {
            slot.copy_from_slice(&sample.to_le_bytes());
        }
        let used = chunk.len() * 4;
        writer
            .write_all(&bytes[..used])
            .map_err(|err| StepError::Failed(format!("write output raw: {err}")))?;
    }
    writer
        .flush()
        .map_err(|err| StepError::Failed(format!("flush output raw: {err}")))
}
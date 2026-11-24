//! 帧契约镜像与控制面信封，对应 core/include/mozart/frame.h 与 ctl.h。
//! 修改 C 头后必须同步本文件；布局由编译期断言保证。

pub mod config;
pub mod ctl;
pub mod dgram;
pub mod preset;
pub mod ring;
pub mod speech;
pub mod thread;

pub const RAW_SAMPLE_RATE: u32 = 48000;
pub const INPUT_SAMPLE_RATE: u32 = 16000;
pub const OUTPUT_SAMPLE_RATE: u32 = 48000;
pub const FRAME_MS: u32 = 20;
pub const RAW_SAMPLES: usize = 960;
pub const INPUT_SAMPLES: usize = 320;
pub const INPUT_PCM_ABS_MAX: f32 = 1.0;
pub const OUTPUT_SAMPLES: usize = 960;
pub const CHANNELS: usize = 1;

pub const INPUT_PACKET_MAGIC: u32 = 0x4D5A_5254;
pub const OUTPUT_PACKET_MAGIC: u32 = 0x5452_5A4D;
pub const SPEECH_PACKET_MAGIC: u32 = 0x5053_5A4D;
pub const PACKET_HEADER_SIZE: usize = 20;

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct FrameMeta {
    pub pts_ns: u64,
    pub frame_idx: u32,
    pub vad_flag: u8,
    pub energy_db: u8,
    pub conf: u8,
    pub segment_id: u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct InputFrame {
    pub meta: FrameMeta,
    pub pcm: [f32; INPUT_SAMPLES],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct OutputFrame {
    pub meta: FrameMeta,
    pub sample_rate: u32,
    pub valid_samples: u32,
    pub pcm: [f32; OUTPUT_SAMPLES],
}

impl OutputFrame {
    pub fn new(
        meta: FrameMeta,
        sample_rate: u32,
        valid_samples: u32,
        pcm: [f32; OUTPUT_SAMPLES],
    ) -> Self {
        Self {
            meta,
            sample_rate,
            valid_samples,
            pcm,
        }
    }

    pub fn new_48k(meta: FrameMeta, pcm: [f32; OUTPUT_SAMPLES]) -> Self {
        Self::new(meta, OUTPUT_SAMPLE_RATE, OUTPUT_SAMPLES as u32, pcm)
    }
}

pub const ENHANCE_PACKET_MAGIC: u32 = 0x534E_5A4D;
pub const ENHANCE_PRIMING_FRAMES: usize = 2;

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct EnhanceRequest {
    pub magic: u32,
    pub frame: InputFrame,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct EnhanceReply {
    pub magic: u32,
    pub ready: u32,
    pub frame: InputFrame,
}

const _: () = assert!(std::mem::size_of::<EnhanceRequest>() == 1300);
const _: () = assert!(std::mem::size_of::<EnhanceReply>() == 1304);
const _: () = assert!(std::mem::offset_of!(EnhanceRequest, frame) == 4);
const _: () = assert!(std::mem::offset_of!(EnhanceReply, frame) == 8);

/// TTS output carries a whole-utterance admission token without changing frame timestamps.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct SpeechFrame {
    pub issued_ns: u64,
    pub frame: OutputFrame,
}

const _: () = assert!(std::mem::size_of::<SpeechFrame>() + 4 == 3876);
const _: () = assert!(std::mem::offset_of!(SpeechFrame, frame) == 8);

/// 48 kHz 原始采集帧（进程内使用，不跨进程边界），对应 frame.h 的 `mozart_raw_frame_t`。
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct RawFrame {
    pub meta: FrameMeta,
    pub pcm: [f32; RAW_SAMPLES],
}

const _: () = assert!(std::mem::size_of::<FrameMeta>() == 16);
const _: () = assert!(std::mem::size_of::<InputFrame>() == 16 + INPUT_SAMPLES * 4);
const _: () = assert!(std::mem::size_of::<OutputFrame>() == 24 + OUTPUT_SAMPLES * 4);
const _: () = assert!(std::mem::size_of::<RawFrame>() == 16 + RAW_SAMPLES * 4);
const _: () = assert!(std::mem::size_of::<InputFrame>() + 4 == 1300);
const _: () = assert!(std::mem::size_of::<OutputFrame>() + 4 == 3868);

const _: () = assert!(std::mem::offset_of!(FrameMeta, pts_ns) == 0);
const _: () = assert!(std::mem::offset_of!(FrameMeta, frame_idx) == 8);
const _: () = assert!(std::mem::offset_of!(FrameMeta, vad_flag) == 12);
const _: () = assert!(std::mem::offset_of!(FrameMeta, energy_db) == 13);
const _: () = assert!(std::mem::offset_of!(FrameMeta, conf) == 14);
const _: () = assert!(std::mem::offset_of!(FrameMeta, segment_id) == 15);
const _: () = assert!(std::mem::offset_of!(InputFrame, meta) == 0);
const _: () = assert!(std::mem::offset_of!(InputFrame, pcm) == 16);
const _: () = assert!(std::mem::offset_of!(OutputFrame, meta) == 0);
const _: () = assert!(std::mem::offset_of!(OutputFrame, sample_rate) == 16);
const _: () = assert!(std::mem::offset_of!(OutputFrame, valid_samples) == 20);
const _: () = assert!(std::mem::offset_of!(OutputFrame, pcm) == 24);
const _: () = assert!(std::mem::offset_of!(RawFrame, meta) == 0);
const _: () = assert!(std::mem::offset_of!(RawFrame, pcm) == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_sizes_match_c_header() {
        assert_eq!(std::mem::size_of::<FrameMeta>(), 16);
        assert_eq!(std::mem::size_of::<InputFrame>(), 1296);
        assert_eq!(std::mem::size_of::<OutputFrame>(), 3864);
        assert_eq!(std::mem::size_of::<RawFrame>(), 3856);
    }
}

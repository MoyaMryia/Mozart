//! 帧契约镜像，对应 core/include/mozart/frame.h。
//! 修改 C 头后必须同步本文件；大小由编译期断言保证。

pub const RAW_SAMPLE_RATE: u32 = 48000;
pub const INPUT_SAMPLE_RATE: u32 = 16000;
pub const OUTPUT_SAMPLE_RATE: u32 = 48000;
pub const FRAME_MS: u32 = 20;
pub const RAW_SAMPLES: usize = 960;
pub const INPUT_SAMPLES: usize = 320;
pub const OUTPUT_SAMPLES: usize = 960;
pub const CHANNELS: usize = 1;

pub const INPUT_PACKET_MAGIC: u32 = 0x4D5A_5254;
pub const OUTPUT_PACKET_MAGIC: u32 = 0x5452_5A4D;
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
    pub pcm: [f32; OUTPUT_SAMPLES],
}

const _: () = assert!(std::mem::size_of::<FrameMeta>() == 16);
const _: () = assert!(std::mem::size_of::<InputFrame>() == 16 + INPUT_SAMPLES * 4);
const _: () = assert!(std::mem::size_of::<OutputFrame>() == 16 + OUTPUT_SAMPLES * 4);
const _: () = assert!(std::mem::size_of::<InputFrame>() + 4 == 1300);
const _: () = assert!(std::mem::size_of::<OutputFrame>() + 4 == 3860);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_sizes_match_c_header() {
        assert_eq!(std::mem::size_of::<FrameMeta>(), 16);
        assert_eq!(std::mem::size_of::<InputFrame>(), 1296);
        assert_eq!(std::mem::size_of::<OutputFrame>(), 3856);
    }
}

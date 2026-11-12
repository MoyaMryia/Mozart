//! VAD 滞回与有声判据（旧 dsp.c L37-41, L166-198 等价重写）。
//! 混合判据：WOLA 置信度（SNR 结构）× 主干绝对电平——纯 SNR 判据在停顿
//! 噪声地板处悬浮于退出门限附近，电平门限兜底（旧注释）。
//! 滞回：有声候选 conf ≥ 0.5 且电平可闻；进入需连续 3 帧 conf ≥ 0.6（60 ms），
//! 退出需连续 25 帧 conf < 0.3（500 ms）。`segment_id` 0 = 静音间隔。

/// 进入有声的置信度门限（旧 SEG_ENTER_PROB）。
const ENTER_PROB: f32 = 0.6;
/// 进入有声所需连续帧数（旧 SEG_ENTER_FRAMES，60 ms）。
const ENTER_FRAMES: u32 = 3;
/// 退出有声的置信度门限（旧 SEG_EXIT_PROB）。
const EXIT_PROB: f32 = 0.3;
/// 退出有声所需连续帧数（旧 SEG_EXIT_FRAMES，500 ms）。
const EXIT_FRAMES: u32 = 25;
/// 有声候选的置信度门限（旧 dsp.c L175 `conf >= 0.5f`）。
const CANDIDATE_PROB: f32 = 0.5;

pub struct Vad {
    in_speech: bool,
    enter_count: u32,
    exit_count: u32,
    segment_id: u8,
}

impl Vad {
    pub fn new() -> Self {
        Self {
            in_speech: false,
            enter_count: 0,
            exit_count: 0,
            segment_id: 0,
        }
    }

    /// 推进一帧滞回，返回 (vad_flag, segment_id)。
    /// `conf` 为本帧 WOLA 置信度，`audible` 为主干 RMS > -60 dB。
    pub fn update(&mut self, conf: f32, audible: bool) -> (u8, u8) {
        let candidate = conf >= CANDIDATE_PROB && audible;
        if self.in_speech {
            let quiet = conf < EXIT_PROB || !audible;
            self.exit_count = if quiet {
                self.exit_count + 1
            } else {
                0
            };
            if self.exit_count >= EXIT_FRAMES {
                self.in_speech = false;
                self.exit_count = 0;
            }
        } else {
            let ready = conf >= ENTER_PROB && audible;
            self.enter_count = if ready {
                self.enter_count + 1
            } else {
                0
            };
            if self.enter_count >= ENTER_FRAMES {
                self.in_speech = true;
                self.enter_count = 0;
                // 0 保留给静音间隔；回绕到 0 时跳到 1（旧 dsp.c L189-190）。
                self.segment_id = self.segment_id.wrapping_add(1);
                if self.segment_id == 0 {
                    self.segment_id = 1;
                }
            }
        }
        // 候选帧即置 vad_flag（滞回未完成时提前反映瞬时判据，旧 L195 语义）。
        let vad_flag = u8::from(self.in_speech || candidate);
        let segment_id = if self.in_speech { self.segment_id } else { 0 };
        (vad_flag, segment_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enters_after_three_ready_frames() {
        let mut vad = Vad::new();
        for frame in 0..ENTER_FRAMES {
            let (vad_flag, segment) = vad.update(0.7, true);
            // 候选判据先于滞回点亮 vad_flag；分段号在滞回完成前保持 0。
            assert_eq!(vad_flag, 1, "候选帧 {frame} 必须点亮 vad_flag");
            if frame < ENTER_FRAMES - 1 {
                assert_eq!(segment, 0, "滞回完成前分段号必须为 0");
            }
        }
        let (_, segment) = vad.update(0.7, true);
        assert_eq!(segment, 1, "首个有声分段号必须为 1");
        // 保持有声：候选判据不满足时 vad_flag 仍由滞回维持。
        let (vad_flag, segment) = vad.update(0.4, true);
        assert_eq!(vad_flag, 1, "滞回期内 vad_flag 保持 1");
        assert_eq!(segment, 1);
    }

    #[test]
    fn enter_counter_resets_on_single_gap() {
        let mut vad = Vad::new();
        vad.update(0.7, true);
        vad.update(0.7, true);
        vad.update(0.1, true); // 未达进入门限：计数清零。
        let (vad_flag, segment) = vad.update(0.7, true);
        assert_eq!(vad_flag, 1, "候选仍点亮");
        assert_eq!(segment, 0, "断一帧后进入计数必须重来，不得提前分段");
        vad.update(0.7, true);
        vad.update(0.7, true);
        let (_, segment) = vad.update(0.7, true);
        assert_eq!(segment, 1, "清零后需再连续 3 帧才进入");
    }

    #[test]
    fn exits_after_twentyfive_quiet_frames() {
        let mut vad = Vad::new();
        for _ in 0..ENTER_FRAMES {
            vad.update(0.7, true);
        }
        // 前 24 个静帧保持有声（滞回尾巴）；第 25 帧退出。
        for frame in 0..EXIT_FRAMES {
            let (vad_flag, segment) = vad.update(0.1, true);
            if frame < EXIT_FRAMES - 1 {
                assert_eq!((vad_flag, segment), (1, 1), "静帧 {frame} 应仍滞回在有声");
            } else {
                assert_eq!((vad_flag, segment), (0, 0), "第 25 个静帧必须退出并归零分段");
            }
        }
        // 低置信但候选以下：不重新进入。
        let (vad_flag, segment) = vad.update(0.2, true);
        assert_eq!((vad_flag, segment), (0, 0));
    }

    #[test]
    fn inaudible_blocks_entry_and_clears_flag() {
        // 电平门限兜底：置信度足够但主干静音（不可闻），不得进入有声。
        let mut vad = Vad::new();
        for _ in 0..10 {
            let (vad_flag, segment) = vad.update(0.9, false);
            assert_eq!((vad_flag, segment), (0, 0), "不可闻帧不得点亮 vad_flag");
        }
    }

    #[test]
    fn segment_id_increments_per_speech_run_and_skips_zero_on_wrap() {
        let mut vad = Vad::new();
        let mut last = 0u8;
        // 256 个有声段后回绕：255→0 被跳过改为 1（旧 dsp.c L189-190）。
        for run in 1..=256u32 {
            for _ in 0..ENTER_FRAMES {
                vad.update(0.7, true);
            }
            let (_, segment) = vad.update(0.7, true);
            if run == 256 {
                assert_eq!(segment, 1, "回绕后分段号必须跳过 0");
            } else {
                assert_eq!(segment, (run as u8).min(255), "第 {run} 段分段号");
            }
            last = segment;
            for _ in 0..EXIT_FRAMES {
                vad.update(0.1, true);
            }
        }
        assert_eq!(last, 1);
    }

    #[test]
    fn exit_counter_resets_on_single_ready_frame() {
        let mut vad = Vad::new();
        for _ in 0..ENTER_FRAMES {
            vad.update(0.7, true);
        }
        for _ in 0..EXIT_FRAMES - 1 {
            vad.update(0.1, true);
        }
        // 退出计数差一帧时回到有声：计数清零，不退出。
        vad.update(0.7, true);
        let (vad_flag, segment) = vad.update(0.1, true);
        assert_eq!((vad_flag, segment), (1, 1), "单帧回声必须重置退出计数");
        for _ in 0..EXIT_FRAMES - 1 {
            vad.update(0.1, true);
        }
        let (vad_flag, _) = vad.update(0.1, true);
        assert_eq!(vad_flag, 0, "重置后仍需连续 25 个静帧才退出");
    }
}

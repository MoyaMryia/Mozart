//! WOLA 谱降噪：旧 noise_suppressor.c 的 Rust 等价移植（内部 f64 精度，与旧实现
//! 及其 numpy 参考一致）。参数固定为生产默认（frame=128、hop=64、lookahead=0、
//! alpha_noise=0.99、beta_dd=0.98、noise_leak=0.0004、leak_gate=3.0、
//! floor=-18 dB、频域平滑半径 1），不设配置键。
//!
//! 与旧实现的差异（SPEC-02 已确认）：
//! - bypass 模式不移植——禁用降噪由上层 `Dsp` 整级跳过，不再走增益恒 1 的
//!   WOLA 重建，也就没有旧实现切换后约 0.5 s 的收敛期；
//! - `flush`（尾部补零排空）只服务离线文件模式，实时流不结束，不移植；
//! - 遥测计数（consumed/last_gain）无消费方，不移植；`emitted` 保留——
//!   首帧增益分支依赖它。
//!
//! 全部状态为定长数组，构造后零分配；`push` 可在实时线程按帧调用。

use super::fft::{fft_128, NS_FRAME, NS_PI};

pub(super) const NS_HOP: usize = 64;
pub(super) const NS_BINS: usize = NS_FRAME / 2 + 1;

/// 噪声估计初始化帧数（旧 NS_INIT_FRAMES）。
const NS_INIT_FRAMES: usize = 8;
/// 频谱队列深度上界：初始化积压 8 帧；lookahead 固定 0，发射即排空，深度 ≤ 8。
const NS_QUEUE_MAX: usize = NS_INIT_FRAMES;

const ALPHA_NOISE: f64 = 0.99;
const BETA_DD: f64 = 0.98;
const NOISE_LEAK: f64 = 0.0004;
const LEAK_GATE: f64 = 3.0;
const FLOOR_DB: f64 = -18.0;
const NOISE_SMOOTH_BINS: usize = 1;

pub(super) struct NoiseSuppressor {
    floor: f64,
    window: [f64; NS_FRAME],
    re: [f64; NS_FRAME],
    im: [f64; NS_FRAME],
    // 估计器状态
    ready: bool,
    init_count: usize,
    init_power: [f64; NS_BINS],
    power_slow: [f64; NS_BINS],
    noise: [f64; NS_BINS],
    gain_prev: [f64; NS_BINS],
    // 发射队列：频谱按帧序排队，发射时对最老帧算增益
    queue_re: [[f64; NS_BINS]; NS_QUEUE_MAX],
    queue_im: [[f64; NS_BINS]; NS_QUEUE_MAX],
    queue_len: usize,
    // 滑窗输入与 WOLA 输出
    in_buf: [f64; NS_FRAME],
    in_len: usize,
    ola: [f64; NS_FRAME],
    norm: [f64; NS_FRAME],
    // 置信度累积（查询即清零）
    conf_sum: f64,
    conf_count: u64,
    /// 已发射样本数；首帧增益用纯谱减，其后决策导向（同旧实现）。
    emitted: u64,
}

impl NoiseSuppressor {
    pub(super) fn new() -> Self {
        // 周期 sqrt-Hann：hann(i) = 0.5 - 0.5·cos(2πi/N)（与 numpy hanning(N+1)[:N] 一致）
        let mut window = [0.0f64; NS_FRAME];
        for (i, w) in window.iter_mut().enumerate() {
            let hann = 0.5 - 0.5 * (2.0 * NS_PI * i as f64 / NS_FRAME as f64).cos();
            *w = hann.sqrt();
        }
        Self {
            floor: 10.0f64.powf(FLOOR_DB / 20.0),
            window,
            re: [0.0; NS_FRAME],
            im: [0.0; NS_FRAME],
            ready: false,
            init_count: 0,
            init_power: [0.0; NS_BINS],
            power_slow: [0.0; NS_BINS],
            noise: [0.0; NS_BINS],
            gain_prev: [1.0; NS_BINS],
            queue_re: [[0.0; NS_BINS]; NS_QUEUE_MAX],
            queue_im: [[0.0; NS_BINS]; NS_QUEUE_MAX],
            queue_len: 0,
            in_buf: [0.0; NS_FRAME],
            in_len: 0,
            ola: [0.0; NS_FRAME],
            norm: [0.0; NS_FRAME],
            conf_sum: 0.0,
            conf_count: 0,
            emitted: 0,
        }
    }

    /// 流式处理：吃 `input` 个样本，向 `out` 写至多 `out.len()` 个样本。
    /// 输出与输入索引对齐；发射滞后 64 样本（初始化 8 帧攒在队列，就绪后一次
    /// 排干）。任意长度分块与整块结果逐位一致（状态按样本推进）。
    /// 返回实写样本数。
    pub(super) fn push(&mut self, input: &[f32], out: &mut [f32]) -> usize {
        let mut written = 0usize;
        for &sample in input {
            self.in_buf[self.in_len] = sample as f64;
            self.in_len += 1;
            if self.in_len < NS_FRAME {
                continue;
            }
            self.analyze();
            // 滑窗：丢最老 hop，保留 frame-hop 重叠
            self.in_buf.copy_within(NS_HOP..NS_FRAME, 0);
            self.in_len = NS_FRAME - NS_HOP;
            // 就绪后队列排空（lookahead=0）
            while self.ready && self.queue_len > 0 {
                self.emit_oldest(out, &mut written);
            }
        }
        written
    }

    /// 自上次查询以来的语音置信度（0..1）：功率加权的 post-SNR ≥ 2 的 bin 占比。
    /// 查询后清零累积，供 VAD（Task 4）按帧取用；语义同旧 mozart_ns_speech_confidence。
    pub(super) fn speech_confidence(&mut self) -> f64 {
        if self.conf_count == 0 {
            return 0.0;
        }
        let value = self.conf_sum / self.conf_count as f64;
        self.conf_sum = 0.0;
        self.conf_count = 0;
        value
    }

    /// 分析一帧：加窗 FFT、频谱入队、更新噪声估计（旧 ns_update，L126-170）。
    fn analyze(&mut self) {
        for i in 0..NS_FRAME {
            self.re[i] = self.window[i] * self.in_buf[i];
            self.im[i] = 0.0;
        }
        fft_128(&mut self.re, &mut self.im, false);

        for b in 0..NS_BINS {
            self.queue_re[self.queue_len][b] = self.re[b];
            self.queue_im[self.queue_len][b] = self.im[b];
        }
        self.queue_len += 1;

        let mut power = [0.0f64; NS_BINS];
        for b in 0..NS_BINS {
            power[b] = self.re[b] * self.re[b] + self.im[b] * self.im[b];
        }

        if !self.ready {
            for b in 0..NS_BINS {
                self.init_power[b] += power[b];
            }
            self.init_count += 1;
            if self.init_count >= NS_INIT_FRAMES {
                for b in 0..NS_BINS {
                    let averaged = self.init_power[b] / NS_INIT_FRAMES as f64;
                    self.power_slow[b] = averaged;
                    self.noise[b] = averaged;
                }
                self.ready = true;
            }
            return;
        }

        for b in 0..NS_BINS {
            self.power_slow[b] =
                ALPHA_NOISE * self.power_slow[b] + (1.0 - ALPHA_NOISE) * power[b];
        }
        let mut reference = [0.0f64; NS_BINS];
        freq_smooth(&self.power_slow, &mut reference);
        for b in 0..NS_BINS {
            let proposed = if reference[b] < LEAK_GATE * self.noise[b] {
                self.noise[b] * (1.0 + NOISE_LEAK)
            } else {
                self.noise[b]
            };
            self.noise[b] = if proposed < reference[b] { proposed } else { reference[b] };
        }
    }

    /// 发射最老帧：增益、IFFT 合成、WOLA 叠加，产出 hop 个样本
    /// （旧 ns_emit_oldest，L174-235）。
    fn emit_oldest(&mut self, out: &mut [f32], written: &mut usize) {
        let mut power = [0.0f64; NS_BINS];
        let mut gain = [0.0f64; NS_BINS];
        let mut above = 0.0f64;
        let mut total = 0.0f64;

        for b in 0..NS_BINS {
            power[b] = self.queue_re[0][b] * self.queue_re[0][b]
                + self.queue_im[0][b] * self.queue_im[0][b];
            let post = power[b] / if self.noise[b] > 1e-20 { self.noise[b] } else { 1e-20 };
            // 首帧纯谱减，其后决策导向维纳（同旧实现）
            let xi = if self.emitted == 0 {
                let diff = post - 1.0;
                if diff > 0.0 { diff } else { 0.0 }
            } else {
                let diff = post - 1.0;
                let rectified = if diff > 0.0 { diff } else { 0.0 };
                BETA_DD * self.gain_prev[b] * self.gain_prev[b] * post
                    + (1.0 - BETA_DD) * rectified
            };
            let mut g = xi / (1.0 + xi);
            if g < self.floor {
                g = self.floor;
            }
            gain[b] = g;
            self.gain_prev[b] = g;
            total += power[b];
            if post >= 2.0 {
                above += power[b];
            }
        }
        if total > 1e-20 {
            self.conf_sum += above / total;
            self.conf_count += 1;
        }

        // 合成：增益乘谱、共轭对称补全、IFFT、加窗叠加
        for b in 0..NS_BINS {
            self.re[b] = self.queue_re[0][b] * gain[b];
            self.im[b] = self.queue_im[0][b] * gain[b];
        }
        for i in 1..NS_FRAME / 2 {
            self.re[NS_FRAME - i] = self.re[i];
            self.im[NS_FRAME - i] = -self.im[i];
        }
        fft_128(&mut self.re, &mut self.im, true);
        for i in 0..NS_FRAME {
            self.ola[i] += self.window[i] * self.re[i];
            self.norm[i] += self.window[i] * self.window[i];
        }

        // 出队（队头即最老帧）
        self.queue_len -= 1;
        self.queue_re.copy_within(1..NS_QUEUE_MAX, 0);
        self.queue_im.copy_within(1..NS_QUEUE_MAX, 0);

        // 产出 hop 个样本并滚动缓冲；out 余量不足时提前停（同旧实现）
        for i in 0..NS_HOP {
            if *written >= out.len() {
                break;
            }
            let n = if self.norm[i] > 1e-9 { self.norm[i] } else { 1e-9 };
            out[*written] = (self.ola[i] / n) as f32;
            *written += 1;
            self.emitted += 1;
        }
        self.ola.copy_within(NS_HOP..NS_FRAME, 0);
        self.norm.copy_within(NS_HOP..NS_FRAME, 0);
        for i in NS_FRAME - NS_HOP..NS_FRAME {
            self.ola[i] = 0.0;
            self.norm[i] = 0.0;
        }
    }
}

/// 频域平滑（两端复制延拓，对副本；旧 freq_smooth，L105-122）。
/// 平滑半径固定为生产默认 1（旧 mozart_ns_config_t.noise_smooth_bins 默认）；
/// radius=0 的直通分支在旧实现生产配置下不可达，不移植。
fn freq_smooth(input: &[f64; NS_BINS], out: &mut [f64; NS_BINS]) {
    const RADIUS: isize = NOISE_SMOOTH_BINS as isize;
    let denom = (2 * RADIUS + 1) as f64;
    for b in 0..NS_BINS {
        let mut sum = 0.0f64;
        for k in -RADIUS..=RADIUS {
            let mut idx = b as isize + k;
            if idx < 0 {
                idx = 0;
            }
            if idx >= NS_BINS as isize {
                idx = NS_BINS as isize - 1;
            }
            sum += input[idx as usize];
        }
        out[b] = sum / denom;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_satisfies_cola() {
        // 周期 sqrt-Hann 50% 重叠：w²[i] + w²[i+hop] = 1（重建归一恒定）
        let ns = NoiseSuppressor::new();
        for i in 0..NS_HOP {
            let sum = ns.window[i] * ns.window[i] + ns.window[i + NS_HOP] * ns.window[i + NS_HOP];
            assert!((sum - 1.0).abs() < 1e-12, "i={i} sum={sum}");
        }
        assert_eq!(ns.window[0], 0.0);
    }

    #[test]
    fn startup_bursts_after_init_frames() {
        // 帧 k 在输入 128+64k 处完成；第 8 帧（k=7，输入 576）就绪并一次排干 8 帧
        let mut ns = NoiseSuppressor::new();
        let mut out = [0f32; 4096];
        let zeros = [0f32; 64];
        for _ in 0..8 {
            assert_eq!(ns.push(&zeros, &mut out), 0, "就绪前不得发射");
        }
        assert_eq!(ns.push(&zeros, &mut out), 512, "第 576 样本就绪后排干 8 帧");
        // 稳态：每 64 进 64 出
        assert_eq!(ns.push(&zeros, &mut out[..512]), 64);
        // 静音不累积置信度
        assert_eq!(ns.speech_confidence(), 0.0);
    }

    #[test]
    fn chunking_does_not_change_output() {
        // 流式合同：任意分块与整块逐位一致（旧实现头部注释同此声明）
        let mut input = [0f32; 9600];
        for (i, v) in input.iter_mut().enumerate() {
            let t = i as f32 / 48000.0;
            *v = 0.3 * (2.0 * core::f32::consts::PI * 300.0 * t).sin()
                + 0.02 * ((i as i32).wrapping_mul(1103515245) as f32 / 1e7);
        }
        let mut whole = [0f32; 9600];
        let mut ns = NoiseSuppressor::new();
        let whole_written = ns.push(&input, &mut whole);
        let mut split = Vec::with_capacity(9600);
        let mut ns2 = NoiseSuppressor::new();
        let chunks = [1usize, 7, 64, 128, 333, 960, 5, 4096];
        let mut pos = 0usize;
        let mut ci = 0usize;
        while pos < input.len() {
            let take = chunks[ci % chunks.len()].min(input.len() - pos);
            let mut sink = [0f32; 4096];
            let written = ns2.push(&input[pos..pos + take], &mut sink);
            split.extend_from_slice(&sink[..written]);
            pos += take;
            ci += 1;
        }
        assert_eq!(whole_written, split.len(), "分块不得改变发射总数");
        assert_eq!(&whole[..whole_written], &split[..]);
    }
}

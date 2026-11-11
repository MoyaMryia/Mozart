//! 128 点 radix-2 迭代 FFT：旧 noise_suppressor.c `fft_run`（L62-101）的逐行移植。
//! 不用 rustfft 的取舍（对拍依据见 Task 3 报告）：与旧 C 同算法、同运算顺序、
//! 同 f64 精度，跨平台差异只剩 libm cos/sin 末位，对拍门限可以收得很紧；
//! 且免去 Jetson 端新增 crates.io 依赖。旋转因子与旧实现同法在级内乘法递推。

pub(super) const NS_FRAME: usize = 128;
pub(super) const NS_PI: f64 = 3.14159265358979323846;

/// 原地 FFT。inverse = true 时做逆变换并除以 n（含归一化），与旧实现一致。
pub(super) fn fft_128(re: &mut [f64; NS_FRAME], im: &mut [f64; NS_FRAME], inverse: bool) {
    let n = NS_FRAME;
    // 位反转置换
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    // 蝶形：每级主旋转因子用乘法递推，保持与旧实现相同的浮点路径
    let mut len = 2usize;
    while len <= n {
        let angle = (if inverse { 2.0 } else { -2.0 }) * NS_PI / len as f64;
        let wr = angle.cos();
        let wi = angle.sin();
        let mut i = 0usize;
        while i < n {
            let mut cr = 1.0f64;
            let mut ci = 0.0f64;
            for k in 0..len / 2 {
                let a = i + k;
                let b = a + len / 2;
                let tr = cr * re[b] - ci * im[b];
                let ti = cr * im[b] + ci * re[b];
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
    if inverse {
        for v in re.iter_mut().chain(im.iter_mut()) {
            *v /= n as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性 f64 源（xorshift64*），避免依赖外部随机。
    fn sample(i: usize) -> f64 {
        let mut x = (i as u64).wrapping_mul(0x9E3779B97F4A7C15) | 1;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        (x.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    }

    #[test]
    fn impulse_gives_flat_spectrum() {
        let mut re = [0.0; NS_FRAME];
        let mut im = [0.0; NS_FRAME];
        re[0] = 1.0;
        fft_128(&mut re, &mut im, false);
        for k in 0..NS_FRAME {
            assert!(
                (re[k] - 1.0).abs() < 1e-12 && im[k].abs() < 1e-12,
                "bin {k}: re={} im={}",
                re[k],
                im[k]
            );
        }
    }

    #[test]
    fn sine_peaks_at_expected_bins() {
        let mut re = [0.0; NS_FRAME];
        let mut im = [0.0; NS_FRAME];
        for (i, v) in re.iter_mut().enumerate() {
            *v = (2.0 * NS_PI * 5.0 * i as f64 / NS_FRAME as f64).cos();
        }
        fft_128(&mut re, &mut im, false);
        // 实余弦在 ±bin5（即 5 与 123）各贡献一半幅 N/2
        assert!((re[5] - 64.0).abs() < 1e-9, "re[5]={}", re[5]);
        assert!((re[NS_FRAME - 5] - 64.0).abs() < 1e-9, "re[123]={}", re[NS_FRAME - 5]);
        let noise: f64 = (0..NS_FRAME)
            .filter(|k| *k != 5 && *k != NS_FRAME - 5)
            .map(|k| re[k] * re[k] + im[k] * im[k])
            .sum();
        assert!(noise.sqrt() < 1e-9, "泄漏能量 sqrt={noise}");
    }

    #[test]
    fn roundtrip_restores_input() {
        let mut re = [0.0f64; NS_FRAME];
        let mut im = [0.0f64; NS_FRAME];
        let mut orig = [0.0f64; NS_FRAME];
        for (i, v) in orig.iter_mut().enumerate() {
            *v = sample(i);
        }
        re.copy_from_slice(&orig);
        fft_128(&mut re, &mut im, false);
        fft_128(&mut re, &mut im, true);
        for i in 0..NS_FRAME {
            assert!((re[i] - orig[i]).abs() < 1e-12, "i={i} diff={}", re[i] - orig[i]);
            assert!(im[i].abs() < 1e-12, "im[{i}]={}", im[i]);
        }
    }
}

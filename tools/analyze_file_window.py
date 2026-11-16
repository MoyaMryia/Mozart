#!/usr/bin/env python3
"""文件链路分窗质量分析（docs/RVC-FILE-BENCHMARK.md 的契约/对齐/接缝三层）。

用法（uv 提供 numpy/scipy/librosa）：
  uv run --with numpy --with scipy --with librosa python tools/analyze_file_window.py \
    --input in.wav --output out.wav --hop-s 21.8 --crossfade-s 0.2 --windows 2 \
    --label trump-gkw-30s --json metrics.json

接缝理论位置：第 k 条缝（k=1..windows-1）在输出 (k*hop + crossfade/2) 秒处。
F0 用 librosa pyin（16 kHz、60–500 Hz、hop 160），与 reports/rvc-30s-voice-20261115 同口径。
"""
import argparse
import json

import librosa
import numpy as np


def load_mono(path, sr):
    x, _ = librosa.load(path, sr=sr, mono=True)
    return x.astype(np.float64)


def f0_cents(x16k):
    f0, voiced, _ = librosa.pyin(
        x16k, fmin=60.0, fmax=500.0, sr=16000, hop_length=160)
    return f0, voiced


def align_and_diff(f0_in, f0_out, max_lag=100):
    n = min(len(f0_in), len(f0_out))
    a, b = f0_in[:n], f0_out[:n]
    va, vb = np.isfinite(a) & (a > 0), np.isfinite(b) & (b > 0)
    both = va & vb
    iou = float(both.sum() / max(1, (va | vb).sum()))
    if both.sum() < 10:
        return {"voiced_iou": iou, "both_voiced_frames": int(both.sum())}
    cents = np.zeros(n)
    cents[both] = 1200.0 * np.log2(b[both] / a[both])
    # 全局滞后：有声帧质心轮廓互相关，±max_lag 帧（±1 s @100 Hz）。
    ca = np.where(va, np.log2(np.maximum(a, 1e-6)), 0.0)
    cb = np.where(vb, np.log2(np.maximum(b, 1e-6)), 0.0)
    ca -= ca[va].mean() if va.any() else 0.0
    cb -= cb[vb].mean() if vb.any() else 0.0
    corr = np.correlate(cb, ca, mode="full")
    lags = np.arange(-(n - 1), n)
    win = (lags >= -max_lag) & (lags <= max_lag)
    best = int(lags[win][np.argmax(corr[win])])
    shifted = np.roll(cents, best)
    valid = np.roll(both, best)
    dev = np.abs(shifted[valid])
    return {
        "voiced_iou": iou,
        "both_voiced_frames": int(both.sum()),
        "best_lag_frames": best,
        "best_lag_ms": best * 10,
        "cents_dev_median": float(np.median(dev)),
        "cents_dev_p90": float(np.percentile(dev, 90)),
        "cents_dev_at_lag0_median": float(np.median(np.abs(cents[both]))),
    }


def spectral_flux(x48k):
    mag = np.abs(librosa.stft(x48k, n_fft=2048, hop_length=512))
    flux = np.maximum(0.0, np.diff(mag, axis=1)).sum(axis=0)
    return flux


def seam_metrics(x48k, sr, seams_s, flux, flux_in=None):
    hop_ms = 512 / sr * 1000.0
    half = max(1, int(round(100.0 / hop_ms)))  # ±100 ms
    guard = max(1, int(round(300.0 / hop_ms)))
    n = len(flux)
    seam_idx = [int(round(t * sr / 512)) for t in seams_s]
    mask = np.ones(n, dtype=bool)
    mask[:int(round(500.0 / hop_ms))] = False
    mask[n - int(round(500.0 / hop_ms)):] = False
    picks = []
    for c in seam_idx:
        lo, hi = max(0, c - half), min(n, c + half + 1)
        picks.append(flux[lo:hi])
        mask[max(0, c - guard):min(n, c + guard + 1)] = False
    interior = flux[mask]
    seam_flux = np.concatenate(picks) if picks else np.array([])
    # 样本级最大跳变（相位咔哒代理），接缝 ±20 ms 对窗内。
    d = np.abs(np.diff(x48k))
    seam_d, interior_d = [], []
    for c_s in seams_s:
        c = int(c_s * sr)
        seam_d.append(d[max(0, c - int(0.02 * sr)):c + int(0.02 * sr)])
    dm = np.ones(len(d), dtype=bool)
    for c_s in seams_s:
        c = int(c_s * sr)
        dm[max(0, c - int(0.3 * sr)):c + int(0.3 * sr)] = False
    dm[:int(0.5 * sr)] = False
    dm[len(d) - int(0.5 * sr):] = False
    interior_d = d[dm]
    out = {
        "seams_s": seams_s,
        "flux_interior_median": float(np.median(interior)) if interior.size else 0.0,
        "flux_seam_mean": float(seam_flux.mean()) if seam_flux.size else 0.0,
        "flux_seam_max": float(seam_flux.max()) if seam_flux.size else 0.0,
        "jump_interior_p99": float(np.percentile(interior_d, 99)) if interior_d.size else 0.0,
        "jump_seam_max": float(max(a.max() for a in seam_d)) if seam_d else 0.0,
    }
    if out["flux_interior_median"] > 0:
        out["flux_seam_ratio"] = out["flux_seam_mean"] / out["flux_interior_median"]
    # 输入同位置对照：自然语音起点在输入输出都出现，只有输出升高的才是接缝伪影。
    if flux_in is not None and seam_idx:
        ni = len(flux_in)
        mask_in = np.ones(ni, dtype=bool)
        mask_in[:int(round(500.0 / hop_ms))] = False
        mask_in[ni - int(round(500.0 / hop_ms)):] = False
        picks_in = []
        for c in seam_idx:
            ci = int(round(c * 48000 / sr))  # 输入按 48 kHz 重采样后同网格
            lo, hi = max(0, ci - half), min(ni, ci + half + 1)
            picks_in.append(flux_in[lo:hi])
            mask_in[max(0, ci - guard):min(ni, ci + guard + 1)] = False
        interior_in = flux_in[mask_in]
        seam_in = np.concatenate(picks_in)
        med_in = float(np.median(interior_in)) if interior_in.size else 0.0
        out["flux_input_interior_median"] = med_in
        out["flux_input_seam_mean"] = float(seam_in.mean()) if seam_in.size else 0.0
        if med_in > 0:
            out["flux_input_seam_ratio"] = out["flux_input_seam_mean"] / med_in
            if out["flux_input_seam_ratio"] > 0:
                out["seam_artifact_ratio"] = (
                    out.get("flux_seam_ratio", 0.0) / out["flux_input_seam_ratio"])
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--input", required=True)
    ap.add_argument("--output", required=True)
    ap.add_argument("--hop-s", type=float, required=True)
    ap.add_argument("--crossfade-s", type=float, default=0.2)
    ap.add_argument("--windows", type=int, required=True)
    ap.add_argument("--label", required=True)
    ap.add_argument("--json", required=True)
    args = ap.parse_args()

    xin = load_mono(args.input, None)
    sr_in = librosa.get_samplerate(args.input)
    xout = load_mono(args.output, 48000)
    xout16 = load_mono(args.output, 16000)
    xin16 = load_mono(args.input, 16000)

    seams = [k * args.hop_s + args.crossfade_s / 2
             for k in range(1, args.windows)]
    seams = [t for t in seams if t < len(xout) / 48000 - 0.3]

    f0_in, _ = f0_cents(xin16)
    f0_out, _ = f0_cents(xout16)
    flux = spectral_flux(xout)
    flux_in = spectral_flux(load_mono(args.input, 48000))

    spec = librosa.feature.spectral_flatness(y=xout, n_fft=2048, hop_length=512)[0]
    freqs = np.fft.rfftfreq(2048, 1 / 48000)
    ps = np.abs(librosa.stft(xout, n_fft=2048, hop_length=512)) ** 2
    hf = ps[(freqs >= 4000) & (freqs <= 16000)].sum() / max(1e-12, ps.sum())

    metrics = {
        "label": args.label,
        "input": args.input,
        "output": args.output,
        "input_sr": sr_in,
        "input_duration_s": len(xin) / sr_in,
        "output_duration_s": len(xout) / 48000,
        "output_samples": len(xout),
        "rms": float(np.sqrt(np.mean(xout ** 2))),
        "peak": float(np.max(np.abs(xout))),
        "clip_ratio": float(np.mean(np.abs(xout) >= 0.99)),
        "hf_ratio_4_16k": float(hf),
        "flatness_median": float(np.median(spec)),
        "alignment": align_and_diff(f0_in, f0_out),
        "seams": seam_metrics(xout, 48000, seams, flux, flux_in),
    }
    with open(args.json, "w") as f:
        json.dump(metrics, f, ensure_ascii=False, indent=2)
    print(json.dumps(metrics, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
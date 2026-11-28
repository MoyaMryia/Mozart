#!/usr/bin/env python3
"""Golden 对拍门：候选（板端实时回录）对参考（本机离线 L2 链 golden）。

对齐策略：RMS 包络粗对齐（±6 s，含回录预卷与启动缓冲）→ mel 相关细对齐
（±40 帧，宽于 compare_streaming_quality 的 ±20 帧）。输出整体指标与逐秒
cents/mel 分布，用于定位偏差段。
用法：gate_compare.py <reference.wav> <candidate.wav> [--table]
"""
import argparse
import contextlib
import io
import json
import sys
from pathlib import Path

import numpy as np
import soundfile as sf
from scipy.signal import fftconvolve

sys.path.insert(0, str(Path(__file__).resolve().parent))
from compare_streaming_quality import compare, extract_features, load_audio  # noqa: E402

SR = 16000


def coarse_lag(ref, cand, max_lag_s=6.0):
    w = SR // 10
    er = np.sqrt((ref[:len(ref)//w*w].reshape(-1, w)**2).mean(axis=1))
    ec = np.sqrt((cand[:len(cand)//w*w].reshape(-1, w)**2).mean(axis=1))
    er = er - er.mean(); ec = ec - ec.mean()
    corr = fftconvolve(ec, er[::-1], mode='full')
    lo = max(0, len(er) - 3)
    hi = min(len(corr), len(er) - 1 + int(max_lag_s * 10))
    i = lo + int(np.argmax(corr[lo:hi]))
    return (i - (len(er) - 1)) * w


def fine_lag(mel_r, mel_c, lag0, span=40):
    n = min(mel_r.shape[1], mel_c.shape[1])
    best = None
    for lag in range(lag0 - span, lag0 + span + 1):
        rs = slice(max(0, lag), max(0, lag) + n - abs(lag))
        cs = slice(max(0, -lag), max(0, -lag) + n - abs(lag))
        m = min(rs.stop - rs.start, cs.stop - cs.start, n)
        a = mel_r[:, rs.start:rs.start+m].ravel()
        b = mel_c[:, cs.start:cs.start+m].ravel()
        if a.size < 2:
            continue
        c = np.corrcoef(a, b)[0, 1]
        if best is None or c > best[0]:
            best = c, lag, m
    return best


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('reference')
    p.add_argument('candidate')
    p.add_argument('--stream-hop', type=int, default=3840)
    p.add_argument('--table', action='store_true', help='逐秒 cents/mel 分布')
    args = p.parse_args()

    ref = load_audio(args.reference)
    cand = load_audio(args.candidate)
    lag0 = coarse_lag(ref, cand)
    trimmed = cand[max(lag0, 0):max(lag0, 0) + len(ref)]
    tmp = Path('/tmp/gate_aligned.wav')
    sf.write(tmp, trimmed, SR, subtype='PCM_16')

    ref_f = extract_features(ref)
    cand_f = extract_features(trimmed)
    mel_r, pr, rms_r = ref_f[0], ref_f[3], ref_f[1]
    mel_c, pc, rms_c = cand_f[0], cand_f[3], cand_f[1]
    corr, lag, m = fine_lag(mel_r, mel_c, 0)
    rs = slice(max(0, lag), max(0, lag) + m)
    cs = slice(max(0, -lag), max(0, -lag) + m)
    mae = float(np.abs(mel_r[:, rs] - mel_c[:, cs]).mean())
    both = np.isfinite(pr[rs]) & np.isfinite(pc[cs])
    cents = 1200 * np.log2(pr[rs][both] / pc[cs][both])
    rms_corr = float(np.corrcoef(rms_r[rs], rms_c[cs])[0, 1])
    print(f'粗对齐 {lag0/16000:+.2f}s 细对齐 {lag*10:+d}ms')
    print(json.dumps({
        'logmel_mae_db': round(mae, 2),
        'rms_envelope_corr': round(rms_corr, 3),
        'cents_abs_median': round(float(np.median(np.abs(cents))), 1),
        'cents_signed_median': round(float(np.median(cents)), 1),
        'cents_p90_abs': round(float(np.percentile(np.abs(cents), 90)), 1),
        'voiced_frames': int(both.sum()),
    }, ensure_ascii=False))

    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        result = compare(args.reference, str(tmp), stream_hop=args.stream_hop)
    r = result if isinstance(result, dict) else {}
    print('compare_streaming_quality:', json.dumps(
        {k: r.get(k) for k in ('logmel_corr', 'logmel_mae_db', 'rms_envelope_corr',
                               'rms_ratio_db', 'f0_median_cents', 'voiced_iou',
                               'all_spectral_step_mean_db', 'alignment_lag_ms')},
        ensure_ascii=False))

    if args.table:
        n = min(mel_r.shape[1], mel_c.shape[1])
        seg = 100
        both_n = np.isfinite(pr[:n]) & np.isfinite(pc[:n])
        cents_n = np.where(both_n, 1200*np.log2(
            np.where(both_n, pr[:n], 1)/np.where(both_n, pc[:n], 1)), np.nan)
        mae_n = np.abs(mel_r[:, :n] - mel_c[:, :n]).mean(axis=0)
        for s in range(0, n // seg):
            v = cents_n[s*seg:(s+1)*seg]
            v = v[np.isfinite(v)]
            ms = mae_n[s*seg:(s+1)*seg].mean()
            if len(v) < 10:
                print(f'  t={s:3d}s (少有声帧) mel={ms:5.1f}dB')
                continue
            print(f'  t={s:3d}s cents_med={np.median(v):+7.0f} |abs|='
                  f'{np.median(np.abs(v)):5.0f} mel={ms:5.1f}dB')


if __name__ == '__main__':
    main()

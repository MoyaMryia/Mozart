#!/usr/bin/env python3
"""Task 7 实测证据分析器：对 tools/jetson-measure.sh 产物出数字。

用法：
  analyze_measure.py floor <dir>...            地板校准：各 run 点击偏移与抖动
  analyze_measure.py latency <dir> <floor_ms>  脉冲串延迟：分段顺序配对，direct/rvc 分段统计
  analyze_measure.py fade <dir>                淡变：分段稳态跳变基线 + 切换窗跳变
  analyze_measure.py longrun <dir>             长跑：计数 1:1、掉帧、GPU 负载
  analyze_measure.py voice <dir>               真语音：分段频谱与活动/低能量对比
  analyze_measure.py heal <dir>                自愈：恢复事件时间线、静默时长与恢复耗时

回录坐标约定：arecord 比 aplay 提前 0.5 s 启动，墙钟时刻换算回录时间加 0.5；
地板值（含同坐标偏移）作延迟基线，net = 实测滞后 − 地板。
依赖 numpy；证据文件见各场景目录（rec.raw、status.jsonl、meta.json）。
"""
import json
import math
import sys
from pathlib import Path

import numpy as np

SR = 48000


def load_rec(path):
    raw = np.fromfile(path, dtype='<i2')
    mono = raw[0::2].astype(np.float64)
    mono += raw[1::2].astype(np.float64)
    return mono / 2.0


def load_meta(d):
    return json.loads((d / 'meta.json').read_text())


def pct(values, q):
    if not values:
        return None
    return float(np.percentile(values, q))


def stats_ms(values):
    if not values:
        return {'n': 0}
    return {
        'n': len(values),
        'min_ms': round(min(values), 2),
        'avg_ms': round(float(np.mean(values)), 2),
        'max_ms': round(max(values), 2),
        'p95_ms': round(pct(values, 95), 2),
        'values_ms': [round(v, 2) for v in values],
    }


def find_clicks(mono, threshold=2000, min_gap_s=0.15):
    """点击沿：绝对值首次越过阈值的样本，进入 150 ms 盲区。"""
    hits = []
    blind_until = 0
    over = np.abs(mono) > threshold
    i = 0
    n = len(mono)
    while i < n:
        if over[i] and i >= blind_until:
            hits.append(i)
            blind_until = i + int(SR * min_gap_s)
        i += 1
    return hits


def onset_envelope(mono, win_ms=5.0):
    """短窗 RMS 包络（窗口左沿对齐，长度与信号一致）。"""
    win = max(1, int(SR * win_ms / 1000))
    sq = mono * mono
    cs = np.concatenate(([0.0], np.cumsum(sq)))
    ms = (cs[win:] - cs[:-win]) / win
    env = np.sqrt(np.concatenate((np.zeros(win), ms)))
    return env


def detect_bursts(env, plateau, thresh_ratio=0.10, min_gap_s=0.15, min_len_ms=30):
    """包络上的起振沿：过 10% 平台阈并保持 ≥30 ms 记一次，带 150 ms 盲区。"""
    thr = plateau * thresh_ratio
    hits = []
    blind_until = 0
    n = len(env)
    i = 0
    while i < n:
        if env[i] > thr and i >= blind_until:
            hold_until = i + int(SR * min_len_ms / 1000)
            if hold_until < n and env[i:hold_until].min() > thr:
                hits.append(i)
                blind_until = i + int(SR * min_gap_s)
                i = blind_until
                continue
        i += 1
    return hits


def goertzel(x, freq, sr=SR):
    n = len(x)
    k = round(freq * n / sr)
    w = 2 * math.pi * k / n
    cw = math.cos(w)
    coeff = 2 * cw
    s1 = s2 = 0.0
    for v in x:
        s0 = v + coeff * s1 - s2
        s2 = s1
        s1 = s0
    return math.sqrt(s1 * s1 + s2 * s2 - coeff * s1 * s2)


def goertzel_batch(x, freqs, sr=SR):
    out = {}
    for f in freqs:
        out[f] = goertzel(x, f, sr)
    return out


def cmd_floor(dirs):
    for d in dirs:
        d = Path(d)
        # 优先整体解析（heredoc 可多行）；历史产物曾按行追加多个 JSON 对象，
        # 整体失败时退回按行合并。
        text = (d / 'meta.json').read_text()
        try:
            meta = json.loads(text)
        except json.JSONDecodeError:
            meta = {}
            for line in text.splitlines():
                line = line.strip()
                if line:
                    try:
                        meta.update(json.loads(line))
                    except json.JSONDecodeError:
                        pass
        mono = load_rec(d / 'rec.raw')
        clicks_ms = meta['clicks_ms']
        found = find_clicks(mono)
        offs = []
        for i, c in enumerate(clicks_ms):
            if i < len(found):
                offs.append((found[i] - int(c / 1000 * SR)) / SR * 1000)
        result = {
            'dir': str(d),
            'writer_period': meta['writer_period'],
            'reader_period': meta['reader_period'],
            'sub': meta['sub'],
            'n_expected': len(clicks_ms),
            'n_found': len(found),
            **stats_ms(offs),
        }
        print(json.dumps(result, ensure_ascii=False, indent=1))


def load_switches(d):
    switches = []
    for line in (d / 'switches.jsonl').read_text().splitlines():
        if line.strip():
            switches.append(json.loads(line))
    return switches


def cmd_latency(d, floor_ms):
    d = Path(d)
    meta = load_meta(d)
    mono = load_rec(d / 'rec.raw')
    env = onset_envelope(mono)
    plateau = float(np.percentile(env, 99.5))
    switches = load_switches(d)
    # 切换命令完成时刻（wall clock）→ 回录坐标：arecord 比 aplay 提前 0.5 s。
    sw = {s['switch']: (s['at_epoch_ms'] - meta['aplay_epoch_ms']) / 1000 + 0.5
          for s in switches}
    end_s = len(mono) / SR
    direct_all = meta['direct_bursts_s']
    # 分段顺序配对：rvc 滞后（约 0.7 s）与脉冲间距同量级，固定窗口首沿
    # 匹配会把前一脉冲的起振沿配给后一脉冲；按段内起振沿出现顺序与
    # 脉冲顺序一一对应。最后一段末尾脉冲可能被路由切回截断，如数报出。
    sections = [
        ('direct1', direct_all[:8], 0.5, sw['rvc-cold']),
        ('rvc_cold', meta['rvc_cold_bursts_s'], sw['rvc-cold'], sw['direct']),
        ('direct2', direct_all[8:], sw['direct'], sw['rvc-hot']),
        ('rvc_hot', meta['rvc_hot_bursts_s'], sw['rvc-hot'], end_s),
    ]
    out = {'floor_ms': floor_ms, 'plateau_rms': round(plateau, 1)}
    for name, bursts, t0, t1 in sections:
        lo, hi = int((t0 + 0.25) * SR), min(len(env), int(t1 * SR))
        onsets = [lo + o for o in detect_bursts(env[lo:hi], plateau, min_gap_s=0.4)]
        if name.startswith('rvc'):
            # 热切换后新流的第一个有声窗会回放切换前音频的残尾（实测滞后约
            # 1.26 s），早于本段首脉冲的任何回显（最小 0.5 arecord + 0.4 启动
            # 缓冲 + 地板），只保留段内首脉冲可能回显时刻之后的沿。
            onsets = [o for o in onsets if o >= (bursts[0] + 0.85) * SR]
        offs = [(onsets[i] - b * SR) / SR * 1000
                for i, b in enumerate(bursts) if i < len(onsets)]
        result = stats_ms(offs) if offs else {'n': 0}
        if offs:
            net = {k: round(v - floor_ms, 2) for k, v in result.items()
                   if k.endswith('_ms') and k != 'values_ms'}
            net['n'] = result['n']
            net['values_ms'] = [round(v - floor_ms, 2) for v in result['values_ms']]
            result['net_ms'] = net
        result['n_bursts'] = len(bursts)
        result['n_onsets'] = len(onsets)
        result['unpaired_bursts_s'] = bursts[len(onsets):]
        out[name] = result
    out['switches'] = switches
    print(json.dumps(out, ensure_ascii=False, indent=1))


def cmd_fade(d):
    d = Path(d)
    meta = load_meta(d)
    mono = load_rec(d / 'rec.raw')
    jumps = np.abs(np.diff(mono))
    # 稳态 220 Hz @ amp≈24000 的理论相邻样本最大跳变
    steady_max_theory = 24000 * 2 * math.pi * 220 / SR
    # 切换音频伪影先于 ctl 命令返回：计划时刻 = 命令完成时刻 − 命令耗时。
    # 冷切换含 LoadModel+StartStream（约 0.65 s），窗取计划时刻起
    # [-0.1, +0.95]；direct/rvc-hot 切换为即发即回，取 [-0.1, +0.3]。
    sw_sched = {s['switch']: (s['at_epoch_ms'] - s.get('cmd_wall_ms', 0)
                              - meta['aplay_epoch_ms']) / 1000
                for s in load_switches(d)}
    win = {'rvc-cold': (-0.1, 0.95), 'direct': (-0.1, 0.3), 'rvc-hot': (-0.1, 0.3)}
    def seg_max(t0, t1):
        lo, hi = max(0, int(t0 * SR)), min(len(jumps), int(t1 * SR))
        return round(float(jumps[lo:hi].max()), 1) if hi > lo else None
    report = {
        'steady_max_theory_direct': round(steady_max_theory, 1),
        'overall_max_jump': round(float(jumps.max()), 1),
        'overall_p9999_jump': round(float(np.percentile(jumps, 99.99)), 1),
    }
    # 稳态段界：各切换窗之间（回录坐标 = 计划时刻 + 0.5 s arecord 预录偏移）
    names = ['direct1', 'rvc1', 'direct2', 'rvc2']
    keys = ['rvc-cold', 'direct', 'rvc-hot']
    bounds = [0.5] + [sw_sched[k] + 0.5 for k in keys] + [len(mono) / SR]
    for i, name in enumerate(names):
        report[f'steady_{name}_max_jump'] = seg_max(bounds[i] + 1.0, bounds[i + 1] - 0.4)
    for s in load_switches(d):
        t = sw_sched[s['switch']] + 0.5
        lo_off, hi_off = win[s['switch']]
        report.setdefault('switch_windows', []).append({
            'switch': s['switch'],
            'sched_rec_s': round(t, 3),
            'cmd_wall_ms': s.get('cmd_wall_ms'),
            'win_max_jump': seg_max(t + lo_off, t + hi_off),
        })
    top = np.argsort(jumps)[-10:][::-1]
    report['top10'] = [
        {'at_rec_s': round(int(i) / SR, 4), 'jump': round(float(jumps[i]), 1)}
        for i in top
    ]
    print(json.dumps(report, ensure_ascii=False, indent=1))


def load_jsonl_objects(path):
    """解析 poll_status 产物：ctl --json 输出 pretty-printed 多行，行界不可靠。
    文件内容形如 {"epoch_ms":N},"status":{...} 的重复序列（shell 变量拼接时
    前段与 status 体分开成两个顶层值），用 raw_decode 顺序抽取后按
    [epoch, "status", status] 三元模式重组成完整记录。"""
    text = path.read_text()
    decoder = json.JSONDecoder()
    raw = []
    idx = 0
    n = len(text)
    while idx < n:
        while idx < n and text[idx].isspace():
            idx += 1
        if idx >= n:
            break
        try:
            obj, end = decoder.raw_decode(text, idx)
            raw.append(obj)
            idx = end
        except json.JSONDecodeError:
            idx += 1
    items = []
    i = 0
    while i < len(raw):
        if (isinstance(raw[i], dict) and 'epoch_ms' in raw[i]
                and i + 2 < len(raw) and raw[i + 1] == 'status'
                and isinstance(raw[i + 2], dict)):
            rec = raw[i]
            rec['status'] = raw[i + 2]
            items.append(rec)
            i += 3
        else:
            i += 1
    return items


def cmd_longrun(d):
    d = Path(d)
    meta = load_meta(d)
    mono = load_rec(d / 'rec.raw')
    env = onset_envelope(mono, win_ms=10.0)
    plateau = float(np.percentile(env, 90))
    # 掉帧检测：包络 < 10% 平台并持续 > 20 ms
    low = env < plateau * 0.10
    gaps = []
    i = 0
    n = len(low)
    while i < n:
        if low[i]:
            j = i
            while j < n and low[j]:
                j += 1
            if (j - i) / SR > 0.02:
                gaps.append({'from_s': round(i / SR, 3), 'len_ms': round((j - i) / SR * 1000, 1)})
            i = j
        else:
            i += 1
    polls = load_jsonl_objects(d / 'status.jsonl')
    rows = []
    for p in polls:
        s = p.get('status')
        if not s:
            continue
        pre = s.get('pre') or {}
        rvc = s.get('rvc') or {}
        hw = (s.get('hw') or {}).get('gpu') or {}
        rows.append({
            't_s': round((p['epoch_ms'] - meta['aplay_epoch_ms']) / 1000, 1),
            'route': s.get('route', {}).get('active'),
            'pre_cap': pre.get('frames_captured'),
            'pre_play': pre.get('frames_played'),
            'pre_drop': pre.get('frames_dropped'),
            'pre_q': pre.get('queue_depth'),
            'pre_cdrop_rvc': (pre.get('consumer_drops') or {}).get('rvc'),
            'pre_err': pre.get('last_error'),
            'rvc_in': rvc.get('frames_in'),
            'rvc_out': rvc.get('frames_out'),
            'rvc_drop': rvc.get('frames_dropped'),
            'rvc_blocks': rvc.get('blocks'),
            'inf_last': rvc.get('inference_ms_last'),
            'inf_avg': rvc.get('inference_ms_avg'),
            'inf_p95': rvc.get('inference_ms_p95'),
            'rvc_q': rvc.get('queue_depth'),
            'rvc_state': rvc.get('state'),
            'gpu_load': hw.get('load_percent'),
            'gpu_mem_used_bytes': hw.get('memory_used_bytes'),
        })
    steady = [r for r in rows if r['t_s'] >= 5 and r['rvc_state'] == 'streaming']
    inf = [r['inf_avg'] for r in steady if r['inf_avg'] is not None]
    gpu = [r['gpu_load'] for r in steady if r['gpu_load'] is not None]
    mem = [r['gpu_mem_used_bytes'] for r in steady if r['gpu_mem_used_bytes'] is not None]
    if steady:
        first, last = steady[0], steady[-1]
        dt_s = last['t_s'] - first['t_s']
        cap_rate = (last['pre_cap'] - first['pre_cap']) / dt_s if dt_s else None
        in_rate = (last['rvc_in'] - first['rvc_in']) / dt_s if dt_s else None
    else:
        first = last = None
        cap_rate = in_rate = None
    nz_gpu = [g for g in gpu if g > 0]
    summary = {
        'duration_s': meta.get('duration_s'),
        'rec_len_s': round(len(mono) / SR, 2),
        'dropout_gaps': gaps,
        'steady_window_s': [first['t_s'], last['t_s']] if steady else None,
        'n_steady_polls': len(steady),
        'frames_captured_rate_per_s': round(cap_rate, 2) if cap_rate else None,
        'rvc_frames_in_rate_per_s': round(in_rate, 2) if in_rate else None,
        'pre_drop_delta': (last['pre_drop'] - first['pre_drop']) if steady else None,
        'pre_cdrop_rvc_delta': (last['pre_cdrop_rvc'] - first['pre_cdrop_rvc']) if steady else None,
        'rvc_drop_delta': (last['rvc_drop'] - first['rvc_drop']) if steady else None,
        'pre_queue_max': max((r['pre_q'] or 0) for r in rows),
        'rvc_queue_max': max((r['rvc_q'] or 0) for r in rows),
        'inference_avg_of_avgs_ms': round(float(np.mean(inf)), 1) if inf else None,
        'inference_p95_max_ms': round(max(r['inf_p95'] for r in steady if r['inf_p95'] is not None), 1) if steady else None,
        'gpu_load_samples': [round(g, 1) for g in gpu],
        'gpu_load_avg': round(float(np.mean(gpu)), 1) if gpu else None,
        'gpu_load_min_max': [round(min(gpu), 1), round(max(gpu), 1)] if gpu else None,
        # sysfs GPU 负载窗与轮询周期错拍时采样呈 0/满载交替；非零样本均值才是负载口径。
        'gpu_load_nonzero': {'n': len(nz_gpu), 'avg': round(float(np.mean(nz_gpu)), 1),
                              'min_max': [round(min(nz_gpu), 1), round(max(nz_gpu), 1)]} if nz_gpu else None,
        'gpu_mem_used_mib_range': [round(min(mem) / 2**20, 1), round(max(mem) / 2**20, 1)] if mem else None,
        'pre_last_errors': sorted({r['pre_err'] for r in rows if r['pre_err']}),
    }
    print(json.dumps(summary, ensure_ascii=False, indent=1))
    print('--- rows ---')
    for r in rows:
        print(json.dumps(r, ensure_ascii=False))


def spectrum_summary(x, freqs):
    g = goertzel_batch(x, freqs)
    rms = float(np.sqrt(np.mean(x * x)))
    peak = float(np.max(np.abs(x)))
    return {'rms': round(rms, 1), 'peak': round(peak, 1),
            'goertzel': {str(f): round(v, 1) for f, v in g.items()}}


def activity_summary(x):
    """活动跨度与低能量时长：direct 与 rvc 两段对比，
    rvc 段明显多于 direct 的低能量时间即掉帧证据（语音自然停顿两侧同量）。"""
    env = onset_envelope(x, win_ms=20)
    med = float(np.percentile(env, 75))
    active = env > med * 0.10
    idx = np.where(active)[0]
    if len(idx) == 0:
        return {'active_s': 0.0, 'low_energy_s': round(len(x) / SR, 2)}
    low_s = float(len(x) - np.count_nonzero(active)) / SR
    return {
        'active_s': round((idx[-1] - idx[0]) / SR, 2),
        'low_energy_s': round(low_s, 2),
        'internal_low_energy_s': round(
            float(len(idx) - np.count_nonzero(active[idx[0]:idx[-1] + 1])) / SR, 2),
    }


def cmd_voice(d):
    d = Path(d)
    meta = load_meta(d)
    mono = load_rec(d / 'rec.raw')
    sig_len = int(meta['signal_s'] * SR)
    head = int(0.5 * SR)
    # 第一段 direct：arecord 头 0.5 s + 信号段；第二段 rvc：从第二段 aplay 前后推算。
    # 用包络找两次语音活动的起止，按能量粗分两段。
    env = onset_envelope(mono, win_ms=20)
    active = env > np.percentile(env, 50) * 0.2
    # 找活动块
    blocks = []
    i = 0
    n = len(active)
    while i < n:
        if active[i]:
            j = i
            while j < n and (active[j] or (j + int(0.8 * SR) < n and active[j:j + int(0.8 * SR)].any())):
                j += 1
            blocks.append((i, j))
            i = j
        else:
            i += 1
    # 合并间隔 <2 s 的块
    merged = []
    for b in blocks:
        if merged and (b[0] - merged[-1][1]) < 2 * SR:
            merged[-1] = (merged[-1][0], b[1])
        else:
            merged.append(b)
    freqs = [220, 300, 500, 800, 1000, 1500, 2000, 3000]
    out = {'blocks': [{'from_s': round(a / SR, 2), 'to_s': round(b / SR, 2)} for a, b in merged]}
    for k, (a, b) in enumerate(merged):
        out[f'block{k}'] = spectrum_summary(mono[a:b], freqs)
        out[f'block{k}']['activity'] = activity_summary(mono[a:b])
    print(json.dumps(out, ensure_ascii=False, indent=1))


def cmd_heal(d):
    d = Path(d)
    meta = load_meta(d)
    who = meta['who']
    kill_ms = meta['kill_epoch_ms']
    aplay0 = meta['aplay_epoch_ms']
    polls = load_jsonl_objects(d / 'status.jsonl')
    kill_i = next((i for i, p in enumerate(polls) if p['epoch_ms'] >= kill_ms), None)
    events = []
    recovered_pid = None
    restored = None
    for p in polls[kill_i or 0:]:
        s = p.get('status')
        if not s:
            continue
        dt = round((p['epoch_ms'] - kill_ms) / 1000, 2)
        proc = (s.get('processes') or {}).get(who) or {}
        pre = s.get('pre') or {}
        rvc = s.get('rvc') or {}
        pid = proc.get('pid')
        state = proc.get('state')
        if recovered_pid is None and pid is not None and pid > 0 and state == 'running' and (proc.get('restarts') or 0) >= 1:
            recovered_pid = pid
            events.append({'dt_s': dt, 'event': 'process_restarted', 'pid': pid})
        if who == 'rvc' and restored is None and recovered_pid and rvc.get('state') == 'streaming':
            restored = rvc.get('state')
            events.append({'dt_s': dt, 'event': 'rvc_streaming_restored',
                           'frames_in': rvc.get('frames_in'), 'frames_out': rvc.get('frames_out')})
        if who == 'pre' and restored is None and recovered_pid and pre.get('route') == 'rvc':
            restored = pre.get('route')
            events.append({'dt_s': dt, 'event': 'pre_route_restored'})
    # 回录静默段与恢复点。基线取 kill 前的稳态电平：heal 全程走 rvc 路由，
    # kill 后长静默会把全段分位数拉低，必须以 kill 前窗口取阈值。
    mono = load_rec(d / 'rec.raw')
    env = onset_envelope(mono, win_ms=10)
    kill_rec_s = (kill_ms - aplay0) / 1000 + 0.5
    lo = int(kill_rec_s * SR)
    base = env[int(0.6 * SR):max(int(0.6 * SR) + 1, lo - int(0.2 * SR))]
    plateau = float(np.percentile(base, 90)) if len(base) else float(np.percentile(env, 90))
    low = env[lo:lo + int(24 * SR)] < plateau * 0.10
    # heal pre 场景 kill 后重供信号：重供时刻（回录坐标）。
    resupply_rec_s = ((meta['aplay2_epoch_ms'] - aplay0) / 1000 + 0.5
                      if meta.get('aplay2_epoch_ms') else None)
    gaps = []
    gap_ends = []
    i = 0
    while i < len(low):
        if low[i]:
            j = i
            while j < len(low) and low[j]:
                j += 1
            if (j - i) / SR > 0.05:
                gaps.append({'from_s': round((lo + i) / SR, 2),
                             'len_ms': round((j - i) / SR * 1000, 1)})
                # 只到扫描窗尾仍未回升的段没有恢复点，不能当音频上升沿。
                if j < len(low):
                    gap_ends.append(lo + j)
            i = j
        else:
            i += 1
    # 恢复点：heal pre 场景 kill 后先排空链内残留音频再重供信号，恢复以
    # 重供时刻后的首个音频上升为准；无重供时取 kill 后第一个静默段结束。
    bound = resupply_rec_s if resupply_rec_s is not None else kill_rec_s
    resumes = [e for e in gap_ends if e / SR >= bound]
    resume_s = resumes[0] if resumes else None
    final = json.loads((d / 'st-final.json').read_text())
    result = {
        'who': who,
        'kill_at_s_rel_aplay': round((kill_ms - aplay0) / 1000, 2),
        'kill_rec_s': round(kill_rec_s, 2),
        'events': events,
        'silence_gaps_after_kill': gaps,
        'audio_gap_ms': round((resume_s / SR - kill_rec_s) * 1000, 1) if resume_s else None,
        'final_restarts': (final.get('processes') or {}).get(who, {}).get('restarts'),
        'final_route': (final.get('route') or {}).get('active'),
        'final_rvc_state': (final.get('rvc') or {}).get('state'),
        'final_rvc_frames': [ (final.get('rvc') or {}).get('frames_in'), (final.get('rvc') or {}).get('frames_out'),
                               (final.get('rvc') or {}).get('frames_dropped')],
        'final_pre_frames': [ (final.get('pre') or {}).get('frames_captured'), (final.get('pre') or {}).get('frames_played'),
                               (final.get('pre') or {}).get('frames_dropped')],
        'final_pre_cdrop_rvc': (final.get('pre') or {}).get('consumer_drops', {}).get('rvc'),
        'final_pre_skipped_rvc': (final.get('pre') or {}).get('consumer_drops', {}).get('rvc.skipped'),
        'final_degraded': final.get('degraded'),
    }
    if resupply_rec_s is not None:
        # heal pre：snd-aloop 断缆后重供信号，恢复耗时从信号重供时刻起算。
        result['aplay_survived_kill'] = meta.get('aplay_survived_kill')
        result['signal_resupply_rec_s'] = round(resupply_rec_s, 2)
        if resume_s:
            result['chain_recovery_after_resupply_ms'] = round(
                (resume_s / SR - resupply_rec_s) * 1000, 1)
    print(json.dumps(result, ensure_ascii=False, indent=1))


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    cmd = sys.argv[1]
    if cmd == 'floor':
        cmd_floor(sys.argv[2:])
    elif cmd == 'latency':
        cmd_latency(sys.argv[2], float(sys.argv[3]))
    elif cmd == 'fade':
        cmd_fade(sys.argv[2])
    elif cmd == 'longrun':
        cmd_longrun(sys.argv[2])
    elif cmd == 'voice':
        cmd_voice(sys.argv[2])
    elif cmd == 'heal':
        cmd_heal(sys.argv[2])
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main())

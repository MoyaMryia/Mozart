#!/usr/bin/env python3
"""Mozart 语言链路自动化验收核验工具。

检查单轮（及可选多轮重放）语言链路产物目录中的字幕存档、终态状态快照与
播报回录音频，输出完整核心指标并按 smoke / strict 两种级别执行门限核验。

产物目录约定：
- subtitles.jsonl：桥落盘字幕存档
- st-final.json：排空后的 daemon 状态快照
- rec.raw（或可选 rec.wav）：48 kHz S16_LE 播报回录
- speech.wav（可选）：输入语音样本，用于参考统计输入自然静音

退出码：
- 0：当前模式下的全部硬门限 PASS
- 1：至少一项硬门限 FAIL
- 2：参数非法、缺少必需文件或输入格式损坏
"""

from __future__ import annotations

import argparse
from array import array
import json
import math
import operator
from pathlib import Path
import re
import sys
import tempfile
from typing import Any
import unicodedata
import wave

try:
    import numpy as np  # type: ignore[import-not-found]
except ImportError:
    np = None

# 常见未完结句尾词（去除末尾标点后判定腰斩切句）
UNFINISHED_TAILS = (
    "因为", "但是", "所以", "然后", "就是", "这个", "那个", "如果", "虽然",
    "而且", "或者", "还是", "其实", "觉得", "时候", "之后", "之前", "的话",
    "为了", "关于", "对于", "通过", "开始", "一直", "可能", "应该", "需要",
    "没有", "不是", "已经", "正在", "可以", "就是说", "怎么", "什么",
    "的", "了", "着", "和", "跟", "把", "被", "给", "在", "对", "向", "从",
)

# 受监管语言链路进程角色
MONITORED_ROLES = ("pre", "rvc", "asr", "tts", "bridge", "llama")

# 音频回录与活动段检测常量（与 Mozart pre 48 kHz 立体声 S16_LE 契约对齐）
RAW_SAMPLE_RATE = 48000
RAW_CHANNELS = 2
SPAN_FRAME_MS = 20
PLAYBACK_RMS_THRESHOLD = 0.008
PLAYBACK_MERGE_GAP_MS = 220
INPUT_RMS_THRESHOLD = 0.010
INPUT_MERGE_GAP_MS = 100

# 断句与停顿统计口径常量
SHORT_FRAG_MAX_DUR_S = 1.0
SHORT_FRAG_MAX_CHARS = 4
LONG_UTT_DUR_S = 10.0
GAP_WARN_S = 1.5
GAP_SEVERE_S = 3.0
# 单一来源：bridge/src/subtitles.rs 中的 REVIEW_TIMEOUT 常量
REFINEMENT_TIMEOUT_ERROR = "refinement timeout"

# 唯一验收门限定义（对应 SPEC-05 步骤 1–4，不在他处重复硬编码）
THRESHOLDS: dict[str, Any] = {
    "max_process_restarts": 0,
    "min_asr_finals": 1,
    "min_translated": 1,
    "min_tts_synthesized": 1,
    "max_utt_dur_s": 13.5,
    "max_short_frag_rate": 0.15,
    "max_unfinished_tail_rate": 0.15,
    "max_under_split_over_10s_rate": 0.15,
    "max_refinement_timeouts": 0,
    "max_review_skip_rate": 0.02,
    "max_review_fail_rate": 0.0,
    "max_qc_reject_rate": 0.05,
    "max_tts_skipped": 0,
    "max_tts_failed": 0,
    "max_silent_drop_rate": 0.05,
    "max_final_to_mt_p50_ms": 1500.0,
    "max_final_to_mt_p95_ms": 2500.0,
    "min_playback_rms": 0.01,
    "max_abnormal_gap_s": 2.5,
    "max_abnormal_gaps_gt_1p5s_rate": 0.10,
    "max_repeat_count_cv": 0.08,
    "min_repeat_boundary_iou": 0.85,
}


class InputError(ValueError):
    """输入目录缺失或产物格式非法。"""


def _strict_int(mapping: dict[str, Any] | None, key: str) -> int | None:
    if not isinstance(mapping, dict) or key not in mapping:
        return None
    val = mapping[key]
    if isinstance(val, bool) or not isinstance(val, int):
        return None
    return val


def percentile(values: list[float], q: float) -> float:
    ordered = sorted(values)
    n = len(ordered)
    if n == 1:
        return float(ordered[0])
    pos = (n - 1) * (q / 100.0)
    lo = math.floor(pos)
    hi = math.ceil(pos)
    if lo == hi:
        return float(ordered[lo])
    frac = pos - lo
    return float(ordered[lo] + (ordered[hi] - ordered[lo]) * frac)


def median(values: list[float]) -> float:
    return percentile(values, 50.0)


def pop_std(values: list[float]) -> float:
    mean_v = sum(values) / len(values)
    var = sum((v - mean_v) ** 2 for v in values) / len(values)
    return math.sqrt(var)


def clean_visible_text(text: str) -> str:
    return "".join(c for c in text if unicodedata.category(c) not in ("Cf", "Cc")).strip()


def visible_char_count(text: str) -> int:
    clean = clean_visible_text(text)
    return sum(1 for c in clean if not c.isspace() and unicodedata.category(c)[0] != "P")


def load_subtitles(path: Path) -> tuple[list[dict[str, Any]], list[str]]:
    if not path.is_file():
        raise InputError(f"缺少字幕文件：{path}")
    utts: dict[str, dict[str, Any]] = {}
    order: list[str] = []
    startup_ids: set[str] = set()
    content = path.read_text(encoding="utf-8")
    for line_no, raw in enumerate(content.splitlines(), 1):
        raw = raw.strip()
        if not raw:
            continue
        try:
            row = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise InputError(f"字幕 JSON 解析失败 {path}:{line_no}：{exc}") from exc
        if not isinstance(row, dict) or not isinstance(row.get("utterance_id"), str) or not row["utterance_id"]:
            raise InputError(f"字幕记录缺少合法 utterance_id：{path}:{line_no}")
        uid = row["utterance_id"]
        if ":" in uid:
            prefix = uid.split(":", 1)[0].strip()
            if prefix:
                startup_ids.add(prefix)
        if uid not in utts:
            utts[uid] = {
                "uid": uid,
                "seq": row.get("seq", 0),
                "final": None,
                "latest": row,
                "latest_rev": row.get("revision", -1),
            }
            order.append(uid)
        entry = utts[uid]
        rev = row.get("revision")
        if rev is None or not isinstance(entry["latest_rev"], (int, float)) or rev >= entry["latest_rev"]:
            entry["latest"] = row
            entry["latest_rev"] = rev if rev is not None else entry["latest_rev"]
        if row.get("final") is True and entry["final"] is None:
            entry["final"] = row
    return [utts[uid] for uid in order], sorted(startup_ids)


def _analyze_pcm16_stdlib(raw_bytes: bytes, nch: int, sr: int, frame_ms: int) -> tuple[list[float], float]:
    frame_bytes = 2 * nch
    usable = (len(raw_bytes) // frame_bytes) * frame_bytes
    if usable == 0:
        return [], 0.0
    samples = array("h")
    samples.frombytes(raw_bytes[:usable])
    if sys.byteorder != "little":
        samples.byteswap()
    mono = samples[::nch] if nch > 1 else samples
    n_samples = len(mono)
    scale_sq = 32768.0 * 32768.0
    total_sq = sum(map(operator.mul, mono, mono))
    overall_rms = math.sqrt(total_sq / (n_samples * scale_sq))
    fn = int(round(sr * frame_ms / 1000.0))
    nf = n_samples // fn
    frame_rms: list[float] = []
    for idx in range(nf):
        chunk = mono[idx * fn : (idx + 1) * fn]
        sq = sum(map(operator.mul, chunk, chunk))
        frame_rms.append(math.sqrt(sq / (fn * scale_sq)))
    return frame_rms, overall_rms


def _analyze_pcm16_numpy(raw_bytes: bytes, nch: int, sr: int, frame_ms: int) -> tuple[list[float], float]:
    frame_bytes = 2 * nch
    usable = (len(raw_bytes) // frame_bytes) * frame_bytes
    if usable == 0:
        return [], 0.0
    arr = np.frombuffer(raw_bytes[:usable], dtype="<i2")
    mono = arr.reshape(-1, nch)[:, 0] if nch > 1 else arr
    mono_f64 = mono.astype(np.float64) / 32768.0
    overall_rms = float(np.sqrt(np.mean(mono_f64 * mono_f64)))
    fn = int(round(sr * frame_ms / 1000.0))
    nf = mono_f64.size // fn
    if nf == 0:
        return [], overall_rms
    frames = mono_f64[: nf * fn].reshape(nf, fn)
    frame_rms = np.sqrt(np.mean(frames * frames, axis=1)).tolist()
    return frame_rms, overall_rms


def read_pcm16_bytes(path: Path) -> tuple[bytes, int, int]:
    if path.suffix.lower() == ".raw":
        return path.read_bytes(), RAW_CHANNELS, RAW_SAMPLE_RATE
    try:
        with wave.open(str(path), "rb") as wf:
            nch = wf.getnchannels()
            sw = wf.getsampwidth()
            sr = wf.getframerate()
            nf = wf.getnframes()
            raw = wf.readframes(nf)
    except wave.Error as exc:
        raise InputError(f"WAV 文件解析失败 {path}：{exc}") from exc
    if sw != 2:
        raise InputError(f"不支持的采样位宽 {sw * 8} bit：{path}（仅支持 16 bit PCM）")
    if nch < 1 or sr < 1:
        raise InputError(f"非法的 WAV 音频参数（channels={nch}, rate={sr}）：{path}")
    return raw, nch, sr


def detect_speech_spans(
    path: Path,
    frame_ms: int = SPAN_FRAME_MS,
    rms_thr: float = PLAYBACK_RMS_THRESHOLD,
    merge_gap_ms: int = PLAYBACK_MERGE_GAP_MS,
) -> tuple[list[dict[str, float]], float]:
    raw_bytes, nch, sr = read_pcm16_bytes(path)
    analyzer = _analyze_pcm16_numpy if np is not None else _analyze_pcm16_stdlib
    frame_rms, overall_rms = analyzer(raw_bytes, nch, sr, frame_ms)
    spans: list[dict[str, float]] = []
    nf = len(frame_rms)
    i = 0
    while i < nf:
        if frame_rms[i] >= rms_thr:
            j = i
            while j < nf and frame_rms[j] >= rms_thr:
                j += 1
            s_s = i * frame_ms / 1000.0
            e_s = j * frame_ms / 1000.0
            if spans and (s_s - spans[-1]["end_s"]) * 1000.0 <= merge_gap_ms:
                spans[-1]["end_s"] = e_s
            else:
                spans.append({"start_s": s_s, "end_s": e_s})
            i = j
        else:
            i += 1
    for sp in spans:
        sp["start_s"] = round(sp["start_s"], 3)
        sp["end_s"] = round(sp["end_s"], 3)
        sp["dur_s"] = round(sp["end_s"] - sp["start_s"], 3)
    return spans, round(overall_rms, 5)


def compute_repeat_consistency(run_utt_lists: list[list[dict[str, Any]]]) -> dict[str, Any]:
    counts = [len(u) for u in run_utt_lists]
    mean_c = sum(counts) / len(counts)
    cv = pop_std([float(c) for c in counts]) / mean_c if mean_c > 0 else 1.0

    norm_spans: list[list[tuple[float, float]]] = []
    for ulist in run_utt_lists:
        pts_list: list[tuple[float, float]] = []
        for u in ulist:
            f = u["final"]
            sp = f.get("audio_start_pts_ns")
            ep = f.get("audio_end_pts_ns")
            if sp is not None and ep is not None and ep >= sp:
                pts_list.append((sp / 1e9, ep / 1e9))
        if not pts_list:
            continue
        t0 = pts_list[0][0]
        norm_spans.append([(a - t0, b - t0) for a, b in pts_list])

    ious: list[float] = []
    for i in range(len(norm_spans)):
        for j in range(i + 1, len(norm_spans)):
            for src, dst in ((norm_spans[i], norm_spans[j]), (norm_spans[j], norm_spans[i])):
                for a0, a1 in src:
                    best = 0.0
                    for b0, b1 in dst:
                        inter = max(0.0, min(a1, b1) - max(a0, b0))
                        union = max(a1, b1) - min(a0, b0)
                        if union > 0:
                            best = max(best, inter / union)
                    ious.append(best)

    return {
        "counts": counts,
        "count_cv": round(cv, 4),
        "boundary_iou_median": round(median(ious), 4) if ious else 0.0,
    }


def _load_status_json(run_dir: Path) -> dict[str, Any]:
    status_path = run_dir / "st-final.json"
    if not status_path.is_file():
        raise InputError(f"缺少终态状态文件 st-final.json：{run_dir}")
    try:
        data = json.loads(status_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise InputError(f"状态文件 JSON 解析失败 {status_path}：{exc}") from exc
    if not isinstance(data, dict):
        raise InputError(f"状态文件根节点必须为 JSON 对象：{status_path}")
    return data


def evaluate_run(
    run_dir: Path,
    mode: str = "strict",
    is_short_response_suite: bool = False,
    repeat_dirs: list[Path] | None = None,
) -> dict[str, Any]:
    if not run_dir.is_dir():
        raise InputError(f"产物目录不存在：{run_dir}")

    utts, sub_startup_ids = load_subtitles(run_dir / "subtitles.jsonl")
    status = _load_status_json(run_dir)

    # 1. 进程稳定性与模式隔离（不静默补 0，缺角色/缺字段/非 running 一律判失败）
    procs = status.get("processes") if isinstance(status.get("processes"), dict) else None
    restarts_by_role: dict[str, int] = {}
    missing_or_invalid_roles: list[str] = []
    non_running_roles: dict[str, Any] = {}
    if procs is None:
        missing_or_invalid_roles = [f"processes.{role}" for role in MONITORED_ROLES]
    else:
        for role in MONITORED_ROLES:
            info = procs.get(role)
            if not isinstance(info, dict):
                missing_or_invalid_roles.append(f"processes.{role}")
                continue
            restarts = _strict_int(info, "restarts")
            if restarts is None:
                missing_or_invalid_roles.append(f"processes.{role}.restarts")
            else:
                restarts_by_role[role] = restarts
            state_val = info.get("state")
            if state_val != "running":
                non_running_roles[role] = state_val
    total_restarts = sum(restarts_by_role.values()) if not missing_or_invalid_roles else None

    pre_st = status.get("pre") if isinstance(status.get("pre"), dict) else None
    asr_st = status.get("asr") if isinstance(status.get("asr"), dict) else None
    br_st = status.get("bridge") if isinstance(status.get("bridge"), dict) else None
    tts_st = status.get("tts") if isinstance(status.get("tts"), dict) else None
    output_mode_st = status.get("output_mode") if isinstance(status.get("output_mode"), dict) else None

    playout_policy = pre_st.get("playout_policy") if pre_st is not None else None
    output_mode_applied = output_mode_st.get("applied") if output_mode_st is not None else None
    output_mode_phase = output_mode_st.get("phase") if output_mode_st is not None else None

    asr_startup_id = str(asr_st.get("startup_id") or "").strip() if asr_st is not None else ""
    all_startup_ids = sorted(set(sub_startup_ids) | ({asr_startup_id} if asr_startup_id else set()))
    startup_id_unique = len(all_startup_ids) == 1
    startup_id_changes = max(0, len(all_startup_ids) - 1)

    # 2. 字幕与断句质量统计
    finals = [u for u in utts if u["final"] is not None]
    n_finals = len(finals)
    durs: list[float] = []
    short_frags = 0
    unfinished_cuts = 0
    over_10s = 0
    refinement_timeouts = 0
    pending_refinements = 0
    pending_translations = 0
    subtitle_bilingual_finals = 0
    final_to_mt_ms: list[float] = []
    mt_ms_list: list[float] = []

    for u in finals:
        first_final = u["final"]
        latest = u["latest"]
        zh = clean_visible_text(str(latest.get("zh", "")))
        en = clean_visible_text(str(latest.get("en", "")))
        if en:
            subtitle_bilingual_finals += 1
        ch = visible_char_count(zh)
        sp = first_final.get("audio_start_pts_ns")
        ep = first_final.get("audio_end_pts_ns")
        if sp is None or ep is None:
            sp = latest.get("audio_start_pts_ns")
            ep = latest.get("audio_end_pts_ns")
        dur_s = (ep - sp) / 1e9 if (sp is not None and ep is not None and ep >= sp) else None
        if dur_s is not None:
            durs.append(dur_s)
            if dur_s > LONG_UTT_DUR_S:
                over_10s += 1
        if (dur_s is not None and dur_s < SHORT_FRAG_MAX_DUR_S) or ch <= SHORT_FRAG_MAX_CHARS:
            short_frags += 1
        zh_nopunc = re.sub(r"[。，！？,.!?;；：:]+$", "", zh)
        if any(zh_nopunc.endswith(word) for word in UNFINISHED_TAILS):
            unfinished_cuts += 1
        if latest.get("refinement_error") == REFINEMENT_TIMEOUT_ERROR:
            refinement_timeouts += 1
        if latest.get("refinement_status") == "pending":
            pending_refinements += 1
        if latest.get("translation_status") == "pending":
            pending_translations += 1
        rec_at = first_final.get("received_at")
        if rec_at is None:
            rec_at = latest.get("received_at")
        if (
            latest.get("translation_status") == "completed"
            and latest.get("translated_at") is not None
            and rec_at is not None
        ):
            final_to_mt_ms.append((float(latest["translated_at"]) - float(rec_at)) * 1000.0)
            if latest.get("translate_ms") is not None:
                mt_ms_list.append(float(latest["translate_ms"]))

    # 3. 队列排空与丢句/质检/TTS 账本（字段缺失显式保留 None，禁止静默补 0）
    asr_finals = _strict_int(asr_st, "finals")
    bridge_translated = _strict_int(br_st, "translated")
    bridge_translate_failed = _strict_int(br_st, "translate_failed")
    bridge_speech_delivered = _strict_int(br_st, "speech_delivered")
    bridge_speech_dropped = _strict_int(br_st, "speech_dropped")
    translate_queue_depth = _strict_int(br_st, "translate_queue_depth")
    speech_queue_depth = _strict_int(br_st, "speech_queue_depth")
    tts_queue_depth = _strict_int(tts_st, "queue_depth")
    tts_state = tts_st.get("state") if (tts_st is not None and isinstance(tts_st.get("state"), str)) else None

    missing_queue_fields: list[str] = []
    if br_st is None:
        missing_queue_fields.append("bridge")
    else:
        if translate_queue_depth is None:
            missing_queue_fields.append("bridge.translate_queue_depth")
        if speech_queue_depth is None:
            missing_queue_fields.append("bridge.speech_queue_depth")
    if tts_st is None:
        missing_queue_fields.append("tts")
    else:
        if tts_queue_depth is None:
            missing_queue_fields.append("tts.queue_depth")
        if tts_state is None:
            missing_queue_fields.append("tts.state")

    translate_enabled = br_st.get("translate_enabled", True) if br_st is not None else True
    subtitles_pending_ok = (pending_refinements == 0) and (
        pending_translations == 0 or translate_enabled is False
    )
    queues_drained = (
        not missing_queue_fields
        and translate_queue_depth == 0
        and speech_queue_depth == 0
        and tts_queue_depth == 0
        and tts_state == "idle"
        and subtitles_pending_ok
    )

    qc_rejected = _strict_int(br_st, "qc_rejected")
    review_skipped = _strict_int(asr_st, "review_skipped")
    review_failed = _strict_int(asr_st, "review_failed")
    tts_synthesized = _strict_int(tts_st, "synthesized")
    tts_skipped = _strict_int(tts_st, "skipped")
    tts_failed = _strict_int(tts_st, "failed")

    has_finals = asr_finals is not None and asr_finals >= 1
    review_skipped_rate = round(review_skipped / asr_finals, 4) if (has_finals and review_skipped is not None) else None
    review_failed_rate = round(review_failed / asr_finals, 4) if (has_finals and review_failed is not None) else None
    qc_reject_rate = round(qc_rejected / asr_finals, 4) if (has_finals and qc_rejected is not None) else None
    silent_drop_rate = (
        round(max(0.0, 1.0 - tts_synthesized / asr_finals), 4)
        if (has_finals and tts_synthesized is not None)
        else None
    )

    # 4. 播报音频有效能量与句间停顿统计（仅接受契约回录文件 rec.raw 或可选 rec.wav）
    rec_path: Path | None = None
    for cand in ("rec.raw", "rec.wav"):
        candidate = run_dir / cand
        if candidate.is_file():
            rec_path = candidate
            break

    pb_metrics: dict[str, Any] | None = None
    if rec_path is not None:
        spans, overall_rms = detect_speech_spans(
            rec_path,
            frame_ms=SPAN_FRAME_MS,
            rms_thr=PLAYBACK_RMS_THRESHOLD,
            merge_gap_ms=PLAYBACK_MERGE_GAP_MS,
        )
        gaps = [round(spans[i]["start_s"] - spans[i - 1]["end_s"], 3) for i in range(1, len(spans))]
        in_max_sil_s = 0.0
        speech_in = run_dir / "speech.wav"
        if speech_in.is_file():
            in_spans, _ = detect_speech_spans(
                speech_in,
                frame_ms=SPAN_FRAME_MS,
                rms_thr=INPUT_RMS_THRESHOLD,
                merge_gap_ms=INPUT_MERGE_GAP_MS,
            )
            in_gaps = [in_spans[i]["start_s"] - in_spans[i - 1]["end_s"] for i in range(1, len(in_spans))]
            in_max_sil_s = max(in_gaps) if in_gaps else 0.0
        gt_1p5 = sum(1 for g in gaps if g > GAP_WARN_S)
        gt_3p0 = sum(1 for g in gaps if g > GAP_SEVERE_S)
        pb_metrics = {
            "audio_file": rec_path.name,
            "playout_policy": playout_policy,
            "overall_rms": overall_rms,
            "speech_spans_count": len(spans),
            "inter_span_gaps_s": gaps,
            "gap_p50_s": round(median(gaps), 3) if gaps else 0.0,
            "gap_p95_s": round(percentile(gaps, 95.0), 3) if gaps else 0.0,
            "gap_max_s": round(max(gaps), 3) if gaps else 0.0,
            "gaps_gt_1p5s": gt_1p5,
            "gaps_gt_1p5s_rate": round(gt_1p5 / len(gaps), 4) if gaps else 0.0,
            "gaps_gt_3p0s": gt_3p0,
            "input_max_natural_silence_s": round(in_max_sil_s, 3),
        }

    # 5. 多轮重放一致性
    repeat_metrics: dict[str, Any] | None = None
    if repeat_dirs:
        all_lists = [finals]
        for rep_dir in repeat_dirs:
            if not rep_dir.is_dir():
                raise InputError(f"重放目录不存在：{rep_dir}")
            u_rep, _ = load_subtitles(rep_dir / "subtitles.jsonl")
            all_lists.append([u for u in u_rep if u["final"] is not None])
        repeat_metrics = compute_repeat_consistency(all_lists)

    metrics: dict[str, Any] = {
        "total_restarts": total_restarts,
        "restarts_by_role": restarts_by_role,
        "missing_or_invalid_roles": missing_or_invalid_roles,
        "non_running_roles": non_running_roles,
        "startup_ids": all_startup_ids,
        "startup_id_unique": startup_id_unique,
        "startup_id_changes": startup_id_changes,
        "playout_policy": playout_policy,
        "output_mode_applied": output_mode_applied,
        "output_mode_phase": output_mode_phase,
        "queues_drained": queues_drained,
        "missing_queue_fields": missing_queue_fields,
        "translate_queue_depth": translate_queue_depth,
        "speech_queue_depth": speech_queue_depth,
        "tts_queue_depth": tts_queue_depth,
        "tts_state": tts_state,
        "pending_refinements": pending_refinements,
        "pending_translations": pending_translations,
        "asr_finals": asr_finals,
        "subtitle_finals": n_finals,
        "subtitle_bilingual_finals": subtitle_bilingual_finals,
        "bridge_translated": bridge_translated,
        "bridge_translate_failed": bridge_translate_failed,
        "bridge_speech_delivered": bridge_speech_delivered,
        "bridge_speech_dropped": bridge_speech_dropped,
        "utt_dur_min_s": round(min(durs), 2) if durs else None,
        "utt_dur_median_s": round(median(durs), 2) if durs else None,
        "utt_dur_p95_s": round(percentile(durs, 95.0), 2) if durs else None,
        "utt_dur_max_s": round(max(durs), 2) if durs else None,
        "short_frag_count": short_frags,
        "short_frag_rate": round(short_frags / n_finals, 4) if n_finals else None,
        "unfinished_tail_count": unfinished_cuts,
        "unfinished_tail_rate": round(unfinished_cuts / n_finals, 4) if n_finals else None,
        "under_split_over_10s_count": over_10s,
        "under_split_over_10s_rate": round(over_10s / len(durs), 4) if durs else None,
        "refinement_timeouts": refinement_timeouts,
        "review_skipped": review_skipped,
        "review_failed": review_failed,
        "review_skipped_rate": review_skipped_rate,
        "review_failed_rate": review_failed_rate,
        "qc_rejected": qc_rejected,
        "qc_reject_rate": qc_reject_rate,
        "tts_synthesized": tts_synthesized,
        "tts_skipped": tts_skipped,
        "tts_failed": tts_failed,
        "silent_drop_rate": silent_drop_rate,
        "final_to_mt_p50_ms": round(median(final_to_mt_ms), 1) if final_to_mt_ms else None,
        "final_to_mt_p95_ms": round(percentile(final_to_mt_ms, 95.0), 1) if final_to_mt_ms else None,
        "final_to_mt_max_ms": round(max(final_to_mt_ms), 1) if final_to_mt_ms else None,
        "translate_ms_p50": round(median(mt_ms_list), 1) if mt_ms_list else None,
        "translate_ms_p95": round(percentile(mt_ms_list, 95.0), 1) if mt_ms_list else None,
        "playback": pb_metrics,
        "repeat_consistency": repeat_metrics,
    }

    # 6. 门限核验（同一套硬阈值；smoke 仅将非健康项降级为 INFO/WARN，绝不伪造 PASS）
    required_checks: list[dict[str, Any]] = []
    advisory_checks: list[dict[str, Any]] = []

    def add_gate(gate: str, group: str, smoke_required: bool, ok: bool, detail: str) -> None:
        is_required = (mode == "strict") or smoke_required
        if is_required:
            required_checks.append({
                "gate": gate,
                "group": group,
                "level": "required",
                "passed": bool(ok),
                "status": "PASS" if ok else "FAIL",
                "detail": detail,
            })
        else:
            advisory_checks.append({
                "gate": gate,
                "group": group,
                "level": "advisory",
                "passed": bool(ok),
                "status": "INFO" if ok else "WARN",
                "detail": detail,
            })

    # G0：链路健康、隔离与队列排空（smoke + strict 均为硬门限）
    process_ok = (
        not missing_or_invalid_roles
        and not non_running_roles
        and total_restarts is not None
        and total_restarts <= THRESHOLDS["max_process_restarts"]
    )
    proc_parts = [f"restarts={total_restarts}", f"by_role={restarts_by_role}"]
    if missing_or_invalid_roles:
        proc_parts.append(f"missing_or_invalid={missing_or_invalid_roles}")
    if non_running_roles:
        proc_parts.append(f"non_running={non_running_roles}")
    add_gate(
        "G0_no_process_restarts",
        "G0",
        True,
        process_ok,
        f"{', '.join(proc_parts)} (要求 6 角色齐备、state='running' 且 restarts <= {THRESHOLDS['max_process_restarts']})",
    )
    add_gate(
        "G0_asr_startup_id_unique",
        "G0",
        True,
        startup_id_unique,
        f"startup_ids={all_startup_ids} (要求唯一非空)",
    )
    add_gate(
        "G0_broadcast_playout_policy",
        "G0",
        True,
        playout_policy == "broadcast_only",
        f"pre.playout_policy={playout_policy!r} (要求 'broadcast_only')",
    )
    add_gate(
        "G0_output_mode_broadcast",
        "G0",
        True,
        output_mode_applied == "broadcast" and output_mode_phase == "applied",
        (
            f"output_mode.applied={output_mode_applied!r}, phase={output_mode_phase!r} "
            f"(要求 applied='broadcast' 且 phase='applied')"
        ),
    )
    queue_detail = (
        f"translate_q={translate_queue_depth}, speech_q={speech_queue_depth}, "
        f"tts_q={tts_queue_depth}, tts.state={tts_state!r}, "
        f"pending_refinements={pending_refinements}, pending_translations={pending_translations}"
    )
    if missing_queue_fields:
        queue_detail = f"missing={missing_queue_fields}; {queue_detail}"
    add_gate("G0_queues_drained", "G0", True, queues_drained, queue_detail)

    add_gate(
        "G0_pipeline_produced_output",
        "G0",
        True,
        (
            asr_finals is not None
            and bridge_translated is not None
            and tts_synthesized is not None
            and asr_finals >= THRESHOLDS["min_asr_finals"]
            and bridge_translated >= THRESHOLDS["min_translated"]
            and subtitle_bilingual_finals >= THRESHOLDS["min_translated"]
            and tts_synthesized >= THRESHOLDS["min_tts_synthesized"]
        ),
        (
            f"asr_finals={asr_finals} (>={THRESHOLDS['min_asr_finals']}), "
            f"translated={bridge_translated} (>={THRESHOLDS['min_translated']}), "
            f"bilingual_finals={subtitle_bilingual_finals} (>={THRESHOLDS['min_translated']}), "
            f"tts_synthesized={tts_synthesized} (>={THRESHOLDS['min_tts_synthesized']})"
        ),
    )

    # G1：断句质量（strict 硬门限，smoke 报告 INFO/WARN）
    if metrics["utt_dur_max_s"] is not None:
        add_gate(
            "G1_no_14s_near_crash",
            "G1",
            False,
            metrics["utt_dur_max_s"] <= THRESHOLDS["max_utt_dur_s"],
            f"utt_dur_max_s={metrics['utt_dur_max_s']} s (limit <= {THRESHOLDS['max_utt_dur_s']} s)",
        )
    else:
        add_gate("G1_no_14s_near_crash", "G1", False, False, "缺少句级音频时间戳（audio_start/end_pts_ns）")

    if not is_short_response_suite:
        if metrics["short_frag_rate"] is not None:
            add_gate(
                "G1_over_split_short_frag",
                "G1",
                False,
                metrics["short_frag_rate"] <= THRESHOLDS["max_short_frag_rate"],
                (
                    f"short_frag_rate={metrics['short_frag_rate']:.1%} "
                    f"({short_frags}/{n_finals}, limit <= {THRESHOLDS['max_short_frag_rate']:.0%})"
                ),
            )
        else:
            add_gate("G1_over_split_short_frag", "G1", False, False, "无 final 字幕记录")

    if metrics["unfinished_tail_rate"] is not None:
        add_gate(
            "G1_unfinished_tail_cut",
            "G1",
            False,
            metrics["unfinished_tail_rate"] <= THRESHOLDS["max_unfinished_tail_rate"],
            (
                f"unfinished_tail_rate={metrics['unfinished_tail_rate']:.1%} "
                f"({unfinished_cuts}/{n_finals}, limit <= {THRESHOLDS['max_unfinished_tail_rate']:.0%})"
            ),
        )
    else:
        add_gate("G1_unfinished_tail_cut", "G1", False, False, "无 final 字幕记录")

    if metrics["under_split_over_10s_rate"] is not None:
        add_gate(
            "G1_under_split_over_10s",
            "G1",
            False,
            metrics["under_split_over_10s_rate"] <= THRESHOLDS["max_under_split_over_10s_rate"],
            (
                f"over_10s_rate={metrics['under_split_over_10s_rate']:.1%} "
                f"({over_10s}/{len(durs)}, limit <= {THRESHOLDS['max_under_split_over_10s_rate']:.0%})"
            ),
        )
    else:
        add_gate("G1_under_split_over_10s", "G1", False, False, "缺少句级音频时间戳")

    # G2：复核、质检、丢句与 TTS 异常
    add_gate(
        "G2_refinement_timeouts",
        "G2",
        True,
        refinement_timeouts <= THRESHOLDS["max_refinement_timeouts"],
        f"refinement_timeouts={refinement_timeouts} (limit <= {THRESHOLDS['max_refinement_timeouts']})",
    )
    add_gate(
        "G2_tts_errors_zero",
        "G2",
        True,
        (
            tts_skipped is not None
            and tts_failed is not None
            and tts_skipped <= THRESHOLDS["max_tts_skipped"]
            and tts_failed <= THRESHOLDS["max_tts_failed"]
        ),
        (
            f"tts_skipped={tts_skipped} (limit <= {THRESHOLDS['max_tts_skipped']}), "
            f"tts_failed={tts_failed} (limit <= {THRESHOLDS['max_tts_failed']})"
        ),
    )
    if review_skipped_rate is not None:
        add_gate(
            "G2_review_skip_rate",
            "G2",
            False,
            review_skipped_rate <= THRESHOLDS["max_review_skip_rate"],
            (
                f"review_skipped_rate={review_skipped_rate:.1%} "
                f"({review_skipped}/{asr_finals}, limit <= {THRESHOLDS['max_review_skip_rate']:.0%})"
            ),
        )
    else:
        add_gate(
            "G2_review_skip_rate",
            "G2",
            False,
            False,
            f"缺少 asr.finals 或 asr.review_skipped（finals={asr_finals}, review_skipped={review_skipped}）",
        )

    if review_failed_rate is not None:
        add_gate(
            "G2_review_fail_rate",
            "G2",
            False,
            review_failed_rate <= THRESHOLDS["max_review_fail_rate"],
            (
                f"review_failed_rate={review_failed_rate:.1%} "
                f"({review_failed}/{asr_finals}, limit <= {THRESHOLDS['max_review_fail_rate']:.0%})"
            ),
        )
    else:
        add_gate(
            "G2_review_fail_rate",
            "G2",
            False,
            False,
            f"缺少 asr.finals 或 asr.review_failed（finals={asr_finals}, review_failed={review_failed}）",
        )

    if qc_reject_rate is not None:
        add_gate(
            "G2_qc_reject_rate",
            "G2",
            False,
            qc_reject_rate <= THRESHOLDS["max_qc_reject_rate"],
            (
                f"qc_reject_rate={qc_reject_rate:.1%} "
                f"({qc_rejected}/{asr_finals}, limit <= {THRESHOLDS['max_qc_reject_rate']:.0%})"
            ),
        )
    else:
        add_gate(
            "G2_qc_reject_rate",
            "G2",
            False,
            False,
            f"缺少 asr.finals 或 bridge.qc_rejected（finals={asr_finals}, qc_rejected={qc_rejected}）",
        )

    if silent_drop_rate is not None:
        add_gate(
            "G2_silent_drop_rate",
            "G2",
            False,
            silent_drop_rate <= THRESHOLDS["max_silent_drop_rate"],
            (
                f"silent_drop_rate={silent_drop_rate:.1%} "
                f"(synthesized={tts_synthesized}/finals={asr_finals}, limit <= {THRESHOLDS['max_silent_drop_rate']:.0%})"
            ),
        )
    else:
        add_gate(
            "G2_silent_drop_rate",
            "G2",
            False,
            False,
            f"缺少 asr.finals 或 tts.synthesized（finals={asr_finals}, synthesized={tts_synthesized}）",
        )

    # G3：句末 final -> 译文落盘延迟
    if metrics["final_to_mt_p50_ms"] is not None and metrics["final_to_mt_p95_ms"] is not None:
        add_gate(
            "G3_final_to_mt_p50",
            "G3",
            False,
            metrics["final_to_mt_p50_ms"] <= THRESHOLDS["max_final_to_mt_p50_ms"],
            f"p50={metrics['final_to_mt_p50_ms']} ms (limit <= {THRESHOLDS['max_final_to_mt_p50_ms']} ms)",
        )
        add_gate(
            "G3_final_to_mt_p95",
            "G3",
            False,
            metrics["final_to_mt_p95_ms"] <= THRESHOLDS["max_final_to_mt_p95_ms"],
            f"p95={metrics['final_to_mt_p95_ms']} ms (limit <= {THRESHOLDS['max_final_to_mt_p95_ms']} ms)",
        )
    else:
        add_gate("G3_final_to_mt_p50", "G3", False, False, "缺少已完成翻译时间戳（received_at / translated_at）")
        add_gate("G3_final_to_mt_p95", "G3", False, False, "缺少已完成翻译时间戳（received_at / translated_at）")

    # G4：播报回录有效能量与句间停顿
    if pb_metrics is not None:
        has_speech = pb_metrics["speech_spans_count"] >= 1
        add_gate(
            "G4_playback_non_silent",
            "G4",
            True,
            pb_metrics["overall_rms"] > THRESHOLDS["min_playback_rms"] and has_speech,
            (
                f"rms={pb_metrics['overall_rms']} (limit > {THRESHOLDS['min_playback_rms']}), "
                f"spans={pb_metrics['speech_spans_count']}"
            ),
        )
        add_gate(
            "G4_max_abnormal_gap",
            "G4",
            False,
            has_speech and pb_metrics["gap_max_s"] <= THRESHOLDS["max_abnormal_gap_s"],
            (
                f"gap_max_s={pb_metrics['gap_max_s']} s (limit <= {THRESHOLDS['max_abnormal_gap_s']} s)"
                if has_speech
                else "播报回录无有效语音段"
            ),
        )
        add_gate(
            "G4_gaps_gt_1p5s_rate",
            "G4",
            False,
            has_speech and pb_metrics["gaps_gt_1p5s_rate"] <= THRESHOLDS["max_abnormal_gaps_gt_1p5s_rate"],
            (
                f"gaps>{GAP_WARN_S}s={pb_metrics['gaps_gt_1p5s']} "
                f"({pb_metrics['gaps_gt_1p5s_rate']:.1%}, limit <= {THRESHOLDS['max_abnormal_gaps_gt_1p5s_rate']:.0%})"
                if has_speech
                else "播报回录无有效语音段"
            ),
        )
    else:
        add_gate("G4_playback_non_silent", "G4", True, False, "缺少播报回录文件（rec.raw / rec.wav）")
        add_gate("G4_max_abnormal_gap", "G4", False, False, "缺少播报回录文件")
        add_gate("G4_gaps_gt_1p5s_rate", "G4", False, False, "缺少播报回录文件")

    # G5：跨轮重放切句一致性（仅传 --repeat-dirs 时启用）
    if repeat_metrics is not None:
        add_gate(
            "G5_repeat_count_cv",
            "G5",
            False,
            repeat_metrics["count_cv"] <= THRESHOLDS["max_repeat_count_cv"],
            (
                f"count_cv={repeat_metrics['count_cv']}, counts={repeat_metrics['counts']} "
                f"(limit <= {THRESHOLDS['max_repeat_count_cv']})"
            ),
        )
        add_gate(
            "G5_repeat_boundary_iou",
            "G5",
            False,
            repeat_metrics["boundary_iou_median"] >= THRESHOLDS["min_repeat_boundary_iou"],
            (
                f"boundary_iou_median={repeat_metrics['boundary_iou_median']} "
                f"(limit >= {THRESHOLDS['min_repeat_boundary_iou']})"
            ),
        )

    passed = all(c["passed"] for c in required_checks)
    strict_passed = passed and all(c["passed"] for c in advisory_checks)
    summary = {
        "required_total": len(required_checks),
        "required_passed": sum(1 for c in required_checks if c["passed"]),
        "required_failed": sum(1 for c in required_checks if not c["passed"]),
        "advisory_total": len(advisory_checks),
        "advisory_ok": sum(1 for c in advisory_checks if c["passed"]),
        "advisory_warn": sum(1 for c in advisory_checks if not c["passed"]),
    }
    return {
        "mode": mode,
        "passed": passed,
        "strict_passed": strict_passed,
        "summary": summary,
        "metrics": metrics,
        "checks": required_checks,
        "advisory_checks": advisory_checks,
    }


def _fmt_opt(val: Any, suffix: str = "", pct: bool = False) -> str:
    if val is None:
        return "N/A"
    if pct:
        return f"{val:.1%}"
    return f"{val}{suffix}"


def print_report(res: dict[str, Any]) -> None:
    m = res["metrics"]
    pb = m["playback"]
    rep = m["repeat_consistency"]
    print("=== METRICS ===")
    print(
        f"  进程与模式  : restarts={m['total_restarts']}  startup_ids={m['startup_ids']}  "
        f"playout_policy={m['playout_policy']}  output_mode={m['output_mode_applied']}/{m['output_mode_phase']}  "
        f"queues_drained={m['queues_drained']}"
    )
    print(
        f"  句数与时长  : asr_finals={m['asr_finals']}  subtitle_finals={m['subtitle_finals']}  "
        f"bilingual={m['subtitle_bilingual_finals']}  translated={m['bridge_translated']}  "
        f"dur(med/p95/max)={_fmt_opt(m['utt_dur_median_s'], ' s')}/"
        f"{_fmt_opt(m['utt_dur_p95_s'], ' s')}/{_fmt_opt(m['utt_dur_max_s'], ' s')}"
    )
    print(
        f"  断句指标    : short_frag={_fmt_opt(m['short_frag_rate'], pct=True)}  "
        f"unfinished_tail={_fmt_opt(m['unfinished_tail_rate'], pct=True)}  "
        f"over_10s={_fmt_opt(m['under_split_over_10s_rate'], pct=True)}"
    )
    print(
        f"  复核/质检/TTS: review_timeouts={m['refinement_timeouts']}  "
        f"review_skip={_fmt_opt(m['review_skipped_rate'], pct=True)}  "
        f"review_fail={_fmt_opt(m['review_failed_rate'], pct=True)}  "
        f"qc_reject={_fmt_opt(m['qc_reject_rate'], pct=True)}  "
        f"silent_drop={_fmt_opt(m['silent_drop_rate'], pct=True)}  "
        f"tts(synth/skip/fail)={m['tts_synthesized']}/{m['tts_skipped']}/{m['tts_failed']}"
    )
    print(
        f"  翻译延迟    : final->mt p50={_fmt_opt(m['final_to_mt_p50_ms'], ' ms')}  "
        f"p95={_fmt_opt(m['final_to_mt_p95_ms'], ' ms')}  "
        f"translate_ms p50={_fmt_opt(m['translate_ms_p50'], ' ms')}"
    )
    if pb is not None:
        print(
            f"  播报回录    : file={pb['audio_file']}  rms={pb['overall_rms']}  "
            f"spans={pb['speech_spans_count']}  gap_max={pb['gap_max_s']} s  "
            f"gaps>{GAP_WARN_S}s={pb['gaps_gt_1p5s']} ({pb['gaps_gt_1p5s_rate']:.1%})  "
            f"in_max_sil={pb['input_max_natural_silence_s']} s"
        )
    else:
        print("  播报回录    : MISSING")
    if rep is not None:
        print(
            f"  跨轮一致性  : counts={rep['counts']}  count_cv={rep['count_cv']}  "
            f"boundary_iou_median={rep['boundary_iou_median']}"
        )

    print(f"\n=== REQUIRED GATES (mode={res['mode']}) ===")
    for c in res["checks"]:
        print(f"  [{c['status']:4s}] {c['gate']:28s} | {c['detail']}")

    if res["advisory_checks"]:
        print("\n=== STRICT GATES (advisory in smoke mode; not counted as PASS) ===")
        for c in res["advisory_checks"]:
            print(f"  [{c['status']:4s}] {c['gate']:28s} | {c['detail']}")

    s = res["summary"]
    if res["mode"] == "smoke":
        strict_tag = "YES" if res["strict_passed"] else "NO"
        verdict = "PASS" if res["passed"] else f"FAIL ({s['required_failed']} required failed)"
        print(f"\nOVERALL (smoke): {verdict} | strict_ready={strict_tag} ({s['advisory_warn']} strict warnings)")
    else:
        verdict = "PASS" if res["passed"] else f"FAIL ({s['required_failed']} required failed)"
        print(f"\nOVERALL (strict): {verdict}")


def _make_stereo_pcm16(segments: list[tuple[float, float]], sr: int = RAW_SAMPLE_RATE) -> bytes:
    """用纯标准库生成立体声 S16_LE 字节流，segments 为 (时长秒, 振幅 0..1) 列表。"""
    out = array("h")
    freq = 440.0
    for dur_s, amp in segments:
        n = int(round(sr * dur_s))
        if amp <= 0.0:
            out.extend([0] * (n * 2))
        else:
            scale = amp * 32767.0
            two_pi_f = 2.0 * math.pi * freq / sr
            for idx in range(n):
                sample = int(scale * math.sin(two_pi_f * idx))
                out.append(sample)
                out.append(sample)
    if sys.byteorder != "little":
        out.byteswap()
    return out.tobytes()


def _write_wav_pcm16(path: Path, pcm_bytes: bytes, nch: int = 2, sr: int = RAW_SAMPLE_RATE) -> None:
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(nch)
        wf.setsampwidth(2)
        wf.setframerate(sr)
        wf.writeframes(pcm_bytes)


def _write_fixture(
    target: Path,
    subs: list[dict[str, Any]],
    status: dict[str, Any],
    rec_bytes: bytes | None,
    rec_name: str = "rec.raw",
) -> None:
    target.mkdir(parents=True, exist_ok=True)
    text = "\n".join(json.dumps(row, ensure_ascii=False) for row in subs) + ("\n" if subs else "")
    (target / "subtitles.jsonl").write_text(text, encoding="utf-8")
    (target / "st-final.json").write_text(json.dumps(status, ensure_ascii=False), encoding="utf-8")
    if rec_bytes is not None:
        if rec_name.endswith(".wav"):
            _write_wav_pcm16(target / rec_name, rec_bytes)
        else:
            (target / rec_name).write_bytes(rec_bytes)


def _base_ok_subs() -> list[dict[str, Any]]:
    return [
        {
            "seq": 1,
            "utterance_id": "aaaa1111:1",
            "zh": "今天天气真不错。",
            "en": "The weather is really nice today.",
            "final": True,
            "revision": 2,
            "received_at": 100.0,
            "translated_at": 100.9,
            "translate_ms": 380,
            "audio_start_pts_ns": 0,
            "audio_end_pts_ns": 3_200_000_000,
            "refinement_status": "completed",
            "translation_status": "completed",
        },
        {
            "seq": 2,
            "utterance_id": "aaaa1111:2",
            "zh": "我们下午去公园散步吧。",
            "en": "Let us take a walk in the park this afternoon.",
            "final": True,
            "revision": 2,
            "received_at": 104.0,
            "translated_at": 105.1,
            "translate_ms": 420,
            "audio_start_pts_ns": 3_800_000_000,
            "audio_end_pts_ns": 7_400_000_000,
            "refinement_status": "completed",
            "translation_status": "completed",
        },
    ]


def _base_ok_status() -> dict[str, Any]:
    return {
        "output_mode": {"requested": "broadcast", "applied": "broadcast", "phase": "applied", "last_error": None},
        "processes": {role: {"state": "running", "restarts": 0} for role in MONITORED_ROLES},
        "pre": {"playout_policy": "broadcast_only", "speaker_state": "broadcast", "tts_frames_played": 180},
        "asr": {"state": "idle", "startup_id": "aaaa1111", "finals": 2, "review_skipped": 0, "review_failed": 0},
        "bridge": {
            "translate_enabled": True,
            "speech_enabled": True,
            "translate_queue_depth": 0,
            "speech_queue_depth": 0,
            "translated": 2,
            "translate_failed": 0,
            "qc_rejected": 0,
            "speech_delivered": 2,
            "speech_dropped": 0,
        },
        "tts": {"state": "idle", "synthesized": 2, "skipped": 0, "failed": 0, "queue_depth": 0},
    }


def _failed_gates(res: dict[str, Any]) -> set[str]:
    return {c["gate"] for c in res["checks"] if not c["passed"]}


def self_test() -> None:
    """运行完整正反例自测，覆盖全部门限、字段缺失拦截、smoke/strict 区分与纯标准库音频解码。"""
    ok_pcm = _make_stereo_pcm16([(1.2, 0.2), (0.4, 0.0), (1.2, 0.2)])
    long_gap_pcm = _make_stereo_pcm16([(1.0, 0.2), (3.1, 0.0), (1.0, 0.2)])
    silent_pcm = _make_stereo_pcm16([(1.0, 0.0)])

    # 0. 验证纯标准库音频分析与 numpy 路径（若已安装）数值一致，并能容忍末尾残缺帧字节
    pcm_with_tail = ok_pcm + b"\x00\x01"
    std_frames, std_rms = _analyze_pcm16_stdlib(pcm_with_tail, RAW_CHANNELS, RAW_SAMPLE_RATE, SPAN_FRAME_MS)
    assert std_rms > 0.1 and len(std_frames) > 0, "stdlib PCM 分析应测得非零 RMS 与帧"
    if np is not None:
        np_frames, np_rms = _analyze_pcm16_numpy(pcm_with_tail, RAW_CHANNELS, RAW_SAMPLE_RATE, SPAN_FRAME_MS)
        assert abs(std_rms - np_rms) < 1e-9, f"stdlib 与 numpy overall_rms 不一致：{std_rms} vs {np_rms}"
        assert len(std_frames) == len(np_frames)
        assert max(abs(a - b) for a, b in zip(std_frames, np_frames)) < 1e-9

    with tempfile.TemporaryDirectory(prefix="mozart-lang-verifier-selftest-") as td:
        root = Path(td)

        # 1. 全通过正例（含多轮重放 + WAV 输入参考）
        ok_dir = root / "ok_run"
        rep1_dir = root / "ok_rep1"
        rep2_dir = root / "ok_rep2"
        _write_fixture(ok_dir, _base_ok_subs(), _base_ok_status(), ok_pcm)
        _write_wav_pcm16(ok_dir / "speech.wav", ok_pcm)
        _write_fixture(rep1_dir, _base_ok_subs(), _base_ok_status(), ok_pcm)
        _write_fixture(rep2_dir, _base_ok_subs(), _base_ok_status(), ok_pcm)

        res_strict = evaluate_run(ok_dir, mode="strict", repeat_dirs=[rep1_dir, rep2_dir])
        assert res_strict["passed"] and res_strict["strict_passed"], f"正例 strict 应 PASS：{res_strict['checks']}"
        assert not res_strict["advisory_checks"]

        res_smoke = evaluate_run(ok_dir, mode="smoke", repeat_dirs=[rep1_dir, rep2_dir])
        assert res_smoke["passed"] and res_smoke["strict_passed"], f"正例 smoke 应 PASS：{res_smoke['checks']}"
        assert all(c["status"] == "INFO" for c in res_smoke["advisory_checks"]), "smoke 建议项不得伪造 PASS"

        # 2. smoke PASS 但 strict FAIL 的区分测试（健康全绿，断句/停顿超标）
        warn_dir = root / "smoke_ok_strict_warn"
        warn_subs = _base_ok_subs()
        warn_subs[0]["zh"] = "因为"
        warn_subs[0]["audio_end_pts_ns"] = 500_000_000
        _write_fixture(warn_dir, warn_subs, _base_ok_status(), long_gap_pcm)

        res_warn_smoke = evaluate_run(warn_dir, mode="smoke")
        assert res_warn_smoke["passed"], f"健康项全绿时 smoke 应 PASS：{res_warn_smoke['checks']}"
        assert not res_warn_smoke["strict_passed"], "存在断句/停顿告警时 strict_passed 必须为 False"
        warn_adv = {c["gate"]: c["status"] for c in res_warn_smoke["advisory_checks"]}
        for g in ("G1_over_split_short_frag", "G1_unfinished_tail_cut", "G4_max_abnormal_gap", "G4_gaps_gt_1p5s_rate"):
            assert warn_adv.get(g) == "WARN", f"smoke 应对 {g} 标 WARN，实际：{warn_adv}"

        res_warn_strict = evaluate_run(warn_dir, mode="strict")
        assert not res_warn_strict["passed"]
        assert {
            "G1_over_split_short_frag",
            "G1_unfinished_tail_cut",
            "G4_max_abnormal_gap",
            "G4_gaps_gt_1p5s_rate",
        }.issubset(_failed_gates(res_warn_strict))

        # 验证 --short-response 仅豁免 G1_over_split_short_frag，不放宽其他门限
        res_short = evaluate_run(warn_dir, mode="strict", is_short_response_suite=True)
        assert "G1_over_split_short_frag" not in {c["gate"] for c in res_short["checks"]}
        assert "G1_unfinished_tail_cut" in _failed_gates(res_short)

        # 3. G0/G2/G4 全部健康守卫及缺失字段反例必败（smoke 模式下同样必须 FAIL）
        def check_smoke_must_fail(
            name: str,
            subs: list[dict[str, Any]],
            st: dict[str, Any],
            pcm: bytes | None,
            expected_gate: str,
            detail_substr: str | None = None,
        ) -> None:
            d = root / name
            _write_fixture(d, subs, st, pcm)
            r = evaluate_run(d, mode="smoke")
            assert not r["passed"], f"{name}: smoke 必须 FAIL"
            assert expected_gate in _failed_gates(r), f"{name}: 未命中 {expected_gate}，实际={_failed_gates(r)}"
            if detail_substr is not None:
                gate_detail = next(c["detail"] for c in r["checks"] if c["gate"] == expected_gate)
                assert detail_substr in gate_detail, f"{name}: detail 未包含 {detail_substr!r}：{gate_detail}"

        # 3a. 进程重启、缺失角色、缺失 restarts 字段、终态非 running
        st = _base_ok_status()
        st["processes"]["asr"]["restarts"] = 1
        check_smoke_must_fail("neg_restart", _base_ok_subs(), st, ok_pcm, "G0_no_process_restarts")

        st = _base_ok_status()
        del st["processes"]["llama"]
        check_smoke_must_fail("neg_missing_role", _base_ok_subs(), st, ok_pcm, "G0_no_process_restarts", "processes.llama")

        st = _base_ok_status()
        del st["processes"]["asr"]["restarts"]
        check_smoke_must_fail("neg_missing_restarts", _base_ok_subs(), st, ok_pcm, "G0_no_process_restarts", "processes.asr.restarts")

        st = _base_ok_status()
        st["processes"]["tts"]["state"] = "restarting"
        check_smoke_must_fail("neg_non_running", _base_ok_subs(), st, ok_pcm, "G0_no_process_restarts", "non_running")

        # 3b. startup_id 不唯一（字幕内换号 + asr.startup_id 不一致）
        subs = _base_ok_subs()
        subs[1]["utterance_id"] = "bbbb2222:1"
        check_smoke_must_fail("neg_startup_sub", subs, _base_ok_status(), ok_pcm, "G0_asr_startup_id_unique")
        st = _base_ok_status()
        st["asr"]["startup_id"] = "cccc3333"
        check_smoke_must_fail("neg_startup_asr", _base_ok_subs(), st, ok_pcm, "G0_asr_startup_id_unique")

        # 3c. playout_policy 非 broadcast_only
        st = _base_ok_status()
        st["pre"]["playout_policy"] = "legacy"
        check_smoke_must_fail("neg_policy", _base_ok_subs(), st, ok_pcm, "G0_broadcast_playout_policy")

        # 3d. output_mode.applied 非 broadcast 或 phase 非 applied
        st = _base_ok_status()
        st["output_mode"]["applied"] = "direct"
        check_smoke_must_fail("neg_mode_applied", _base_ok_subs(), st, ok_pcm, "G0_output_mode_broadcast")
        st = _base_ok_status()
        st["output_mode"]["phase"] = "opening_speech"
        check_smoke_must_fail("neg_mode_phase", _base_ok_subs(), st, ok_pcm, "G0_output_mode_broadcast")

        # 3e. 队列未排空与队列字段缺失（禁止静默补 0）
        st = _base_ok_status()
        st["bridge"]["translate_queue_depth"] = 1
        check_smoke_must_fail("neg_tq", _base_ok_subs(), st, ok_pcm, "G0_queues_drained")
        st = _base_ok_status()
        st["bridge"]["speech_queue_depth"] = 1
        check_smoke_must_fail("neg_sq", _base_ok_subs(), st, ok_pcm, "G0_queues_drained")
        st = _base_ok_status()
        st["tts"]["queue_depth"] = 1
        check_smoke_must_fail("neg_ttsq", _base_ok_subs(), st, ok_pcm, "G0_queues_drained")
        st = _base_ok_status()
        st["tts"]["state"] = "synthesizing"
        check_smoke_must_fail("neg_tts_busy", _base_ok_subs(), st, ok_pcm, "G0_queues_drained")
        subs = _base_ok_subs()
        subs[1]["translation_status"] = "pending"
        check_smoke_must_fail("neg_sub_pending", subs, _base_ok_status(), ok_pcm, "G0_queues_drained")

        for section, field in (
            ("bridge", "translate_queue_depth"),
            ("bridge", "speech_queue_depth"),
            ("tts", "queue_depth"),
            ("tts", "state"),
        ):
            st = _base_ok_status()
            del st[section][field]
            check_smoke_must_fail(
                f"neg_missing_{section}_{field}",
                _base_ok_subs(),
                st,
                ok_pcm,
                "G0_queues_drained",
                f"{section}.{field}",
            )

        # 3f. 链路零产出（含字幕无双语译文与字段缺失）
        for field_group, key in (("asr", "finals"), ("bridge", "translated"), ("tts", "synthesized")):
            st = _base_ok_status()
            st[field_group][key] = 0
            check_smoke_must_fail(f"neg_zero_{key}", _base_ok_subs(), st, ok_pcm, "G0_pipeline_produced_output")
        subs_no_en = _base_ok_subs()
        for row in subs_no_en:
            row["en"] = ""
        check_smoke_must_fail("neg_zero_bilingual", subs_no_en, _base_ok_status(), ok_pcm, "G0_pipeline_produced_output")

        # 3g. 复核超时、TTS skipped/failed（含字段缺失）、回录静音与缺文件
        subs = _base_ok_subs()
        subs[0]["refinement_error"] = REFINEMENT_TIMEOUT_ERROR
        check_smoke_must_fail("neg_ref_timeout", subs, _base_ok_status(), ok_pcm, "G2_refinement_timeouts")

        st = _base_ok_status()
        st["tts"]["skipped"] = 1
        check_smoke_must_fail("neg_tts_skip", _base_ok_subs(), st, ok_pcm, "G2_tts_errors_zero")
        st = _base_ok_status()
        st["tts"]["failed"] = 1
        check_smoke_must_fail("neg_tts_fail", _base_ok_subs(), st, ok_pcm, "G2_tts_errors_zero")
        st = _base_ok_status()
        del st["tts"]["failed"]
        check_smoke_must_fail("neg_tts_fail_missing", _base_ok_subs(), st, ok_pcm, "G2_tts_errors_zero")

        check_smoke_must_fail("neg_silent", _base_ok_subs(), _base_ok_status(), silent_pcm, "G4_playback_non_silent")
        check_smoke_must_fail("neg_no_audio", _base_ok_subs(), _base_ok_status(), None, "G4_playback_non_silent")

        # 4. strict 其余门限反例：G1(>13.5s & >10s), G2(review_skip/review_fail/qc_reject/silent_drop), G3, G4, G5
        bad_strict = root / "neg_strict_all"
        subs_s = _base_ok_subs()
        subs_s[0]["audio_end_pts_ns"] = 13_900_000_000
        subs_s[0]["translated_at"] = 103.5
        subs_s[1]["translated_at"] = 107.2
        st_s = _base_ok_status()
        st_s["asr"]["review_skipped"] = 1
        st_s["asr"]["review_failed"] = 1
        st_s["bridge"]["qc_rejected"] = 1
        st_s["tts"]["synthesized"] = 1
        _write_fixture(bad_strict, subs_s, st_s, long_gap_pcm)

        rep_drift = root / "neg_rep_drift"
        subs_drift = [
            {
                "seq": 1,
                "utterance_id": "dddd4444:1",
                "zh": "只切出了一句且边界偏移。",
                "en": "Only one shifted utterance.",
                "final": True,
                "revision": 1,
                "received_at": 100.0,
                "translated_at": 100.5,
                "audio_start_pts_ns": 0,
                "audio_end_pts_ns": 1_000_000_000,
                "refinement_status": "completed",
                "translation_status": "completed",
            }
        ]
        _write_fixture(rep_drift, subs_drift, _base_ok_status(), ok_pcm)

        res_bad_s = evaluate_run(bad_strict, mode="strict", repeat_dirs=[rep_drift])
        assert not res_bad_s["passed"]
        expected_strict_fails = {
            "G1_no_14s_near_crash",
            "G1_under_split_over_10s",
            "G2_review_skip_rate",
            "G2_review_fail_rate",
            "G2_qc_reject_rate",
            "G2_silent_drop_rate",
            "G3_final_to_mt_p50",
            "G3_final_to_mt_p95",
            "G4_max_abnormal_gap",
            "G4_gaps_gt_1p5s_rate",
            "G5_repeat_count_cv",
            "G5_repeat_boundary_iou",
        }
        missing = expected_strict_fails - _failed_gates(res_bad_s)
        assert not missing, f"strict 反例漏检：{missing}，实际：{_failed_gates(res_bad_s)}"

        # 4b. 单独验证 review_failed > 0 且 review_skipped == 0 时 G2_review_fail_rate 必败（不漏报）
        rev_fail_dir = root / "neg_review_fail_only"
        st_rf = _base_ok_status()
        st_rf["asr"]["review_failed"] = 2
        _write_fixture(rev_fail_dir, _base_ok_subs(), st_rf, ok_pcm)
        res_rf = evaluate_run(rev_fail_dir, mode="strict")
        assert not res_rf["passed"] and "G2_review_fail_rate" in _failed_gates(res_rf)

        # 5. 缺少翻译时间戳时 G3 必败；可选 rec.wav 回录文件也可正常读取
        no_ts_dir = root / "neg_no_mt_ts"
        subs_no_ts = _base_ok_subs()
        for row in subs_no_ts:
            row.pop("translated_at", None)
        _write_fixture(no_ts_dir, subs_no_ts, _base_ok_status(), ok_pcm, rec_name="rec.wav")
        res_no_ts = evaluate_run(no_ts_dir, mode="strict")
        assert {"G3_final_to_mt_p50", "G3_final_to_mt_p95"}.issubset(_failed_gates(res_no_ts))
        assert res_no_ts["metrics"]["playback"]["audio_file"] == "rec.wav"

    print(
        f"SELF-TEST PASSED: covered all {len(res_strict['checks'])} gates "
        f"(strict missing-field guards, review_failed, smoke/strict split, repeat consistency, stdlib PCM16)."
    )


def main() -> None:
    ap = argparse.ArgumentParser(description="Mozart 语言链路自动化验收核验工具")
    ap.add_argument("run_dir", nargs="?", help="语言链路实测产物目录（含 subtitles.jsonl、st-final.json、rec.raw）")
    ap.add_argument("--repeat-dirs", nargs="*", default=None, help="同音频多轮重放产物目录，核验跨轮切句一致性")
    ap.add_argument("--out-json", help="保存完整核验结果 JSON 路径")
    ap.add_argument(
        "--mode",
        choices=("smoke", "strict"),
        default="strict",
        help="门限级别：smoke 严卡链路健康并以 INFO/WARN 报告断句停顿；strict 要求 G0–G5 全部 PASS",
    )
    ap.add_argument("--short-response", action="store_true", help="标记短回应专项（豁免短碎句率门限）")
    ap.add_argument("--self-test", action="store_true", help="运行正反例自测验证全部门限有效性")
    args = ap.parse_args()

    if args.self_test:
        self_test()
        return

    if not args.run_dir:
        ap.error("请提供 run_dir，或使用 --self-test 运行自测")

    try:
        repeat_paths = [Path(p) for p in args.repeat_dirs] if args.repeat_dirs else None
        res = evaluate_run(
            Path(args.run_dir),
            mode=args.mode,
            is_short_response_suite=args.short_response,
            repeat_dirs=repeat_paths,
        )
    except (InputError, OSError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        sys.exit(2)

    if args.out_json:
        out_path = Path(args.out_json)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(res, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    print_report(res)
    sys.exit(0 if res["passed"] else 1)


if __name__ == "__main__":
    main()

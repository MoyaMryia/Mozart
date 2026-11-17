#!/usr/bin/env python3
"""Single source of truth for realtime RVC geometry and compile parameters."""

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path

PARAMS_PATH = Path(__file__).with_name("realtime_chain_params.json")


@dataclass(frozen=True)
class RealtimeGeometry:
    input_sample_rate: int
    output_sample_rate: int
    feature_hop: int
    feature_frame_rate: int
    window_samples: int
    past_context_samples: int
    crossfade_max_out: int
    sola_search_max_out: int
    overlap_out: int
    margin_16k: int
    front_frames: int
    skip_head: int
    return_frames: int
    decoder_frames: int
    hubert_samples: int
    hubert_frames: int
    decoder_out_samples: int
    output_hop: int
    flow_head: int
    latent_frames: int
    decoder_head: int
    rmvpe_frames: int
    startup_delay_ms: int
    startup_buffer_blocks: int
    skip_silence: bool


def load_params(path: Path = PARAMS_PATH) -> dict:
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def params_sha256(path: Path = PARAMS_PATH) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def derive_geometry(
    params: dict | None = None,
    output_sample_rate: int | None = None,
) -> RealtimeGeometry:
    if params is None:
        params = load_params()
    mc = params["model_contract"]
    g = params["geometry"]

    in_sr = int(mc["input_sample_rate"])
    contract_out_sr = int(mc["output_sample_rate"])
    out_sr = int(output_sample_rate or contract_out_sr)
    hop = int(mc["feature_hop"])
    frame_rate = int(mc["feature_frame_rate"])

    w = int(g["window_samples"])
    p = int(g["past_context_samples"])
    c_ref = int(g["crossfade_max_out"])
    s_ref = int(g["sola_search_max_out"])
    overlap_ref = c_ref + s_ref

    if w <= 0 or w % hop != 0:
        raise ValueError(f"window_samples ({w}) must be positive and divisible by {hop}")
    if p <= 0 or p % hop != 0:
        raise ValueError(f"past_context_samples ({p}) must be positive and divisible by {hop}")
    if (overlap_ref * in_sr) % contract_out_sr != 0:
        raise ValueError("overlap_out must map to integer 16k samples")

    margin_16k = (overlap_ref * in_sr) // contract_out_sr
    if (p + w + margin_16k) % hop != 0:
        raise ValueError("(P + W + margin_16k) must be divisible by feature_hop")

    crossfade_max_out = (c_ref * out_sr) // contract_out_sr
    sola_search_max_out = (s_ref * out_sr) // contract_out_sr
    overlap_out = crossfade_max_out + sola_search_max_out

    front_frames = (p + w + margin_16k) // hop
    skip_head = p // hop
    return_frames = (w + margin_16k) // hop
    decoder_frames = return_frames

    hubert_samples = front_frames * hop
    # HuBERT hop 由输入采样率与 HuBERT 帧率推导（JSON model_contract），不写字面量。
    hubert_hop = in_sr // int(mc["hubert_frame_rate"])
    hubert_frames = (hubert_samples - 400) // hubert_hop + 1
    decoder_out_samples = return_frames * (out_sr // frame_rate)
    output_hop = (w * out_sr) // in_sr

    flow_head = max(skip_head - 24, 0)
    latent_frames = front_frames - flow_head
    decoder_head = skip_head - flow_head
    if return_frames > latent_frames:
        raise ValueError("return_frames exceeds latent_frames")

    f0_samples = w + 800
    rmvpe_block = 32 * hop
    rmvpe_audio = rmvpe_block * ((f0_samples - 1) // rmvpe_block + 1) - hop
    rmvpe_mel_frames = rmvpe_audio // hop + 1
    rmvpe_frames = ((rmvpe_mel_frames + 31) // 32) * 32

    return RealtimeGeometry(
        input_sample_rate=in_sr,
        output_sample_rate=out_sr,
        feature_hop=hop,
        feature_frame_rate=frame_rate,
        window_samples=w,
        past_context_samples=p,
        crossfade_max_out=crossfade_max_out,
        sola_search_max_out=sola_search_max_out,
        overlap_out=overlap_out,
        margin_16k=margin_16k,
        front_frames=front_frames,
        skip_head=skip_head,
        return_frames=return_frames,
        decoder_frames=decoder_frames,
        hubert_samples=hubert_samples,
        hubert_frames=hubert_frames,
        decoder_out_samples=decoder_out_samples,
        output_hop=output_hop,
        flow_head=flow_head,
        latent_frames=latent_frames,
        decoder_head=decoder_head,
        rmvpe_frames=rmvpe_frames,
        startup_delay_ms=int(g["startup_delay_ms"]),
        startup_buffer_blocks=int(g["startup_buffer_blocks"]),
        skip_silence=bool(g["skip_silence"]),
    )


def trtexec_flags(params: dict | None = None, fp16_override: bool = False) -> list[str]:
    if params is None:
        params = load_params()
    c = params["compile"]
    flags: list[str] = []
    if c.get("no_tf32", True):
        flags.append("--noTF32")
    flags.append(f"--memPoolSize=workspace:{int(c.get('workspace_mib', 1024))}")
    flags.append(f"--builderOptimizationLevel={int(c.get('builder_optimization_level', 3))}")
    flags.append(f"--profilingVerbosity={c.get('profiling_verbosity', 'detailed')}")
    if fp16_override or c.get("fp16", False):
        flags.append("--fp16")
    return flags

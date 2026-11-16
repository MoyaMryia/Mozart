#!/usr/bin/env python3
"""Build fixed-shape TensorRT engines for the realtime RVC chain.

Validates that each ONNX input has fixed dimensions matching the derived
geometry from realtime_chain_params.json, then runs trtexec with flags from the
same JSON. Covers the realtime generator front/decoder and the realtime
HuBERT/RMVPE feature engines.
"""
import argparse
import json
import subprocess
from pathlib import Path

import onnx

from realtime_chain_params import (
    PARAMS_PATH,
    RealtimeGeometry,
    derive_geometry,
    load_params,
    params_sha256,
    trtexec_flags,
)
from trtexec import resolve as resolve_trtexec


def _input_dims(model_path: Path) -> dict[str, list[int]]:
    model = onnx.load(str(model_path))
    dims_by_name: dict[str, list[int]] = {}
    for inp in model.graph.input:
        dims = [d.dim_value for d in inp.type.tensor_type.shape.dim]
        if any(d <= 0 for d in dims):
            raise ValueError(
                f"{model_path.name} input {inp.name} has a dynamic dim; "
                "realtime engines require fixed shapes"
            )
        dims_by_name[inp.name] = dims
    return dims_by_name


def validate_fixed_shapes(
    model_path: Path, role: str, geom: RealtimeGeometry, mel_bands: int
) -> None:
    dims = _input_dims(model_path)
    if role == "front":
        feats = dims.get("feats")
        if not feats or len(feats) != 3 or feats[0] != 1 or feats[1] != geom.front_frames:
            raise ValueError(
                f"{model_path.name} feats={feats} != [1, {geom.front_frames}, emb]"
            )
    elif role == "decoder":
        z = dims.get("z")
        if not z or len(z) != 3 or z[0] != 1 or z[2] != geom.return_frames:
            raise ValueError(
                f"{model_path.name} z={z} != [1, inter, {geom.return_frames}]"
            )
    elif role == "hubert":
        audio = dims.get("audio")
        if audio != [1, geom.hubert_samples]:
            raise ValueError(
                f"{model_path.name} audio={audio} != [1, {geom.hubert_samples}]"
            )
    elif role == "rmvpe":
        mel = dims.get("mel")
        if mel != [1, mel_bands, geom.rmvpe_frames]:
            raise ValueError(
                f"{model_path.name} mel={mel} != [1, {mel_bands}, {geom.rmvpe_frames}]"
            )


def build(trtexec: str, model_path: Path, engine_path: Path, extra: list[str]) -> None:
    command = [
        trtexec,
        f"--onnx={model_path}",
        f"--saveEngine={engine_path}",
        *extra,
    ]
    print("[realtime-engine]", " ".join(command), flush=True)
    subprocess.run(command, check=True, timeout=1800)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--front", type=Path, required=True, help="<id>-front.onnx")
    parser.add_argument("--decoder", type=Path, required=True, help="<id>-decoder.onnx")
    parser.add_argument("--hubert", type=Path, required=True, help="hubert-realtime.onnx")
    parser.add_argument("--rmvpe", type=Path, required=True, help="rmvpe-realtime.onnx")
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--fp16", action="store_true")
    parser.add_argument(
        "--trtexec", type=Path, help="trtexec path (default: TRTEXEC or PATH)"
    )
    args = parser.parse_args()

    params = load_params()
    geom = derive_geometry(params)
    mel_bands = int(params["model_contract"]["rmvpe_mel_bands"])
    extra = trtexec_flags(params, fp16_override=args.fp16)

    models = (
        ("front", args.front),
        ("decoder", args.decoder),
        ("hubert", args.hubert),
        ("rmvpe", args.rmvpe),
    )
    for role, model in models:
        validate_fixed_shapes(model, role, geom, mel_bands)

    args.output_dir.mkdir(parents=True, exist_ok=True)
    trtexec = resolve_trtexec(args.trtexec)
    for _, model in models:
        engine = args.output_dir / model.with_suffix(".engine").name
        build(trtexec, model, engine, extra)

    manifest = {
        "params_source": PARAMS_PATH.name,
        "params_sha256": params_sha256(),
        "geometry": {
            "window_samples": geom.window_samples,
            "past_context_samples": geom.past_context_samples,
            "front_frames": geom.front_frames,
            "skip_head": geom.skip_head,
            "return_frames": geom.return_frames,
            "hubert_samples": geom.hubert_samples,
            "rmvpe_frames": geom.rmvpe_frames,
        },
        "compile_flags": extra,
    }
    (args.output_dir / "build-manifest.json").write_text(
        json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
    )


if __name__ == "__main__":
    main()

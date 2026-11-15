#!/usr/bin/env python3
"""Build TensorRT engines for the file path.

Builds one dynamic generator engine plus the shared dynamic HuBERT/RMVPE
feature engines. Model-specific dimensions come from the ONNX inputs; the
generator samples per frame come from --sample-rate (rate / 100).
"""

import argparse
import json
import subprocess
from pathlib import Path

import onnx

from trtexec import resolve as resolve_trtexec


def input_dims(model_path: Path) -> dict[str, list[int]]:
    model = onnx.load(str(model_path))
    return {
        inp.name: [dim.dim_value for dim in inp.type.tensor_type.shape.dim]
        for inp in model.graph.input
    }


def static_dim(dims: dict[str, list[int]], path: Path, name: str, index: int) -> int:
    values = dims.get(name)
    value = values[index] if values and len(values) > index else 0
    if value <= 0:
        raise SystemExit(f"{path.name}: input {name} dimension {index} must be static")
    return value


def generator_shapes(
    path: Path, frames_list: list[int], samples_per_frame: int
) -> list[str]:
    dims = input_dims(path)
    emb = static_dim(dims, path, "feats", 2)
    latent = static_dim(dims, path, "latent_noise", 1)
    source_dim = static_dim(dims, path, "source_phase", 2)
    return [
        (
            f"feats:1x{frames}x{emb},p_len:1,pitch:1x{frames},pitchf:1x{frames},"
            f"sid:1,latent_noise:1x{latent}x{frames},"
            f"source_phase:1x1x{source_dim},"
            f"source_noise:1x{frames * samples_per_frame}x1"
        )
        for frames in frames_list
    ]


def rmvpe_shapes(path: Path, max_frames: int) -> list[str]:
    mel_bins = static_dim(input_dims(path), path, "mel", 1)
    return [f"mel:1x{mel_bins}x{frames}" for frames in (32, 1056, max_frames)]


def hubert_shapes(max_samples: int) -> list[str]:
    return [f"audio:1x{samples}" for samples in (32000, 64000, max_samples)]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--generator", type=Path, required=True,
                        help="dynamic generator onnx")
    parser.add_argument("--generator-engine", type=Path,
                        help="generator engine output (default: generator with .engine)")
    parser.add_argument("--feature-assets", type=Path, required=True,
                        help="directory containing rmvpe_dynamic.onnx and "
                             "hubert_base_dynamic.onnx")
    parser.add_argument("--sample-rate", type=int, default=48000,
                        help="generator sample rate; samples per frame = rate / 100")
    parser.add_argument("--max-generator-frames", type=int, default=2400)
    parser.add_argument("--max-rmvpe-frames", type=int, default=2304)
    parser.add_argument("--max-hubert-samples", type=int, default=384000)
    parser.add_argument("--trtexec", type=Path,
                        help="trtexec path (default: TRTEXEC or PATH)")
    args = parser.parse_args()

    if args.sample_rate % 100:
        parser.error("--sample-rate must be a multiple of 100")
    if args.max_generator_frames < 398 or args.max_rmvpe_frames < 1056 \
            or args.max_hubert_samples < 64000:
        parser.error("Maximum shapes must include the optimization shape")
    if args.max_rmvpe_frames % 32:
        parser.error("Maximum RMVPE time must be a multiple of 32")

    rmvpe_onnx = args.feature_assets / "rmvpe_dynamic.onnx"
    hubert_onnx = args.feature_assets / "hubert_base_dynamic.onnx"
    for required in (args.generator, rmvpe_onnx, hubert_onnx):
        if not required.exists():
            raise SystemExit(f"onnx not found: {required}")

    samples_per_frame = args.sample_rate // 100
    generator_engine = args.generator_engine or args.generator.with_suffix(".engine")
    plans = [
        (
            generator_engine.stem,
            args.generator,
            generator_engine,
            generator_shapes(
                args.generator, [100, 398, args.max_generator_frames],
                samples_per_frame,
            ),
        ),
        (
            rmvpe_onnx.stem,
            rmvpe_onnx,
            rmvpe_onnx.with_suffix(".engine"),
            rmvpe_shapes(rmvpe_onnx, args.max_rmvpe_frames),
        ),
        (
            hubert_onnx.stem,
            hubert_onnx,
            hubert_onnx.with_suffix(".engine"),
            hubert_shapes(args.max_hubert_samples),
        ),
    ]

    trtexec = resolve_trtexec(args.trtexec)
    plans_path = generator_engine.parent / "build-plans.json"
    plans_path.parent.mkdir(parents=True, exist_ok=True)
    records = []
    for name, source, engine, shapes in plans:
        command = [
            trtexec,
            "--onnx=" + str(source),
            "--saveEngine=" + str(engine),
            "--minShapes=" + shapes[0],
            "--optShapes=" + shapes[1],
            "--maxShapes=" + shapes[2],
            "--noTF32",
            "--skipInference",
            "--memPoolSize=workspace:1024",
            "--builderOptimizationLevel=3",
            "--maxAuxStreams=0",
            "--profilingVerbosity=detailed",
        ]
        records.append({"name": name, "command": command})
        plans_path.write_text(json.dumps(records, indent=2) + "\n")
        log_path = engine.parent / (engine.stem + "-build.log")
        with log_path.open("w") as log:
            subprocess.run(command, stdout=log, stderr=subprocess.STDOUT,
                           check=True, timeout=900)
        print(name, "completed", flush=True)


if __name__ == "__main__":
    main()

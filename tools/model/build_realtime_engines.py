#!/usr/bin/env python3
"""Build fixed-shape TensorRT engines for the realtime RVC chain.

Validates that each ONNX input has fixed dimensions, then runs trtexec. Covers
the realtime generator front/decoder and the realtime HuBERT/RMVPE feature
engines. The file-path dynamic engines are built by build_dynamic_engines.py
instead.

Realtime contract (rvc/src/rvc/pipeline.cpp): front feats 1x280x768, decoder
z 1x192x30, hubert-realtime audio 1x44800, rmvpe-realtime mel 1x128x32. The
exported ONNX already carries these fixed shapes; trtexec reads them from the
model, so model-specific dimensions (inter_channels, source_dim) are not
hard-coded. TensorRT rejects --shapes for a static model.
"""
import argparse
import subprocess
from pathlib import Path

import onnx


def validate_fixed_shapes(model_path: Path) -> None:
    model = onnx.load(str(model_path))
    for inp in model.graph.input:
        dims = [d.dim_value for d in inp.type.tensor_type.shape.dim]
        if any(d <= 0 for d in dims):
            raise ValueError(
                f"{model_path.name} input {inp.name} has a dynamic dim; "
                "realtime engines require fixed shapes"
            )


def build(model_path: Path, engine_path: Path, extra: list) -> None:
    validate_fixed_shapes(model_path)
    command = [
        "/usr/bin/trtexec",
        f"--onnx={model_path}",
        f"--saveEngine={engine_path}",
        *extra,
    ]
    print("[realtime-engine]", " ".join(command), flush=True)
    subprocess.run(command, check=True, timeout=1800)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--front", type=Path, required=True,
                        help="<id>-front.onnx")
    parser.add_argument("--decoder", type=Path, required=True,
                        help="<id>-decoder.onnx")
    parser.add_argument("--hubert", type=Path, required=True,
                        help="hubert-realtime.onnx")
    parser.add_argument("--rmvpe", type=Path, required=True,
                        help="rmvpe-realtime.onnx")
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--fp16", action="store_true")
    args = parser.parse_args()

    args.output_dir.mkdir(parents=True, exist_ok=True)
    extra = ["--noTF32", "--memPoolSize=workspace:1024",
             "--builderOptimizationLevel=3", "--profilingVerbosity=detailed"]
    if args.fp16:
        extra.append("--fp16")

    for model in (args.front, args.decoder, args.hubert, args.rmvpe):
        engine = args.output_dir / model.with_suffix(".engine").name
        build(model, engine, extra)


if __name__ == "__main__":
    main()
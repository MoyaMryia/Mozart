#!/usr/bin/env python3
"""Build TensorRT engines for the file path.

Tiers map a target main window to generator, HuBERT, and RMVPE profiles.
The default is ``large``: a 22 s main window with a 512000-sample HuBERT
profile. HuBERT max leaves headroom for larger windows. Tiers are defaults,
not limits; explicit max overrides are allowed. Model-specific sample rate and
dimensions come from the ONNX inputs; an optional model ``config.json``
supplies the sample rate and validates the embedding width. Static ONNX inputs
are compiled without profile shapes.
"""

import argparse
import hashlib
import json
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

from trtexec import resolve as resolve_trtexec, resolve_or_name

HUBERT_SAMPLES_PER_FRAME = 160
# HuBERT 卷积栈几何，镜像 inferencer.cpp 的 kHubertHop16k/kHubertReceptive16k
#（offline_window_geometry 的窗输入反推）；两侧改动须同步，C++ 侧有单测钉住。
HUBERT_HOP_SAMPLES = 320
HUBERT_RECEPTIVE_SAMPLES = 400
DEFAULT_SAMPLE_RATE = 48000


def window_input_samples(generator_frames: int) -> int:
    """HuBERT 恰好产出 generator_frames 帧的最小窗输入（向上对齐 320）。"""
    min_input = (
        generator_frames * HUBERT_SAMPLES_PER_FRAME
        + (HUBERT_RECEPTIVE_SAMPLES - HUBERT_HOP_SAMPLES)
    )
    return (
        (min_input + HUBERT_HOP_SAMPLES - 1)
        // HUBERT_HOP_SAMPLES
        * HUBERT_HOP_SAMPLES
    )


@dataclass(frozen=True)
class Tier:
    name: str
    generator_frames: tuple[int, int, int]
    hubert_samples: tuple[int, int, int]
    rmvpe_frames: tuple[int, int, int]

    @property
    def main_window_seconds(self) -> int:
        # One second of context on both sides of the generator input.
        return self.generator_frames[2] // 100 - 2


TIERS_PATH = Path(__file__).with_name("file_chain_tiers.json")


def load_tiers(path: Path) -> tuple[dict[str, Tier], str]:
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"{path}: cannot read tiers: {error}")
    if data.get("version") != 1:
        raise SystemExit(f"{path}: unsupported tiers version")
    tiers: dict[str, Tier] = {}
    for name, values in data.get("tiers", {}).items():
        try:
            generator = tuple(int(v) for v in values["generator_frames"])
            hubert = tuple(int(v) for v in values["hubert_samples"])
            rmvpe = tuple(int(v) for v in values["rmvpe_frames"])
        except (KeyError, TypeError, ValueError) as error:
            raise SystemExit(f"{path}: tier {name} is invalid: {error}")
        if len(generator) != 3 or len(hubert) != 3 or len(rmvpe) != 3:
            raise SystemExit(
                f"{path}: tier {name} needs three values per profile"
            )
        tiers[name] = Tier(name, generator, hubert, rmvpe)
    if not tiers:
        raise SystemExit(f"{path}: no tiers defined")
    default = data.get("default")
    if default not in tiers:
        raise SystemExit(f"{path}: default tier is missing or unknown")
    return tiers, default


TIERS, DEFAULT_TIER = load_tiers(TIERS_PATH)


@dataclass(frozen=True)
class ModelConfig:
    sampling_rate: int
    emb_channels: int
    spk_id: int
    spk_count: int


@dataclass
class BuildPlan:
    name: str
    source: Path
    engine: Path
    dynamic: bool
    shapes: tuple[str, str, str] | None
    command: list[str] = field(default_factory=list)


def input_shapes(path: Path) -> dict[str, list[int]]:
    import onnx

    model = onnx.load(str(path))
    return {
        inp.name: [dim.dim_value for dim in inp.type.tensor_type.shape.dim]
        for inp in model.graph.input
    }


def require_input(
    shapes: dict[str, list[int]], name: str, path: Path
) -> list[int]:
    values = shapes.get(name)
    if not values:
        raise SystemExit(f"{path.name}: missing input {name}")
    return values


def static_dim(
    shapes: dict[str, list[int]], name: str, index: int, path: Path
) -> int:
    values = require_input(shapes, name, path)
    value = values[index] if len(values) > index else 0
    if value <= 0:
        raise SystemExit(f"{path.name}: input {name} dimension {index} must be static")
    return value


def input_is_dynamic(shapes: dict[str, list[int]], name: str, path: Path) -> bool:
    return any(value <= 0 for value in require_input(shapes, name, path))


def generator_shapes(
    path: Path, frames: tuple[int, int, int], samples_per_frame: int
) -> list[str]:
    dims = input_shapes(path)
    emb = static_dim(dims, "feats", 2, path)
    latent = static_dim(dims, "latent_noise", 1, path)
    source_dim = static_dim(dims, "source_phase", 2, path)
    return [
        (
            f"feats:1x{value}x{emb},p_len:1,pitch:1x{value},pitchf:1x{value},"
            f"sid:1,latent_noise:1x{latent}x{value},"
            f"source_phase:1x1x{source_dim},"
            f"source_noise:1x{value * samples_per_frame}x1"
        )
        for value in frames
    ]


def rmvpe_shapes(path: Path, frames: tuple[int, int, int]) -> list[str]:
    mel_bins = static_dim(input_shapes(path), "mel", 1, path)
    return [f"mel:1x{mel_bins}x{value}" for value in frames]


def hubert_shapes(samples: tuple[int, int, int]) -> list[str]:
    return [f"audio:1x{value}" for value in samples]


def load_model_config(path: Path) -> ModelConfig:
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"{path}: cannot read config: {error}")
    if not isinstance(data, dict):
        raise SystemExit(f"{path}: config must be a JSON object")
    config_data = data.get("data", {})
    model_data = data.get("model", {})
    spk_data = data.get("spk", {})
    for name, value in (
        ("data", config_data), ("model", model_data), ("spk", spk_data)
    ):
        if not isinstance(value, dict):
            raise SystemExit(f"{path}: {name} must be an object")
    try:
        sampling_rate = int(config_data.get("sampling_rate", 0))
        emb_channels = int(model_data.get("emb_channels", 0))
        spk_id = int(spk_data.get("id", 0))
        spk_count = int(spk_data.get("count", 0))
    except (TypeError, ValueError) as error:
        raise SystemExit(f"{path}: invalid numeric field: {error}")
    if sampling_rate <= 0 or sampling_rate % 100:
        raise SystemExit(f"{path}: sampling_rate must be a positive multiple of 100")
    if data.get("version") != "v2":
        raise SystemExit(f"{path}: only RVC v2 is supported")
    if data.get("f0", 1) != 1:
        raise SystemExit(f"{path}: f0 must be true")
    if emb_channels <= 0:
        raise SystemExit(f"{path}: model.emb_channels must be positive")
    if spk_id < 0 or (spk_count > 0 and spk_id >= spk_count):
        raise SystemExit(f"{path}: spk.id must be within spk.count")
    return ModelConfig(sampling_rate, emb_channels, spk_id, spk_count)


def resolve_tier(tier: Tier, args: argparse.Namespace) -> Tier:
    generator = list(tier.generator_frames)
    hubert = list(tier.hubert_samples)
    rmvpe = list(tier.rmvpe_frames)
    if args.max_generator_frames is not None:
        generator[2] = args.max_generator_frames
    if args.max_hubert_samples is not None:
        hubert[2] = args.max_hubert_samples
    if args.max_rmvpe_frames is not None:
        rmvpe[2] = args.max_rmvpe_frames
    for name, values in (
        ("generator", generator), ("hubert", hubert), ("rmvpe", rmvpe)
    ):
        if values[1] > values[2]:
            print(
                f"{name} opt {values[1]} clamped to max {values[2]}",
                file=sys.stderr,
            )
            values[1] = values[2]
    return Tier(tier.name, tuple(generator), tuple(hubert), tuple(rmvpe))


def validate_tier(tier: Tier) -> None:
    groups = (
        ("generator", tier.generator_frames),
        ("hubert", tier.hubert_samples),
        ("rmvpe", tier.rmvpe_frames),
    )
    for name, values in groups:
        if values[0] > values[1]:
            raise SystemExit(f"{name} min must be <= opt")
    generator_max = tier.generator_frames[2]
    hubert_required = window_input_samples(generator_max)
    if tier.hubert_samples[2] < hubert_required:
        raise SystemExit(
            "hubert max must cover the generator window input "
            f"({generator_max} frames -> {hubert_required} samples)"
        )
    if any(value % 32 for value in tier.rmvpe_frames):
        raise SystemExit("rmvpe frame counts must be multiples of 32")
    # RMVPE pads the whole padded file to a 32-multiple; the STFT adds one frame.
    rmvpe_required = 32 * ((generator_max + 32) // 32)
    if tier.rmvpe_frames[2] < rmvpe_required:
        raise SystemExit(
            f"rmvpe max must cover the generator window ({rmvpe_required} frames)"
        )


def build_command(trtexec: str, plan: BuildPlan) -> list[str]:
    command = [
        trtexec,
        "--onnx=" + str(plan.source),
        "--saveEngine=" + str(plan.engine),
    ]
    if plan.shapes:
        command.extend([
            "--minShapes=" + plan.shapes[0],
            "--optShapes=" + plan.shapes[1],
            "--maxShapes=" + plan.shapes[2],
        ])
    command.extend([
        "--noTF32",
        "--skipInference",
        "--memPoolSize=workspace:1024",
        "--builderOptimizationLevel=3",
        "--maxAuxStreams=0",
        "--profilingVerbosity=detailed",
    ])
    return command


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def trtexec_version(trtexec: str) -> tuple[str, str | None]:
    result = subprocess.run(
        [trtexec, "--version"], capture_output=True, text=True, check=False
    )
    for line in (result.stdout + result.stderr).splitlines():
        if "TensorRT.trtexec" in line:
            return line.strip(), None
    reason = result.stderr.strip() or f"exit {result.returncode}"
    return "unknown", reason


def plan_record(plan: BuildPlan) -> dict:
    return {
        "source": str(plan.source),
        "engine": str(plan.engine),
        "dynamic": plan.dynamic,
        "shapes": {
            "min": plan.shapes[0],
            "opt": plan.shapes[1],
            "max": plan.shapes[2],
        } if plan.shapes else None,
        "command": plan.command,
        "size_bytes": plan.engine.stat().st_size,
        "sha256": sha256(plan.engine),
    }


def print_tiers() -> None:
    print(f"{'tier':<10} {'main':>6} {'generator':>18} {'hubert':>22} {'rmvpe':>16}")
    for tier in TIERS.values():
        generator = "/".join(str(value) for value in tier.generator_frames)
        hubert = "/".join(str(value) for value in tier.hubert_samples)
        rmvpe = "/".join(str(value) for value in tier.rmvpe_frames)
        print(
            f"{tier.name:<10} {tier.main_window_seconds:>5}s "
            f"{generator:>18} {hubert:>22} {rmvpe:>16}"
        )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--generator", type=Path,
                        help="generator onnx")
    parser.add_argument("--generator-engine", type=Path,
                        help="generator engine output (default: generator with .engine)")
    parser.add_argument("--feature-assets", type=Path,
                        help="directory containing rmvpe_dynamic.onnx and "
                             "hubert_base_dynamic.onnx")
    parser.add_argument("--feature-engine-dir", type=Path,
                        help="feature engine output directory (default: next to onnx)")
    parser.add_argument("--model-config", type=Path,
                        help="model config.json; supplies sampling_rate and "
                             "validates emb_channels")
    parser.add_argument("--model-id", help="model id recorded in the manifest")
    parser.add_argument("--sample-rate", type=int,
                        help="model sample rate (default: config.json or 48000)")
    parser.add_argument("--tier", choices=sorted(TIERS), default=DEFAULT_TIER,
                        help="window profile tier (default: %(default)s)")
    parser.add_argument("--max-generator-frames", type=int,
                        help="override the tier generator max")
    parser.add_argument("--max-hubert-samples", type=int,
                        help="override the tier HuBERT max")
    parser.add_argument("--max-rmvpe-frames", type=int,
                        help="override the tier RMVPE max; multiple of 32")
    parser.add_argument("--generator-only", action="store_true",
                        help="build the generator engine only")
    parser.add_argument("--feature-only", action="store_true",
                        help="build the feature engines only")
    parser.add_argument("--manifest", type=Path,
                        help="manifest output path")
    parser.add_argument("--dry-run", action="store_true",
                        help="print the plan without building")
    parser.add_argument("--list-tiers", action="store_true",
                        help="print the tier table and exit")
    parser.add_argument("--trtexec", type=Path,
                        help="trtexec path (default: TRTEXEC or PATH)")
    args = parser.parse_args()

    if args.list_tiers:
        print_tiers()
        return
    if args.generator_only and args.feature_only:
        parser.error("--generator-only and --feature-only are mutually exclusive")
    if args.generator_only and args.generator is None:
        parser.error("--generator-only requires --generator")
    if args.feature_only and args.generator is not None:
        parser.error("--feature-only cannot be combined with --generator")
    if args.feature_only and args.feature_assets is None:
        parser.error("--feature-only requires --feature-assets")
    if not args.generator_only and args.feature_assets is None:
        parser.error("--feature-assets is required unless --generator-only is set")
    if args.generator is None and args.feature_assets is None:
        parser.error("provide --generator, --feature-assets, or both")

    model_config = load_model_config(args.model_config) if args.model_config else None
    if args.sample_rate is not None:
        if args.sample_rate <= 0 or args.sample_rate % 100:
            parser.error("--sample-rate must be a positive multiple of 100")
        if model_config is not None \
                and args.sample_rate != model_config.sampling_rate:
            parser.error(
                "sample rate mismatch: "
                f"--sample-rate {args.sample_rate} vs config "
                f"{model_config.sampling_rate}"
            )
    sample_rate = args.sample_rate if args.sample_rate is not None else (
        model_config.sampling_rate if model_config else DEFAULT_SAMPLE_RATE
    )
    samples_per_frame = sample_rate // 100

    tier = resolve_tier(TIERS[args.tier], args)
    validate_tier(tier)

    plans: list[BuildPlan] = []
    if args.generator is not None:
        if not args.generator.exists():
            raise SystemExit(f"onnx not found: {args.generator}")
        shapes = input_shapes(args.generator)
        dynamic = input_is_dynamic(shapes, "feats", args.generator)
        if model_config is not None:
            emb = static_dim(shapes, "feats", 2, args.generator)
            if emb != model_config.emb_channels:
                raise SystemExit(
                    f"{args.generator.name}: emb_channels {emb} does not match "
                    f"config {model_config.emb_channels}"
                )
        frame_shapes = tuple(generator_shapes(
            args.generator, tier.generator_frames, samples_per_frame
        )) if dynamic else None
        engine = args.generator_engine or args.generator.with_suffix(".engine")
        plans.append(BuildPlan(
            "generator", args.generator, engine, dynamic, frame_shapes
        ))
        if not dynamic:
            static_frames = require_input(shapes, "feats", args.generator)[1]
            print(
                f"generator is static T={static_frames}; "
                "tier window is not applied",
                flush=True,
            )

    if not args.generator_only:
        feature_assets = args.feature_assets
        hubert_onnx = feature_assets / "hubert_base_dynamic.onnx"
        rmvpe_onnx = feature_assets / "rmvpe_dynamic.onnx"
        for required in (hubert_onnx, rmvpe_onnx):
            if not required.exists():
                raise SystemExit(f"onnx not found: {required}")
        engine_dir = args.feature_engine_dir or feature_assets
        hubert_shapes_value = None
        if input_is_dynamic(input_shapes(hubert_onnx), "audio", hubert_onnx):
            hubert_shapes_value = tuple(hubert_shapes(tier.hubert_samples))
        rmvpe_shapes_value = None
        if input_is_dynamic(input_shapes(rmvpe_onnx), "mel", rmvpe_onnx):
            rmvpe_shapes_value = tuple(rmvpe_shapes(rmvpe_onnx, tier.rmvpe_frames))
        plans.append(BuildPlan(
            "hubert", hubert_onnx,
            engine_dir / (hubert_onnx.stem + ".engine"),
            hubert_shapes_value is not None, hubert_shapes_value,
        ))
        plans.append(BuildPlan(
            "rmvpe", rmvpe_onnx,
            engine_dir / (rmvpe_onnx.stem + ".engine"),
            rmvpe_shapes_value is not None, rmvpe_shapes_value,
        ))

    trtexec_name = resolve_or_name(args.trtexec)
    for plan in plans:
        plan.command = build_command(trtexec_name, plan)

    print(
        f"tier={tier.name} main_window={tier.main_window_seconds}s "
        f"sample_rate={sample_rate}",
        flush=True,
    )
    for plan in plans:
        print(f"{plan.name}: {plan.source} -> {plan.engine}", flush=True)
        print(" ".join(plan.command), flush=True)

    if args.dry_run:
        return

    trtexec = resolve_trtexec(args.trtexec)
    for plan in plans:
        plan.engine.parent.mkdir(parents=True, exist_ok=True)
        plan.command = build_command(trtexec, plan)

    for plan in plans:
        log_path = plan.engine.parent / (plan.engine.stem + "-build.log")
        with log_path.open("w") as log:
            subprocess.run(
                plan.command, stdout=log, stderr=subprocess.STDOUT,
                check=True, timeout=1800,
            )
        print(plan.name, "completed", flush=True)

    manifest_path = args.manifest or (
        plans[0].engine.parent / "build-manifest.json"
    )
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    generator_plan = next(
        (plan for plan in plans if plan.name == "generator"), None
    )
    version, version_error = trtexec_version(trtexec)
    sample_rate_source = (
        "config" if model_config else (
            "flag" if args.sample_rate is not None else "default"
        )
    )
    manifest = {
        "version": 1,
        "created_unix_ms": int(time.time() * 1000),
        "tier": tier.name,
        "generator_dynamic": generator_plan.dynamic if generator_plan else None,
        "main_window_seconds": (
            tier.main_window_seconds
            if generator_plan and generator_plan.dynamic else None
        ),
        "sample_rate": sample_rate,
        "sample_rate_source": sample_rate_source,
        "samples_per_frame": samples_per_frame,
        "model": {
            "id": args.model_id or (
                args.model_config.parent.name if args.model_config else None
            ),
            "config": str(args.model_config) if args.model_config else None,
            "emb_channels": model_config.emb_channels if model_config else None,
            "spk_id": model_config.spk_id if model_config else None,
            "spk_count": model_config.spk_count if model_config else None,
        },
        "trtexec": {"version": version, "error": version_error},
        "engines": {plan.name: plan_record(plan) for plan in plans},
    }
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"manifest: {manifest_path}", flush=True)


if __name__ == "__main__":
    main()

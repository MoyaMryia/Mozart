#!/usr/bin/env python3
"""Export the L2 prosody sculptor (prosody_sculptor_l2) to a Mozart runtime ONNX.

Wraps CausalIntentProsodySculptor so the four heads are concatenated into a
single frame-major output [1, T, 771]:
  channel 0     pred_log_f0
  channel 1     pred_voiced_logit
  channel 2     pred_log_gain_db
  channels 3..  delta_feats (768)
Single-output keeps the C++ IEngine interface (one flat result) unchanged.

Usage:
  python3 export_sculptor_onnx.py --checkpoint best.pt --l2-dir /path/prosody_sculptor_l2 \
      --output sculptor.onnx [--target-frames 280]
Requires: torch, onnxruntime (parity check), the prosody_sculptor_l2 package on --l2-dir.
"""
import argparse
import sys
import tempfile
from pathlib import Path

import numpy as np
import torch


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--checkpoint", type=Path, required=True)
    p.add_argument("--l2-dir", type=Path, required=True,
                   help="Directory containing the prosody_sculptor_l2 python modules")
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--target-frames", type=int, default=280)
    args = p.parse_args()

    sys.path.insert(0, str(args.l2_dir.resolve()))
    from kit import INPUTS, load_sculptor  # noqa: E402  (path set above)

    model = load_sculptor(args.checkpoint)
    if model.receptive_field_frames > args.target_frames:
        raise SystemExit(
            f"target-frames {args.target_frames} < receptive field "
            f"{model.receptive_field_frames}")
    if args.target_frames % 8 != 0:
        raise SystemExit("--target-frames must stay on the 80 ms block grid (multiple of 8)")
    frames = args.target_frames
    total = 3 + model.hubert_dim

    class Combined(torch.nn.Module):
        def __init__(self, inner):
            super().__init__()
            self.inner = inner

        def forward(self, hubert_upsampled, intent_signals, cond):
            f0, voiced, gain, delta = self.inner(hubert_upsampled, intent_signals, cond)
            # [B,T],[B,T],[B,T],[B,T,768] -> [B,T,771]
            f0 = f0.unsqueeze(-1)
            voiced = voiced.unsqueeze(-1)
            gain = gain.unsqueeze(-1)
            return torch.cat((f0, voiced, gain, delta), dim=-1)

    wrapper = Combined(model).eval()

    hub = torch.randn(1, frames, model.hubert_dim)
    intent = torch.randn(1, frames, 4)
    cond = torch.tensor([[1.0, 0.65, 0.43, 0.7]], dtype=torch.float32)
    sample = (hub, intent, cond)

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(suffix=".onnx", dir=str(args.output.parent)) as tmp:
        torch.onnx.export(
            wrapper, sample, tmp.name, opset_version=17,
            input_names=list(INPUTS),
            output_names=["outputs"],
            do_constant_folding=True,
            dynamo=False,
        )
        Path(tmp.name).replace(args.output)

    import onnxruntime as ort  # noqa: E402

    sess = ort.InferenceSession(str(args.output), providers=["CPUExecutionProvider"])
    with torch.inference_mode():
        ref = wrapper(hub, intent, cond).numpy()
    got = sess.run(["outputs"], {
        "hubert_upsampled": hub.numpy(),
        "intent_signals": intent.numpy(),
        "cond": cond.numpy(),
    })[0]
    diff = float(np.max(np.abs(ref - got)))
    print(f"Exported {args.output} frames={frames} channels={total} ORT max_diff={diff:.3e}")
    if diff >= 1e-4:
        raise SystemExit("ORT parity check failed")
    print(f"Jetson (run in the ONNX directory):")
    print(f"  /usr/src/tensorrt/bin/trtexec --onnx=sculptor.onnx --fp16 "
          f"--saveEngine=sculptor.engine")


if __name__ == "__main__":
    main()

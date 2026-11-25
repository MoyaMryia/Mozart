#!/usr/bin/env python3
"""Unit tests for realtime_chain_params.py geometry derivation."""
import copy
from dataclasses import asdict
import re
import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from realtime_chain_params import derive_geometry, load_params, trtexec_flags


def expected_geometry(params: dict, out_sr: int | None = None) -> dict:
    """Independent oracle: SPEC-03 formulas applied to the raw JSON numbers.

    Aux training constants (HuBERT receptive field 400, RMVPE block 32 frames,
    f0 right margin 800, decoder_head 24) are anchored by the golden assertions
    below and by tests/test_rvc_window_geometry.cpp on the C++ side.
    """
    mc, g = params["model_contract"], params["geometry"]
    in_sr = mc["input_sample_rate"]
    contract_sr = mc["output_sample_rate"]
    sr = out_sr or contract_sr
    hop = mc["feature_hop"]
    frame_rate = mc["feature_frame_rate"]
    hubert_hop = in_sr // mc["hubert_frame_rate"]
    w, p = g["window_samples"], g["past_context_samples"]
    c_ref, s_ref = g["crossfade_max_out"], g["sola_search_max_out"]

    margin = (c_ref + s_ref) * in_sr // contract_sr
    front = (p + w + margin) // hop
    skip = p // hop
    ret = (w + margin) // hop
    hubert_samples = front * hop
    flow_head = max(skip - 24, 0)
    f0_samples = w + 800
    rmvpe_block = 32 * hop
    rmvpe_audio = rmvpe_block * ((f0_samples - 1) // rmvpe_block + 1) - hop
    mel_frames = rmvpe_audio // hop + 1
    return {
        "input_sample_rate": in_sr,
        "output_sample_rate": sr,
        "feature_hop": hop,
        "feature_frame_rate": frame_rate,
        "window_samples": w,
        "past_context_samples": p,
        "crossfade_max_out": c_ref * sr // contract_sr,
        "sola_search_max_out": s_ref * sr // contract_sr,
        "overlap_out": (c_ref + s_ref) * sr // contract_sr,
        "margin_16k": margin,
        "front_frames": front,
        "skip_head": skip,
        "return_frames": ret,
        "decoder_frames": ret,
        "hubert_samples": hubert_samples,
        "hubert_frames": (hubert_samples - 400) // hubert_hop + 1,
        "decoder_out_samples": ret * (sr // frame_rate),
        "output_hop": w * sr // in_sr,
        "flow_head": flow_head,
        "latent_frames": front - flow_head,
        "decoder_head": skip - flow_head,
        "rmvpe_frames": ((mel_frames + 31) // 32) * 32,
        "startup_delay_ms": g["startup_delay_ms"],
        "startup_buffer_blocks": g["startup_buffer_blocks"],
        "skip_silence": g["skip_silence"],
    }


class RealtimeChainParamsTest(unittest.TestCase):
    def test_default_48k_geometry_matches_spec(self):
        g = derive_geometry()
        self.assertEqual(g.overlap_out, 2880)
        self.assertEqual(g.margin_16k, 960)
        self.assertEqual(g.front_frames, 280)
        self.assertEqual(g.skip_head, 250)
        self.assertEqual(g.return_frames, 30)
        self.assertEqual(g.decoder_frames, 30)
        self.assertEqual(g.hubert_samples, 44800)
        self.assertEqual(g.hubert_frames, 139)
        self.assertEqual(g.decoder_out_samples, 14400)
        self.assertEqual(g.output_hop, 11520)
        self.assertEqual(g.flow_head, 226)
        self.assertEqual(g.latent_frames, 54)
        self.assertEqual(g.decoder_head, 24)
        self.assertEqual(g.rmvpe_frames, 32)

    def test_40k_and_32k_geometry(self):
        params = load_params()
        g40 = derive_geometry(params, output_sample_rate=40000)
        self.assertEqual(g40.front_frames, 280)
        self.assertEqual(g40.return_frames, 30)
        self.assertEqual(g40.hubert_samples, 44800)
        self.assertEqual(g40.rmvpe_frames, 32)
        self.assertEqual(g40.crossfade_max_out, 2000)
        self.assertEqual(g40.sola_search_max_out, 400)
        self.assertEqual(g40.decoder_out_samples, 12000)
        self.assertEqual(g40.output_hop, 9600)

        g32 = derive_geometry(params, output_sample_rate=32000)
        self.assertEqual(g32.crossfade_max_out, 1600)
        self.assertEqual(g32.sola_search_max_out, 320)
        self.assertEqual(g32.decoder_out_samples, 9600)
        self.assertEqual(g32.output_hop, 7680)

    def test_derivation_follows_json_not_hardcoded_values(self):
        """对拍：改 JSON 数值后推导必须随之变化，且等于独立 oracle。

        .py 若把某个值写死（不读 JSON），本用例在变异参数下失败。
        """
        base = load_params()
        variants = [base]
        for window, past in ((4000, 40000), (3840, 38400), (5440, 41600)):
            changed = copy.deepcopy(base)
            changed["geometry"]["window_samples"] = window
            changed["geometry"]["past_context_samples"] = past
            variants.append(changed)
        overlap = copy.deepcopy(base)
        overlap["geometry"]["crossfade_max_out"] = 1920
        overlap["geometry"]["sola_search_max_out"] = 480
        variants.append(overlap)
        contract = copy.deepcopy(base)
        contract["model_contract"]["hubert_frame_rate"] = 25
        variants.append(contract)

        for params in variants:
            for sr in (None, 32000, 40000, 48000):
                with self.subTest(window=params["geometry"]["window_samples"],
                                  past=params["geometry"]["past_context_samples"],
                                  crossfade=params["geometry"]["crossfade_max_out"],
                                  sr=sr):
                    self.assertEqual(
                        asdict(derive_geometry(params, output_sample_rate=sr)),
                        expected_geometry(params, sr),
                    )

    def test_passthrough_fields_follow_json(self):
        params = copy.deepcopy(load_params())
        params["geometry"]["startup_delay_ms"] = 600
        params["geometry"]["startup_buffer_blocks"] = 2
        params["geometry"]["skip_silence"] = False
        g = derive_geometry(params)
        self.assertEqual(g.startup_delay_ms, 600)
        self.assertEqual(g.startup_buffer_blocks, 2)
        self.assertIs(g.skip_silence, False)

    def test_knob_defaults_and_bounds_match_geometry(self):
        """[rvc.realtime] 旋钮：JSON 默认值必须自身合法，上限由形状换算。

        overlap 锁为定值（引擎资产形状按最大 overlap 固定），不是范围。
        整除模数来自 StreamingRvc 的网格要求（模型帧 100 Hz、契约帧 20 ms），
        与 core/src/config.rs、rvc/include/rvc/realtime_geometry.hpp 同源。
        """
        params = load_params()
        mc, g = params["model_contract"], params["geometry"]
        samples_per_ms = mc["output_sample_rate"] // 1000
        crossfade_max_ms = g["crossfade_max_out"] // samples_per_ms
        sola_max_ms = g["sola_search_max_out"] // samples_per_ms
        overlap_max_ms = crossfade_max_ms + sola_max_ms
        overlap_quantum = 1000 // mc["feature_frame_rate"]
        # 契约帧长单一来源是 C 头，不在此写死第三份。
        frame_h = (Path(__file__).resolve().parents[2]
                   / "core" / "include" / "mozart" / "frame.h").read_text()
        startup_quantum = int(re.search(
            r"#define\s+MOZART_OUTPUT_FRAME_MS\s+(\d+)", frame_h).group(1))

        self.assertLessEqual(g["crossfade_ms"], crossfade_max_ms)
        self.assertLessEqual(g["sola_search_ms"], sola_max_ms)
        self.assertEqual(g["crossfade_ms"] + g["sola_search_ms"], overlap_max_ms)
        self.assertEqual((g["crossfade_ms"] + g["sola_search_ms"]) % overlap_quantum, 0)
        self.assertLessEqual(g["startup_delay_ms"], g["startup_delay_max_ms"])
        self.assertEqual(g["startup_delay_ms"] % startup_quantum, 0)
        self.assertIsInstance(g["skip_silence"], bool)
        # 定值自身也须落在网格上，否则出厂默认会被入口拒绝。
        self.assertEqual(overlap_max_ms % overlap_quantum, 0)
        self.assertEqual(g["startup_delay_max_ms"] % startup_quantum, 0)

    def test_compile_flags(self):
        flags = trtexec_flags()
        self.assertIn("--noTF32", flags)
        self.assertIn("--fp16", flags)
        self.assertIn("--memPoolSize=workspace:1024", flags)
        self.assertIn("--builderOptimizationLevel=3", flags)
        self.assertIn("--profilingVerbosity=detailed", flags)

    def test_compile_flags_follow_json(self):
        params = copy.deepcopy(load_params())
        params["compile"]["fp16"] = True
        params["compile"]["no_tf32"] = False
        params["compile"]["workspace_mib"] = 2048
        params["compile"]["builder_optimization_level"] = 5
        params["compile"]["profiling_verbosity"] = "none"
        flags = trtexec_flags(params)
        self.assertIn("--fp16", flags)
        self.assertNotIn("--noTF32", flags)
        self.assertIn("--memPoolSize=workspace:2048", flags)
        self.assertIn("--builderOptimizationLevel=5", flags)
        self.assertIn("--profilingVerbosity=none", flags)


if __name__ == "__main__":
    unittest.main()

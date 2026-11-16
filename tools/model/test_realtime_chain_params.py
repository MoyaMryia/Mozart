#!/usr/bin/env python3
"""Unit tests for realtime_chain_params.py geometry derivation."""
import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from realtime_chain_params import derive_geometry, load_params, trtexec_flags


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

    def test_compile_flags(self):
        flags = trtexec_flags()
        self.assertIn("--noTF32", flags)
        self.assertIn("--memPoolSize=workspace:1024", flags)
        self.assertIn("--builderOptimizationLevel=3", flags)
        self.assertIn("--profilingVerbosity=detailed", flags)


if __name__ == "__main__":
    unittest.main()

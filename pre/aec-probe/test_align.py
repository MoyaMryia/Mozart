import struct
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

import numpy as np

from align import HEADER, MAGIC, NS, alignment, fit, load


class AlignmentTest(unittest.TestCase):
    def test_drift_compensation_and_boundaries(self):
        rate = 48000
        n = 480
        nominal = NS / rate
        start = 100 * NS
        paths = ["capture", "playback"]
        with TemporaryDirectory() as directory:
            traces = {}
            for name in paths:
                capture = name == "capture"
                period = nominal if capture else nominal * (1 + 150e-6)
                pieces = []
                invalid = dropped = 0
                for index in range(22):
                    if index == 10 and capture:
                        dropped += n
                        continue
                    if index == 8 and not capture:
                        invalid += n
                    sample = index * n
                    epoch = int(index >= 11)
                    t = start + sample * period
                    delay = 100
                    stamp = 0 if index == 8 and not capture else round(t + (n + (delay if capture else -delay)) * nominal)
                    # Waveform defined in wall time, not the sample clock coordinate.
                    x = (t - start) + np.arange(n) * period
                    pcm = np.rint(2500 * np.sin(2 * np.pi * 210 * x / NS)).astype("<i2")
                    pieces += [HEADER.pack(sample, epoch, stamp, delay, n), pcm.tobytes()]
                path = Path(directory) / name
                blob = MAGIC + struct.pack("<I", rate) + b"".join(pieces)
                path.write_bytes(blob)
                stats = {"samples_recorded": 22*n - dropped, "invalid_clock_samples": invalid, "queue_dropped_samples": dropped}
                _, traces[name], rejected = load(path, stats, playback=not capture)
                self.assertEqual(rejected, 0)
                self.assertEqual([s.epoch for s in traces[name]], [0, 1] if capture else [0, 0, 1])
                if not capture:
                    # Later snapshots in epoch 0 establish that the first span's tail played.
                    self.assertEqual(len(traces[name][0].pcm), 8*n)
                    self.assertEqual(len(traces[name][2].pcm), 11*n-100)
                path.write_bytes(blob[:-1])
                with self.assertRaisesRegex(ValueError, "truncated samples"):
                    load(path, stats, playback=not capture)
            mic, ref, facts = alignment(traces["capture"], traces["playback"], 0)
            self.assertEqual(facts["capture_epoch"], 1)
            self.assertEqual(facts["playback_epoch"], 1)
            self.assertGreater(len(mic), 5000)
            self.assertLess(len(mic), 5200)
            self.assertAlmostEqual(facts["capture_rate_over_playback_ppm"], 150, places=1)
            self.assertLess(np.max(np.abs(mic-ref)), 2)

    def test_clock_step_splits_and_implausible_rate_is_rejected(self):
        rate = 48000
        n = 480
        base = 10 * NS
        pcm = np.arange(n, dtype="<i2")
        records = [
            (i*n, 0, base + (i+1)*10_000_000 + (20_000_000 if i >= 4 else 0), 0, pcm)
            for i in range(8)
        ]
        with TemporaryDirectory() as directory:
            path = Path(directory) / "capture"
            path.write_bytes(MAGIC + struct.pack("<I", rate) + b"".join(
                HEADER.pack(sample, epoch, stamp, delay, len(data)) + data.tobytes()
                for sample, epoch, stamp, delay, data in records
            ))
            stats = {"samples_recorded": 8*n, "invalid_clock_samples": 0, "queue_dropped_samples": 0}
            _, spans, rejected = load(path, stats, playback=False)
            self.assertEqual(rejected, 0)
            self.assertEqual([s.first_sample for s in spans], [0, 4*n])
            self.assertEqual([len(s.pcm) for s in spans], [4*n, 4*n])

            # An isolated timestamp with a lower delay cannot confirm earlier queued samples.
            playback = [(i*n, 0, base+i*10_000_000, n, pcm) for i in range(4)]
            playback.append((4*n, 0, base+80_000_000, 0, pcm))
            path.write_bytes(MAGIC + struct.pack("<I", rate) + b"".join(
                HEADER.pack(sample, epoch, stamp, delay, len(data)) + data.tobytes()
                for sample, epoch, stamp, delay, data in playback
            ))
            stats["samples_recorded"] = 5*n
            _, spans, rejected = load(path, stats, playback=True)
            self.assertEqual(rejected, 1)
            self.assertEqual([len(s.pcm) for s in spans], [3*n])
        too_fast = [(i*n, 0, base + (i+1)*9_900_000, 0, pcm) for i in range(8)]
        self.assertIsNone(fit(too_fast, NS/rate, playback=False))


if __name__ == "__main__":
    unittest.main()

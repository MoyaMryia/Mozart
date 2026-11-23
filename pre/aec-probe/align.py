"""Align paired mozart-pre ALSA traces for offline AEC comparison."""

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import struct
import sys
import wave

import numpy as np

MAGIC = b"MZAEC01\n"
HEADER = struct.Struct("<QQQqI")  # sample, epoch, monotonic ns, delay frames, samples
NS = 1_000_000_000
MAX_CLOCK_STEP_MS = 5
MAX_CLOCK_FIT_P95_MS = 2
MAX_CLOCK_RATE_ERROR_PPM = 2000


@dataclass
class Span:
    epoch: int
    first_sample: int
    start_ns: float
    ns_per_sample: float
    pcm: np.ndarray
    fit_p95_ms: float
    clock_group: int

    @property
    def end_ns(self):
        return self.start_ns + (len(self.pcm) - 1) * self.ns_per_sample


def clock_offset(delay, count, playback):
    # The snapshot follows I/O. Go back from the application pointer to the
    # start of the transfer after adjusting for queued or unread samples.
    return (delay if playback else -delay) - count


def fit(records, nominal, playback, clock_group=0):
    if len(records) < 2:
        return None
    first, epoch, _, _, _ = records[0]
    origin = records[0][2]
    idx = np.array([r[0] - first for r in records], dtype=np.float64)
    observed_delta = np.array([
        stamp - origin + clock_offset(delay, len(samples), playback) * nominal
        for _, _, stamp, delay, samples in records
    ])
    slope, offset = np.polyfit(idx, observed_delta, 1)
    if not np.isfinite(slope) or slope <= 0:
        return None
    if abs(nominal / slope - 1) * 1e6 > MAX_CLOCK_RATE_ERROR_PPM:
        return None
    error_ms = np.abs(observed_delta - (idx * slope + offset)) / 1_000_000
    p95 = float(np.quantile(error_ms, .95))
    if p95 > MAX_CLOCK_FIT_P95_MS:
        return None
    return Span(epoch, first, origin + offset, slope, np.concatenate([r[-1] for r in records]), p95, clock_group)


def load(path, summary, playback):
    pending, spans = [], []
    total = invalid = previous_end = gaps = rejected = 0
    previous_epoch = clock_group = 0
    previous_valid = None
    played_through = {}
    with path.open("rb") as source:
        header = source.read(len(MAGIC) + 4)
        if len(header) != len(MAGIC) + 4 or not header.startswith(MAGIC):
            raise ValueError(f"{path}: invalid trace header")
        rate, = struct.unpack_from("<I", header, len(MAGIC))
        if rate == 0:
            raise ValueError(f"{path}: invalid sample rate")

        nominal = NS / rate

        def flush():
            nonlocal rejected
            if not pending:
                return
            span = fit(pending, nominal, playback, clock_group)
            if span is None:
                rejected += 1
            else:
                spans.append(span)
                if playback:
                    # Only fitted segments in the same continuous clock group
                    # can confirm that previously queued samples played.
                    last_played = max(idx + len(pcm) - delay for idx, _, _, delay, pcm in pending)
                    played_through[clock_group] = max(played_through.get(clock_group, 0), last_played)
            pending.clear()

        while raw := source.read(HEADER.size):
            if len(raw) != HEADER.size:
                raise ValueError(f"{path}: truncated record header")
            sample, epoch, stamp, delay, n = HEADER.unpack(raw)
            if not 0 < n <= rate // 50 or sample < previous_end or epoch < previous_epoch or delay < 0:
                raise ValueError(f"{path}: invalid sample range, epoch, or delay")
            samples = source.read(n * 2)
            if len(samples) != n * 2:
                raise ValueError(f"{path}: truncated samples")
            if sample != previous_end or epoch != previous_epoch or stamp == 0:
                flush()
            if epoch != previous_epoch:
                clock_group += 1
                previous_valid = None
            gaps += sample - previous_end
            previous_end = sample + n
            previous_epoch = epoch
            total += n
            if stamp == 0:
                invalid += n
            else:
                offset = clock_offset(delay, n, playback)
                if previous_valid is not None:
                    p_idx, p_stamp, p_offset = previous_valid
                    step_error_ms = abs(stamp - p_stamp + (offset - p_offset - (sample - p_idx)) * nominal) / 1e6
                    if step_error_ms > MAX_CLOCK_STEP_MS:
                        flush()
                        clock_group += 1
                previous_valid = sample, stamp, offset
                pending.append((sample, epoch, stamp, delay, np.frombuffer(samples, dtype="<i2")))
        flush()
    if total != summary["samples_recorded"] or invalid != summary["invalid_clock_samples"] or gaps > summary["queue_dropped_samples"]:
        raise ValueError(f"{path}: trace does not match completion summary")
    if playback:
        for span in spans:
            confirmed = max(0, played_through[span.clock_group] - span.first_sample)
            span.pcm = span.pcm[:confirmed]
        spans = [span for span in spans if len(span.pcm)]
    return rate, spans, rejected


def alignment(capture, playback, delay_ns):
    # Within a span the fitted slope maps between the two device clocks. Select
    # the longest overlap; no silence or invented samples across boundaries.
    # The spans are ordered by sample index, not necessarily by their fitted bounds.
    p = sorted(playback, key=lambda v: v.start_ns)
    best = None
    for c in capture:
        for r in p:
            if r.start_ns + delay_ns > c.end_ns:
                break
            if r.end_ns + delay_ns < c.start_ns:
                continue
            start = max(0, int(np.ceil((r.start_ns + delay_ns - c.start_ns) / c.ns_per_sample)))
            end = min(len(c.pcm), 1 + int(np.floor((r.end_ns + delay_ns - c.start_ns) / c.ns_per_sample)))
            if end > start and (best is None or end - start > best[0]):
                best = (end - start, c, r, start, end)
    if best is None:
        raise ValueError("no continuous overlap between the valid capture and playback spans")
    count, c, r, start, end = best
    # Subtract the large clock offsets first to retain subsample precision.
    relative = c.start_ns - r.start_ns - delay_ns
    index = (relative + np.arange(start, end) * c.ns_per_sample) / r.ns_per_sample
    ref = np.rint(np.interp(index, np.arange(len(r.pcm)), r.pcm)).astype("<i2")
    return c.pcm[start:end], ref, {
        "paired_samples": count,
        "capture_epoch": c.epoch,
        "capture_first_sample": c.first_sample + start,
        "playback_epoch": r.epoch,
        "playback_first_sample": r.first_sample + float(index[0]),
        "capture_clock_fit_p95_ms": c.fit_p95_ms,
        "playback_clock_fit_p95_ms": r.fit_p95_ms,
        "capture_rate_over_playback_ppm": (r.ns_per_sample / c.ns_per_sample - 1) * 1e6,
    }


def wav(path, samples, rate):
    with wave.open(str(path), "wb") as output:
        output.setparams((1, 2, rate, 0, "NONE", "not compressed"))
        output.writeframes(samples.tobytes())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trace", type=Path)
    parser.add_argument("out", type=Path, help="new output directory")
    parser.add_argument("--delay-ms", type=float, default=0, help="additional known acoustic delay")
    args = parser.parse_args()
    if not np.isfinite(args.delay_ms) or args.delay_ms < 0:
        parser.error("--delay-ms must be finite and nonnegative")
    try:
        summary = json.loads((args.trace / "summary.json").read_text())
        rate, c, c_rejected = load(args.trace / "capture.mzaec", summary["capture"], False)
        playback_rate, p, p_rejected = load(args.trace / "playback.mzaec", summary["playback"], True)
        if rate != playback_rate:
            raise ValueError("capture and playback nominal rates differ")
        mic, ref, stats = alignment(c, p, args.delay_ms * 1e6)
        stats.update({
            "sample_rate": rate,
            "delay_ms": args.delay_ms,
            "capture_spans": len(c),
            "playback_spans": len(p),
            "capture_rejected_clock_spans": c_rejected,
            "playback_rejected_clock_spans": p_rejected,
            "capture_coverage": len(mic) / (summary["capture"]["samples_recorded"] + summary["capture"]["queue_dropped_samples"]),
        })
        args.out.mkdir(mode=0o700)
        wav(args.out / "mic.wav", mic, rate)
        wav(args.out / "reference.wav", ref, rate)
        result = json.dumps(stats, indent=2)
        (args.out / "alignment.json").write_text(result + "\n")
        print(result)
    except (OSError, ValueError, KeyError, TypeError) as exc:
        print(f"AEC alignment: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

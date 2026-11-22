#!/usr/bin/env python3
"""Unit tests for tools/jetson-audio.py (stdlib only, no audio device or SSH)."""
from __future__ import annotations

from contextlib import redirect_stderr
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import types
import unittest
from unittest import mock
import wave

_MODULE_PATH = Path(__file__).resolve().parent / "jetson-audio.py"
_SPEC = importlib.util.spec_from_file_location("jetson_audio", _MODULE_PATH)
assert _SPEC is not None and _SPEC.loader is not None
ja = importlib.util.module_from_spec(_SPEC)
sys.modules["jetson_audio"] = ja
_SPEC.loader.exec_module(ja)


class FakeStream(io.BytesIO):
    """Simulates partial reads and short pipe writes."""

    def __init__(self, initial: bytes = b"", max_chunk: int = 700) -> None:
        super().__init__(initial)
        self.max_chunk = max_chunk
        self.written = bytearray()

    def read(self, size: int | None = -1) -> bytes:
        if size is None or size < 0:
            return super().read()
        return super().read(min(size, self.max_chunk))

    def write(self, data: bytes | bytearray | memoryview) -> int:
        chunk = bytes(data[: self.max_chunk])
        self.written.extend(chunk)
        return len(chunk)


class FakePopen:
    """In-memory mock for subprocess.Popen in worker and run_bridge tests."""

    def __init__(
        self,
        *,
        stdout_data: bytes = b"",
        stderr_data: bytes = b"",
        returncode: int | None = None,
    ) -> None:
        self.stdout = io.BytesIO(stdout_data)
        self.stdin = FakeStream()
        self.stderr = io.BytesIO(stderr_data)
        self.returncode = returncode
        self.terminated = False
        self.killed = False

    def poll(self) -> int | None:
        return self.returncode

    def terminate(self) -> None:
        self.terminated = True
        if self.returncode is None:
            self.returncode = -15

    def kill(self) -> None:
        self.killed = True
        if self.returncode is None:
            self.returncode = -9

    def wait(self, timeout: float | None = None) -> int:  # noqa: ARG002
        if self.returncode is None:
            self.returncode = 0
        return self.returncode


class FakePortAudioError(Exception):
    """Test double matching sounddevice.PortAudioError."""


class FakeAudioStream:
    """Test double for sounddevice RawOutputStream / RawInputStream."""

    def __init__(
        self,
        *,
        callback,
        finished_callback=None,
        fail_on_start: bool = False,
        fail_on_stop: bool = False,
        **kwargs,  # noqa: ARG002
    ) -> None:
        self.callback = callback
        self.finished_callback = finished_callback
        self.fail_on_start = fail_on_start
        self.fail_on_stop = fail_on_stop
        self.active = False
        self.started = False
        self.stopped = False
        self.closed = False

    def start(self) -> None:
        if self.fail_on_start:
            raise FakePortAudioError("device unavailable")
        self.started = True
        self.active = True

    def stop(self) -> None:
        self.stopped = True
        self.active = False
        if self.fail_on_stop:
            raise FakePortAudioError("stop failed")

    def close(self) -> None:
        self.closed = True


def _make_fake_sd(
    *,
    fail_out_start: bool = False,
    fail_in_start: bool = False,
    fail_out_stop: bool = False,
    created_streams: list[FakeAudioStream] | None = None,
) -> types.SimpleNamespace:
    def make_out(**kwargs) -> FakeAudioStream:
        stream = FakeAudioStream(
            fail_on_start=fail_out_start,
            fail_on_stop=fail_out_stop,
            **kwargs,
        )
        if created_streams is not None:
            created_streams.append(stream)
        return stream

    def make_in(**kwargs) -> FakeAudioStream:
        stream = FakeAudioStream(fail_on_start=fail_in_start, **kwargs)
        if created_streams is not None:
            created_streams.append(stream)
        return stream

    return types.SimpleNamespace(
        PortAudioError=FakePortAudioError,
        RawOutputStream=make_out,
        RawInputStream=make_in,
    )


class FakeRunner:
    """In-memory mock for RemoteController SSH commands."""

    def __init__(
        self,
        *,
        aloop_loaded: bool = True,
        status_payload: dict | None = None,
        status_rc: int = 0,
        fail_device_use_kinds: set[str] | None = None,
    ) -> None:
        self.aloop_loaded = aloop_loaded
        self.status_payload = status_payload
        self.status_rc = status_rc
        self.fail_device_use_kinds = fail_device_use_kinds or set()
        self.calls: list[list[str]] = []
        self.remote_cmds: list[str] = []

    def __call__(
        self, cmd: list[str], _timeout: float
    ) -> subprocess.CompletedProcess[str]:
        self.calls.append(cmd)
        remote_cmd = cmd[-1]
        self.remote_cmds.append(remote_cmd)

        if "[ -d /proc/asound/Loopback ]" in remote_cmd:
            rc = 0 if self.aloop_loaded else 1
            stderr = "" if rc == 0 else "No such file or directory"
            return subprocess.CompletedProcess(cmd, rc, "", stderr)

        if " status --json" in remote_cmd:
            if self.status_rc != 0:
                return subprocess.CompletedProcess(
                    cmd, self.status_rc, "", "mozart-ctl: daemon unreachable"
                )
            body = "" if self.status_payload is None else json.dumps(self.status_payload)
            return subprocess.CompletedProcess(cmd, 0, body, "")

        if " device use " in remote_cmd:
            for kind in self.fail_device_use_kinds:
                if f" device use {kind} " in remote_cmd:
                    return subprocess.CompletedProcess(
                        cmd, 4, "", f"mozart-ctl: rejected {kind} switch"
                    )
            return subprocess.CompletedProcess(cmd, 0, "{}", "")

        raise AssertionError(f"unexpected remote command: {remote_cmd}")


class ParseArgsTests(unittest.TestCase):
    def test_requires_target_and_prefers_positional_over_env(self) -> None:
        with self.assertRaises(ja.UsageError):
            ja.parse_args([], {})
        with self.assertRaises(ja.UsageError):
            ja.parse_args([], {"MOZART_JETSON": "   "})

        cfg_env = ja.parse_args([], {"MOZART_JETSON": "user@192.0.2.10"})
        self.assertEqual(cfg_env.target, "user@192.0.2.10")
        self.assertEqual(cfg_env.mode, ja.MODE_PLAY)
        self.assertEqual(cfg_env.pb_sub, ja.DEFAULT_PB_SUB)
        self.assertEqual(cfg_env.cap_sub, ja.DEFAULT_CAP_SUB)
        self.assertEqual(cfg_env.buffer_ms, ja.DEFAULT_BUFFER_MS)
        self.assertEqual(cfg_env.runtime_dir, ja.DEFAULT_RUNTIME_DIR)
        self.assertEqual(
            cfg_env.remote_config,
            f"{ja.DEFAULT_RUNTIME_DIR}/mozart.toml",
        )

        cfg_cli = ja.parse_args(["other@host"], {"MOZART_JETSON": "user@192.0.2.10"})
        self.assertEqual(cfg_cli.target, "other@host")

    def test_empty_env_vars_fall_back_to_defaults_while_empty_cli_fails(self) -> None:
        cfg = ja.parse_args(
            ["user@host"],
            {
                "PB_SUB": "",
                "CAP_SUB": "   ",
                "MOZART_RUNTIME_DIR": "",
                "MOZART_REMOTE_CONFIG": "",
            },
        )
        self.assertEqual(cfg.pb_sub, ja.DEFAULT_PB_SUB)
        self.assertEqual(cfg.cap_sub, ja.DEFAULT_CAP_SUB)
        self.assertEqual(cfg.runtime_dir, ja.DEFAULT_RUNTIME_DIR)
        self.assertEqual(cfg.remote_config, f"{ja.DEFAULT_RUNTIME_DIR}/mozart.toml")

        with self.assertRaises(ja.UsageError):
            ja.parse_args(["user@host", "--config", "   "], {})
        with self.assertRaises(ja.UsageError):
            ja.parse_args(["user@host", "--runtime-dir", ""], {})

    def test_rejects_option_like_target_from_env(self) -> None:
        with self.assertRaises(ja.UsageError):
            ja.parse_args([], {"MOZART_JETSON": "-oProxyCommand=bad"})

    def test_rejects_record_in_mic_mode_and_directory_path(self) -> None:
        with self.assertRaises(ja.UsageError):
            ja.parse_args(
                ["user@host", "--mode", "mic", "--record", "out.wav"],
                {},
            )
        with tempfile.TemporaryDirectory() as tmpdir:
            with self.assertRaises(ja.UsageError):
                ja.parse_args(["user@host", "--record", tmpdir], {})

        cfg = ja.parse_args(
            ["user@host", "--mode", "duplex", "--record", "out.wav"],
            {},
        )
        self.assertEqual(cfg.record_path, Path("out.wav"))

    def test_validates_subdev_bounds_and_duplex_conflict(self) -> None:
        bad_low = str(ja.SUBDEV_MIN - 1)
        bad_high = str(ja.SUBDEV_MAX + 1)
        for bad in (bad_low, bad_high, "abc"):
            with self.assertRaises(ja.UsageError):
                ja.parse_args(["user@host", "--pb-sub", bad], {})
            with self.assertRaises(ja.UsageError):
                ja.parse_args(["user@host"], {"CAP_SUB": bad})

        with self.assertRaises(ja.UsageError):
            ja.parse_args(
                ["user@host", "--mode", "duplex", "--pb-sub", "6", "--cap-sub", "6"],
                {},
            )
        # In play mode cap_sub is unused, so pb_sub == cap_sub is allowed.
        cfg = ja.parse_args(["user@host", "--pb-sub", "6", "--cap-sub", "6"], {})
        self.assertEqual(cfg.pb_sub, 6)

    def test_validates_buffer_ms_and_runtime_config_overrides(self) -> None:
        bad_low = str(ja.BUFFER_MS_MIN - 1)
        bad_high = str(ja.BUFFER_MS_MAX + 1)
        for bad in (bad_low, bad_high, "fast"):
            with self.assertRaises(ja.UsageError):
                ja.parse_args(["user@host", "--buffer-ms", bad], {})

        cfg = ja.parse_args(
            ["user@host", "--runtime-dir", "/opt/mozart/", "--buffer-ms", "120"],
            {"MOZART_RUNTIME_DIR": "~/ignored"},
        )
        self.assertEqual(cfg.runtime_dir, "/opt/mozart/")
        self.assertEqual(cfg.remote_config, "/opt/mozart/mozart.toml")
        self.assertEqual(cfg.buffer_ms, 120)

        cfg_env = ja.parse_args(
            ["user@host"],
            {
                "MOZART_RUNTIME_DIR": "~/iso/runtime",
                "MOZART_REMOTE_CONFIG": "/etc/mozart/custom.toml",
            },
        )
        self.assertEqual(cfg_env.runtime_dir, "~/iso/runtime")
        self.assertEqual(cfg_env.remote_config, "/etc/mozart/custom.toml")


class RemoteCommandAndLoopbackTests(unittest.TestCase):
    def test_quote_remote_path(self) -> None:
        self.assertEqual(ja.quote_remote_path("~"), '"$HOME"')
        self.assertEqual(
            ja.quote_remote_path("~/mozart-runtime/bin/mozart-ctl"),
            '"$HOME"/mozart-runtime/bin/mozart-ctl',
        )
        self.assertEqual(
            ja.quote_remote_path("~/my dir/mozart.toml"),
            '"$HOME"/\'my dir/mozart.toml\'',
        )
        self.assertEqual(
            ja.quote_remote_path("/opt/a b/mozart.toml"),
            "'/opt/a b/mozart.toml'",
        )
        self.assertEqual(ja.join_remote_path("/", "mozart.toml"), "/mozart.toml")

    def test_parse_loopback_device(self) -> None:
        self.assertEqual(
            ja.parse_loopback_device("plughw:CARD=Loopback,DEV=0,SUBDEV=3"),
            (0, 3),
        )
        self.assertEqual(
            ja.parse_loopback_device("hw:CARD=Loopback,DEV=1,SUBDEV=7"),
            (1, 7),
        )
        self.assertEqual(ja.parse_loopback_device("plughw:Loopback,0,2"), (0, 2))
        self.assertEqual(ja.parse_loopback_device("hw:Loopback,1,0"), (1, 0))
        self.assertIsNone(ja.parse_loopback_device("plughw:CARD=Loopback,DEV=0"))
        self.assertIsNone(
            ja.parse_loopback_device("plughw:CARD=Loopback,DEV=0,SUBDEV=8")
        )
        self.assertIsNone(ja.parse_loopback_device("default"))
        self.assertIsNone(ja.parse_loopback_device("plughw:CARD=USB,DEV=0"))

    def test_ssh_and_alsa_commands_match_48k_s16le_2ch_contract(self) -> None:
        ssh_cmd = ja.ssh_base_cmd("user@host")
        self.assertIn("-T", ssh_cmd)
        self.assertIn("Compression=no", ssh_cmd)
        self.assertIn("BatchMode=yes", ssh_cmd)
        self.assertEqual(ssh_cmd[-1], "user@host")

        rec = ja.remote_arecord_cmd("plughw:CARD=Loopback,DEV=1,SUBDEV=7")
        self.assertIn("exec arecord -q -D plughw:CARD=Loopback,DEV=1,SUBDEV=7", rec)
        self.assertIn("-f S16_LE -c 2 -r 48000 -t raw", rec)

        play = ja.remote_aplay_cmd("plughw:CARD=Loopback,DEV=0,SUBDEV=6")
        self.assertIn("exec aplay -q -D plughw:CARD=Loopback,DEV=0,SUBDEV=6", play)
        self.assertIn("-f S16_LE -c 2 -r 48000 -t raw", play)


class FrameQueueTests(unittest.TestCase):
    def test_compute_queue_bounds(self) -> None:
        self.assertEqual(ja.compute_queue_bounds(20), (1, 6))
        self.assertEqual(ja.compute_queue_bounds(60), (3, 6))
        self.assertEqual(ja.compute_queue_bounds(50), (3, 6))
        self.assertEqual(ja.compute_queue_bounds(200), (10, 20))
        self.assertEqual(ja.compute_queue_bounds(500), (25, 50))

    def test_priming_drop_oldest_and_underrun(self) -> None:
        q = ja.FrameQueue(target_frames=2, max_frames=3)
        f1 = b"\x01" * 3840
        f2 = b"\x02" * 3840
        f3 = b"\x03" * 3840
        f4 = b"\x04" * 3840

        q.push(f1)
        self.assertEqual(q.pop_playback(), b"\x00" * 3840)
        self.assertEqual(q.underruns, 0)

        q.push(f2)
        q.push(f3)
        q.push(f4)
        self.assertEqual(q.dropped, 1)

        self.assertEqual(q.pop_playback(), f2)
        self.assertEqual(q.pop_playback(), f3)
        self.assertEqual(q.pop_playback(), f4)
        self.assertEqual(q.pop_playback(), b"\x00" * 3840)
        self.assertEqual(q.underruns, 1)

        q.push(f1)
        self.assertEqual(q.pop_playback(), b"\x00" * 3840)
        q.push(f2)
        self.assertEqual(q.pop_playback(), f1)

    def test_pop_blocking_returns_none_immediately_when_stopped(self) -> None:
        q = ja.FrameQueue(target_frames=1, max_frames=6)
        stop = threading.Event()
        frame = b"\x05" * 3840
        q.push(frame)
        self.assertEqual(q.pop_blocking(stop, timeout=0.01), frame)
        q.push(frame)
        stop.set()
        # Even with buffered data, stop_event must short-circuit pop_blocking.
        self.assertIsNone(q.pop_blocking(stop, timeout=0.01))

    def test_buffer_alignment_helpers(self) -> None:
        f1 = bytes([1]) * 3840
        f2 = bytes([2]) * 3840
        frames = iter([f1, f2])

        carry = bytearray()
        out1 = bytearray(2000)
        out2 = bytearray(2000)
        out3 = bytearray(3680)
        ja.fill_playback_buffer(carry, out1, lambda: next(frames))
        ja.fill_playback_buffer(carry, out2, lambda: next(frames))
        ja.fill_playback_buffer(carry, out3, lambda: next(frames))
        self.assertEqual(bytes(out1) + bytes(out2) + bytes(out3), f1 + f2)
        self.assertEqual(len(carry), 0)

        in_carry = bytearray()
        captured: list[bytes] = []
        raw = f1 + f2
        ja.drain_capture_buffer(in_carry, raw[:2500], captured.append)
        self.assertEqual(captured, [])
        ja.drain_capture_buffer(in_carry, raw[2500:5000], captured.append)
        self.assertEqual(captured, [f1])
        ja.drain_capture_buffer(in_carry, raw[5000:], captured.append)
        self.assertEqual(captured, [f1, f2])
        self.assertEqual(len(in_carry), 0)


class DeviceLeaseTests(unittest.TestCase):
    def _ctl(self, runner: FakeRunner) -> ja.RemoteController:
        return ja.RemoteController(
            "user@host", "~/mozart-runtime", "~/mozart-runtime/mozart.toml", runner=runner
        )

    def test_reuses_existing_loopback_without_device_use(self) -> None:
        runner = FakeRunner(
            status_payload={
                "pre": {
                    "playback_device": "plughw:CARD=Loopback,DEV=0,SUBDEV=3",
                    "capture_device": "plughw:CARD=USB,DEV=0",
                }
            }
        )
        cfg = ja.parse_args(["user@host", "--mode", "play"], {})
        ctl = self._ctl(runner)
        lease = ja.acquire_devices(cfg, ctl)
        self.assertEqual(lease.playback_peer, "plughw:CARD=Loopback,DEV=1,SUBDEV=3")
        self.assertIsNone(lease.capture_peer)
        self.assertEqual(lease.switches, [])
        self.assertEqual(lease.release(ctl), [])
        self.assertFalse(any(" device use " in c for c in runner.remote_cmds))

    def test_switches_and_restores_non_loopback_device(self) -> None:
        runner = FakeRunner(
            status_payload={
                "pre": {
                    "playback_device": "plughw:CARD=PCH,DEV=0",
                    "capture_device": "plughw:CARD=USB,DEV=0",
                }
            }
        )
        cfg = ja.parse_args(["user@host", "--mode", "play"], {})
        ctl = self._ctl(runner)
        lease = ja.acquire_devices(cfg, ctl)
        self.assertEqual(lease.playback_peer, "plughw:CARD=Loopback,DEV=1,SUBDEV=7")
        self.assertEqual(lease.switches, [("playback", "plughw:CARD=PCH,DEV=0")])
        self.assertEqual(lease.release(ctl), [])
        use_cmds = [c for c in runner.remote_cmds if " device use " in c]
        self.assertEqual(len(use_cmds), 2)
        self.assertIn("playback plughw:CARD=Loopback,DEV=0,SUBDEV=7", use_cmds[0])
        self.assertIn("playback plughw:CARD=PCH,DEV=0", use_cmds[1])

    def test_duplex_rolls_back_first_switch_on_second_failure(self) -> None:
        runner = FakeRunner(
            status_payload={
                "pre": {
                    "playback_device": "plughw:CARD=PCH,DEV=0",
                    "capture_device": "file:/tmp/in.wav",
                }
            },
            fail_device_use_kinds={"capture"},
        )
        cfg = ja.parse_args(["user@host", "--mode", "duplex"], {})
        ctl = self._ctl(runner)
        with self.assertRaises(ja.BridgeError):
            ja.acquire_devices(cfg, ctl)

        use_cmds = [c for c in runner.remote_cmds if " device use " in c]
        self.assertEqual(len(use_cmds), 3)
        self.assertIn("playback plughw:CARD=Loopback,DEV=0,SUBDEV=7", use_cmds[0])
        self.assertIn("capture plughw:CARD=Loopback,DEV=1,SUBDEV=6", use_cmds[1])
        self.assertIn("playback plughw:CARD=PCH,DEV=0", use_cmds[2])

    def test_duplex_avoids_self_loop_collision_and_transient_overlap(self) -> None:
        runner_a = FakeRunner(
            status_payload={
                "pre": {
                    "playback_device": "plughw:CARD=Loopback,DEV=0,SUBDEV=3",
                    "capture_device": "plughw:CARD=Loopback,DEV=1,SUBDEV=3",
                }
            }
        )
        cfg = ja.parse_args(["user@host", "--mode", "duplex"], {})
        lease_a = ja.acquire_devices(cfg, self._ctl(runner_a))
        self.assertEqual(lease_a.playback_peer, "plughw:CARD=Loopback,DEV=1,SUBDEV=3")
        self.assertEqual(lease_a.capture_peer, "plughw:CARD=Loopback,DEV=0,SUBDEV=6")
        self.assertEqual(
            lease_a.switches, [("capture", "plughw:CARD=Loopback,DEV=1,SUBDEV=3")]
        )

        # Capture currently occupies pb_sub=7 while playback is a card:
        # must vacate capture to 6 BEFORE switching playback to 7.
        runner_b = FakeRunner(
            status_payload={
                "pre": {
                    "playback_device": "plughw:CARD=PCH,DEV=0",
                    "capture_device": "plughw:CARD=Loopback,DEV=1,SUBDEV=7",
                }
            }
        )
        lease_b = ja.acquire_devices(cfg, self._ctl(runner_b))
        use_cmds = [c for c in runner_b.remote_cmds if " device use " in c]
        self.assertEqual(len(use_cmds), 2)
        self.assertIn("capture plughw:CARD=Loopback,DEV=1,SUBDEV=6", use_cmds[0])
        self.assertIn("playback plughw:CARD=Loopback,DEV=0,SUBDEV=7", use_cmds[1])
        self.assertEqual(lease_b.playback_peer, "plughw:CARD=Loopback,DEV=1,SUBDEV=7")
        self.assertEqual(lease_b.capture_peer, "plughw:CARD=Loopback,DEV=0,SUBDEV=6")

    def test_no_switch_skips_mozart_ctl(self) -> None:
        runner = FakeRunner(aloop_loaded=True, status_payload=None)
        cfg = ja.parse_args(["user@host", "--mode", "duplex", "--no-switch"], {})
        lease = ja.acquire_devices(cfg, self._ctl(runner))
        self.assertEqual(lease.playback_peer, "plughw:CARD=Loopback,DEV=1,SUBDEV=7")
        self.assertEqual(lease.capture_peer, "plughw:CARD=Loopback,DEV=0,SUBDEV=6")
        self.assertEqual(len(runner.remote_cmds), 1)
        self.assertIn("/proc/asound/Loopback", runner.remote_cmds[0])

    def test_missing_aloop_and_missing_pre_raise_clear_errors(self) -> None:
        cfg = ja.parse_args(["user@host"], {})
        with self.assertRaises(ja.BridgeError) as ctx_aloop:
            ja.acquire_devices(cfg, self._ctl(FakeRunner(aloop_loaded=False)))
        self.assertIn("snd-aloop", str(ctx_aloop.exception))

        with self.assertRaises(ja.BridgeError) as ctx_pre:
            ja.acquire_devices(
                cfg,
                self._ctl(FakeRunner(status_payload={"pre": None})),
            )
        self.assertIn("mozart-pre", str(ctx_pre.exception))

    def test_release_continues_all_directions_and_reports_failures(self) -> None:
        runner = FakeRunner(
            status_payload={
                "pre": {
                    "playback_device": "plughw:CARD=PCH,DEV=0",
                    "capture_device": "plughw:CARD=USB,DEV=0",
                }
            }
        )
        cfg = ja.parse_args(["user@host", "--mode", "duplex"], {})
        ctl = self._ctl(runner)
        lease = ja.acquire_devices(cfg, ctl)

        runner.fail_device_use_kinds = {"capture"}
        errors = lease.release(ctl)
        self.assertEqual(len(errors), 1)
        self.assertIn("plughw:CARD=USB,DEV=0", errors[0])
        self.assertTrue(
            any("playback plughw:CARD=PCH,DEV=0" in c for c in runner.remote_cmds)
        )


class WavAndStreamIoTests(unittest.TestCase):
    def test_wav_recorder_writes_valid_48k_stereo_wav(self) -> None:
        # Verify against independent WAV contract literals: 2 ch, 16-bit (2 B), 48 kHz, 960*2 frames.
        f1 = b"\x11\x22" * 1920
        f2 = b"\x33\x44" * 1920
        with tempfile.TemporaryDirectory() as tmpdir:
            out_path = Path(tmpdir) / "nested" / "test.wav"
            rec = ja.WavRecorder(out_path)
            rec.write(f1)
            rec.write(f2)
            rec.close()
            rec.close()

            with wave.open(str(out_path), "rb") as reader:
                self.assertEqual(reader.getnchannels(), 2)
                self.assertEqual(reader.getsampwidth(), 2)
                self.assertEqual(reader.getframerate(), 48000)
                self.assertEqual(reader.getnframes(), 1920)
                self.assertEqual(reader.readframes(1920), f1 + f2)

    def test_read_exact_and_write_all_handle_partial_chunks(self) -> None:
        payload = bytes(range(256)) * 20
        in_stream = FakeStream(payload, max_chunk=500)
        first = ja.read_exact(in_stream, 3840)
        self.assertEqual(first, payload[:3840])
        self.assertIsNone(ja.read_exact(in_stream, 3840))

        out_stream = FakeStream(max_chunk=400)
        ja.write_all(out_stream, payload[:3840])
        self.assertEqual(bytes(out_stream.written), payload[:3840])


class BridgeRuntimeAndErrorTests(unittest.TestCase):
    def test_import_sounddevice_reports_import_and_portaudio_os_errors(self) -> None:
        with mock.patch("builtins.__import__", side_effect=ImportError("no module")):
            with self.assertRaises(ja.BridgeError) as ctx_imp:
                ja.import_sounddevice()
            self.assertIn("uv run", str(ctx_imp.exception))

        with mock.patch(
            "builtins.__import__", side_effect=OSError("PortAudio library not found")
        ):
            with self.assertRaises(ja.BridgeError) as ctx_os:
                ja.import_sounddevice()
            self.assertIn("libportaudio2", str(ctx_os.exception))

    def test_bridge_state_deduplicates_proc_and_preserves_secondary_errors(self) -> None:
        stop = threading.Event()
        state = ja.BridgeState(stop)
        proc = FakePopen(stderr_data=b"ssh: connection lost", returncode=255)
        state.fail_from_proc("下行 SSH 音频流", proc)
        state.fail_from_proc("下行 SSH 音频流", proc)
        state.fail("关闭 WAV 文件失败：disk full")
        self.assertTrue(stop.is_set())
        self.assertEqual(len(state.errors), 2)
        self.assertIn("ssh: connection lost", state.errors[0])
        self.assertIn("disk full", state.errors[1])

    def test_workers_handle_eof_and_pipe_errors(self) -> None:
        frame = b"\x0a\x0b" * 1920
        stop = threading.Event()
        state = ja.BridgeState(stop)
        q = ja.FrameQueue(1, 6)
        with tempfile.TemporaryDirectory() as tmpdir:
            wav_path = Path(tmpdir) / "worker.wav"
            rec = ja.WavRecorder(wav_path)
            proc = FakePopen(stdout_data=frame, stderr_data=b"arecord exited", returncode=1)
            ja._downlink_worker(proc, q, rec, state)
            rec.close()
            self.assertTrue(stop.is_set())
            self.assertEqual(q.pop_playback(), frame)
            self.assertIn("arecord exited", state.format_errors())

        stop_up = threading.Event()
        state_up = ja.BridgeState(stop_up)
        q_up = ja.FrameQueue(1, 6)
        q_up.push(frame)
        proc_up = FakePopen(stderr_data=b"broken pipe", returncode=1)

        def broken_write(_data) -> int:
            raise BrokenPipeError("broken")

        proc_up.stdin.write = broken_write  # type: ignore[method-assign]
        ja._uplink_worker(proc_up, q_up, state_up)
        self.assertTrue(stop_up.is_set())
        self.assertIn("broken pipe", state_up.format_errors())

    def test_run_bridge_restores_and_warns_even_when_stream_start_fails(self) -> None:
        runner = FakeRunner(
            status_payload={
                "pre": {
                    "playback_device": "plughw:CARD=PCH,DEV=0",
                    "capture_device": "plughw:CARD=USB,DEV=0",
                }
            }
        )
        fake_sd = _make_fake_sd(fail_out_start=True)
        cfg = ja.parse_args(["user@host", "--mode", "play"], {})

        # Simulate failure during restore after stream open error.
        orig_call = runner.__call__

        def fail_on_restore(cmd: list[str], timeout: float) -> subprocess.CompletedProcess[str]:
            if "playback plughw:CARD=PCH,DEV=0" in cmd[-1]:
                runner.fail_device_use_kinds.add("playback")
            return orig_call(cmd, timeout)

        stderr_buf = io.StringIO()
        with (
            mock.patch.object(ja, "import_sounddevice", return_value=fake_sd),
            mock.patch.object(ja, "_default_runner", side_effect=fail_on_restore),
            mock.patch.object(ja, "_spawn_ssh_stream", return_value=FakePopen()),
            redirect_stderr(stderr_buf),
        ):
            with self.assertRaises(ja.BridgeError) as ctx:
                ja.run_bridge(cfg)

        self.assertIn("打开系统默认扬声器失败", str(ctx.exception))
        self.assertIn("还原设备告警", stderr_buf.getvalue())
        self.assertIn("plughw:CARD=PCH,DEV=0", stderr_buf.getvalue())

    def test_run_bridge_detects_runtime_audio_stream_abort_and_exits_nonzero(self) -> None:
        runner = FakeRunner(
            status_payload={
                "pre": {
                    "playback_device": "plughw:CARD=PCH,DEV=0",
                    "capture_device": "plughw:CARD=USB,DEV=0",
                }
            }
        )
        streams: list[FakeAudioStream] = []
        fake_sd = _make_fake_sd(created_streams=streams)
        down_proc = FakePopen(stdout_data=b"")

        # Keep downlink worker blocked until stop, while simulating audio device unplug.
        gate = threading.Event()

        def blocking_read(_size: int = -1) -> bytes:
            gate.wait(timeout=2.0)
            return b""

        down_proc.stdout.read = blocking_read  # type: ignore[method-assign]
        orig_terminate = down_proc.terminate

        def terminate_and_unblock() -> None:
            orig_terminate()
            gate.set()

        down_proc.terminate = terminate_and_unblock  # type: ignore[method-assign]

        def abort_stream() -> None:
            for _ in range(50):
                if streams and streams[0].started:
                    streams[0].active = False
                    assert streams[0].finished_callback is not None
                    streams[0].finished_callback()
                    return
                threading.Event().wait(0.01)

        cfg = ja.parse_args(["user@host", "--mode", "play"], {})
        stderr_buf = io.StringIO()
        t = threading.Thread(target=abort_stream, daemon=True)
        t.start()
        with (
            mock.patch.object(ja, "import_sounddevice", return_value=fake_sd),
            mock.patch.object(ja, "_default_runner", side_effect=runner),
            mock.patch.object(ja, "_spawn_ssh_stream", return_value=down_proc),
            redirect_stderr(stderr_buf),
        ):
            rc = ja.run_bridge(cfg)
        t.join(timeout=1.0)

        self.assertEqual(rc, ja.EXIT_ERROR)
        self.assertIn("扬声器音频流意外中断", stderr_buf.getvalue())
        self.assertTrue(streams[0].stopped and streams[0].closed)
        use_cmds = [c for c in runner.remote_cmds if " device use " in c]
        self.assertEqual(len(use_cmds), 2)
        self.assertIn("playback plughw:CARD=PCH,DEV=0", use_cmds[1])

    def test_run_bridge_graceful_stop_cleans_all_resources_and_returns_zero(self) -> None:
        runner = FakeRunner(
            status_payload={
                "pre": {
                    "playback_device": "plughw:CARD=Loopback,DEV=0,SUBDEV=7",
                    "capture_device": "plughw:CARD=Loopback,DEV=1,SUBDEV=6",
                }
            }
        )
        streams: list[FakeAudioStream] = []
        fake_sd = _make_fake_sd(created_streams=streams)
        down_proc = FakePopen()
        up_proc = FakePopen()
        gate = threading.Event()

        def blocking_read(_size: int = -1) -> bytes:
            gate.wait(timeout=2.0)
            return b""

        down_proc.stdout.read = blocking_read  # type: ignore[method-assign]
        orig_terminate = down_proc.terminate

        def terminate_and_unblock() -> None:
            orig_terminate()
            gate.set()

        down_proc.terminate = terminate_and_unblock  # type: ignore[method-assign]

        spawned = iter([down_proc, up_proc])
        installed: dict[int, object] = {}

        def fake_signal(sig, handler):
            installed[sig] = handler
            return None

        def trigger_sigint() -> None:
            for _ in range(50):
                if streams and len(streams) == 2 and streams[1].started:
                    import signal as _sig

                    handler = installed.get(_sig.SIGINT)
                    assert callable(handler)
                    handler(_sig.SIGINT, None)
                    return
                threading.Event().wait(0.01)

        cfg = ja.parse_args(["user@host", "--mode", "duplex"], {})
        stderr_buf = io.StringIO()
        t = threading.Thread(target=trigger_sigint, daemon=True)
        t.start()
        with (
            mock.patch.object(ja, "import_sounddevice", return_value=fake_sd),
            mock.patch.object(ja, "_default_runner", side_effect=runner),
            mock.patch.object(ja, "_spawn_ssh_stream", side_effect=lambda *a, **k: next(spawned)),
            mock.patch("signal.signal", side_effect=fake_signal),
            mock.patch("signal.getsignal", return_value=None),
            redirect_stderr(stderr_buf),
        ):
            rc = ja.run_bridge(cfg)
        t.join(timeout=1.0)

        self.assertEqual(rc, ja.EXIT_OK)
        self.assertTrue(down_proc.terminated and up_proc.terminated)
        self.assertTrue(down_proc.stdout.closed and up_proc.stdin.closed)
        self.assertTrue(all(s.stopped and s.closed for s in streams))
        self.assertNotIn("错误", stderr_buf.getvalue())

    def test_main_exit_codes(self) -> None:
        with redirect_stderr(io.StringIO()):
            self.assertEqual(ja.main([]), ja.EXIT_USAGE)
        with (
            mock.patch.object(ja, "run_bridge", side_effect=ja.BridgeError("boom")),
            redirect_stderr(io.StringIO()),
        ):
            self.assertEqual(ja.main(["user@host"]), ja.EXIT_ERROR)
        with mock.patch.object(ja, "run_bridge", return_value=ja.EXIT_OK):
            self.assertEqual(ja.main(["user@host"]), ja.EXIT_OK)


if __name__ == "__main__":
    unittest.main()

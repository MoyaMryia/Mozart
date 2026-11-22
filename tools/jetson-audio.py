#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "sounddevice>=0.4.6",
# ]
# ///
"""跨平台本机↔Jetson SSH 音频桥接工具（直连系统默认扬声器与麦克风）。

通过纯 SSH 原始 PCM 管道与 Jetson 的 snd-aloop 虚拟声卡桥接音频，支持：
- play   : Jetson 播放输出 → 本机默认扬声器（可选 --record 同步录 WAV）
- mic    : 本机默认麦克风 → Jetson 采集输入
- duplex : 双向全双工实时音频传输

默认自动检测并复用已在 Loopback 上的方向，否则经 mozart-ctl device use
临时热切并在退出时自动还原；可用 --no-switch 跳过控制面直连子设备。
"""
from __future__ import annotations

import argparse
from collections import deque
from dataclasses import dataclass, field
import json
import math
import os
from pathlib import Path
import posixpath
import re
import shlex
import signal
import subprocess
import sys
import threading
from types import FrameType, ModuleType
from typing import BinaryIO, Callable, Mapping
import wave

SAMPLE_RATE = 48000
CHANNELS = 2
SAMPLE_WIDTH = 2  # S16_LE
FRAME_MS = 20
FRAME_SAMPLES = SAMPLE_RATE * FRAME_MS // 1000  # 960
FRAME_BYTES = FRAME_SAMPLES * CHANNELS * SAMPLE_WIDTH  # 3840
SILENCE_FRAME = b"\x00" * FRAME_BYTES

SUBDEV_MIN = 0
SUBDEV_MAX = 7
DEFAULT_CAP_SUB = 6
DEFAULT_PB_SUB = 7

BUFFER_MS_MIN = 20
BUFFER_MS_MAX = 500
DEFAULT_BUFFER_MS = 60
MIN_QUEUE_MAX_FRAMES = 6

ALSA_BUFFER_US = 40000
ALSA_PERIOD_US = 10000

DEFAULT_RUNTIME_DIR = "~/mozart-runtime"
SSH_CONNECT_TIMEOUT_S = 10
SSH_CTL_TIMEOUT_S = 15
PROCESS_WAIT_TIMEOUT_S = 2.0

EXIT_OK = 0
EXIT_ERROR = 1
EXIT_USAGE = 2

MODE_PLAY = "play"
MODE_MIC = "mic"
MODE_DUPLEX = "duplex"
VALID_MODES = (MODE_PLAY, MODE_MIC, MODE_DUPLEX)

_LOOPBACK_CARD_RE = re.compile(
    r"^(?:plug)?hw:CARD=Loopback,DEV=([01]),SUBDEV=(\d+)$"
)
_LOOPBACK_NUM_RE = re.compile(r"^(?:plug)?hw:Loopback,([01]),(\d+)$")


class UsageError(Exception):
    """命令行或环境变量输入校验失败。"""


class BridgeError(Exception):
    """远端控制、SSH 链路或音频设备运行故障。"""


class _ArgParser(argparse.ArgumentParser):
    def error(self, message: str) -> None:  # noqa: D401
        raise UsageError(message)


@dataclass(frozen=True)
class BridgeConfig:
    target: str
    mode: str
    record_path: Path | None
    pb_sub: int
    cap_sub: int
    no_switch: bool
    runtime_dir: str
    remote_config: str
    buffer_ms: int

    @property
    def needs_play(self) -> bool:
        return self.mode in (MODE_PLAY, MODE_DUPLEX)

    @property
    def needs_mic(self) -> bool:
        return self.mode in (MODE_MIC, MODE_DUPLEX)


def _parse_bounded_int(raw: str, name: str, lo: int, hi: int) -> int:
    try:
        value = int(raw.strip())
    except ValueError as exc:
        raise UsageError(f"{name} 必须是整数，当前为 {raw!r}") from exc
    if not (lo <= value <= hi):
        raise UsageError(f"{name} 必须在 {lo}..={hi} 范围内，当前为 {value}")
    return value


def _env_or_default(env: Mapping[str, str], key: str, default: str) -> str:
    value = env.get(key, "").strip()
    return value if value else default


def parse_args(
    argv: list[str] | None = None,
    env: Mapping[str, str] | None = None,
) -> BridgeConfig:
    """解析并校验命令行参数与环境变量，入口处统一完成全部输入校验。"""
    environ = os.environ if env is None else env
    parser = _ArgParser(
        prog="jetson-audio.py",
        description="本机↔Jetson 跨平台 SSH 音频桥接（直连系统默认扬声器与默认麦克风）",
    )
    parser.add_argument(
        "target",
        nargs="?",
        help="SSH 目标（如 user@host）；缺省读环境变量 MOZART_JETSON",
    )
    parser.add_argument(
        "--mode",
        choices=VALID_MODES,
        default=MODE_PLAY,
        help="传输方向：play（默认听 Jetson）、mic（送本机麦）、duplex（双向）",
    )
    parser.add_argument(
        "--record",
        metavar="PATH",
        help="同步保存下行音频为 48 kHz 立体声 WAV（仅 play/duplex 可用）",
    )
    parser.add_argument(
        "--pb-sub",
        help=f"播放热切用 snd-aloop 子设备号（{SUBDEV_MIN}..={SUBDEV_MAX}，默认 {DEFAULT_PB_SUB}，环境变量 PB_SUB）",
    )
    parser.add_argument(
        "--cap-sub",
        help=f"采集热切用 snd-aloop 子设备号（{SUBDEV_MIN}..={SUBDEV_MAX}，默认 {DEFAULT_CAP_SUB}，环境变量 CAP_SUB）",
    )
    parser.add_argument(
        "--no-switch",
        action="store_true",
        help="跳过 mozart-ctl 查询与设备热切，直接桥接指定 Loopback 子设备",
    )
    parser.add_argument(
        "--runtime-dir",
        help=f"板端运行时目录（默认 {DEFAULT_RUNTIME_DIR}，环境变量 MOZART_RUNTIME_DIR）",
    )
    parser.add_argument(
        "--config",
        help="板端 TOML 配置路径（默认 <runtime-dir>/mozart.toml，环境变量 MOZART_REMOTE_CONFIG）",
    )
    parser.add_argument(
        "--buffer-ms",
        help=f"本地播放/采集目标缓冲毫秒数（{BUFFER_MS_MIN}..={BUFFER_MS_MAX}，默认 {DEFAULT_BUFFER_MS}）",
    )

    ns = parser.parse_args(argv)

    raw_target = (
        ns.target if ns.target is not None else _env_or_default(environ, "MOZART_JETSON", "")
    )
    target = raw_target.strip()
    if not target:
        raise UsageError("缺少 SSH 目标：请传入位置参数或设置环境变量 MOZART_JETSON")
    if target.startswith("-"):
        raise UsageError(f"非法的 SSH 目标：{target!r}")

    record_path: Path | None = None
    if ns.record is not None:
        raw_record = ns.record.strip()
        if not raw_record:
            raise UsageError("--record 路径不能为空")
        if ns.mode == MODE_MIC:
            raise UsageError("mic 模式没有下行音频，不能使用 --record")
        candidate = Path(raw_record)
        if candidate.exists() and candidate.is_dir():
            raise UsageError(f"--record 指向了已存在的目录：{candidate}")
        record_path = candidate

    raw_pb = (
        ns.pb_sub
        if ns.pb_sub is not None
        else _env_or_default(environ, "PB_SUB", str(DEFAULT_PB_SUB))
    )
    pb_sub = _parse_bounded_int(raw_pb, "pb-sub", SUBDEV_MIN, SUBDEV_MAX)

    raw_cap = (
        ns.cap_sub
        if ns.cap_sub is not None
        else _env_or_default(environ, "CAP_SUB", str(DEFAULT_CAP_SUB))
    )
    cap_sub = _parse_bounded_int(raw_cap, "cap-sub", SUBDEV_MIN, SUBDEV_MAX)

    if ns.mode == MODE_DUPLEX and pb_sub == cap_sub:
        raise UsageError(f"duplex 模式下 pb-sub 与 cap-sub 不能相同（当前同为 {pb_sub}）")

    raw_buf = ns.buffer_ms if ns.buffer_ms is not None else str(DEFAULT_BUFFER_MS)
    buffer_ms = _parse_bounded_int(raw_buf, "buffer-ms", BUFFER_MS_MIN, BUFFER_MS_MAX)

    raw_runtime = (
        ns.runtime_dir
        if ns.runtime_dir is not None
        else _env_or_default(environ, "MOZART_RUNTIME_DIR", DEFAULT_RUNTIME_DIR)
    )
    runtime_dir = raw_runtime.strip()
    if not runtime_dir:
        raise UsageError("runtime-dir 不能为空")

    if ns.config is not None:
        remote_config = ns.config.strip()
        if not remote_config:
            raise UsageError("config 路径不能为空")
    else:
        env_cfg = environ.get("MOZART_REMOTE_CONFIG", "").strip()
        remote_config = env_cfg if env_cfg else join_remote_path(runtime_dir, "mozart.toml")

    return BridgeConfig(
        target=target,
        mode=ns.mode,
        record_path=record_path,
        pb_sub=pb_sub,
        cap_sub=cap_sub,
        no_switch=ns.no_switch,
        runtime_dir=runtime_dir,
        remote_config=remote_config,
        buffer_ms=buffer_ms,
    )


def join_remote_path(base: str, rel: str) -> str:
    """拼接远端 POSIX 路径，正确处理根目录与末尾斜杠。"""
    return posixpath.join(base.rstrip("/") or "/", rel.lstrip("/"))


def quote_remote_path(path: str) -> str:
    """为远端 POSIX shell 转义路径，保留开头 ~/ 的 $HOME 展开。"""
    if path == "~":
        return '"$HOME"'
    if path.startswith("~/"):
        rest = path[2:]
        return '"$HOME"/' + shlex.quote(rest) if rest else '"$HOME"/'
    return shlex.quote(path)


def parse_loopback_device(device: str) -> tuple[int, int] | None:
    """解析 ALSA Loopback 设备名，返回 (dev, subdev)；非 Loopback 或非法返回 None。"""
    text = device.strip()
    for pattern in (_LOOPBACK_CARD_RE, _LOOPBACK_NUM_RE):
        match = pattern.match(text)
        if match:
            dev = int(match.group(1))
            sub = int(match.group(2))
            if SUBDEV_MIN <= sub <= SUBDEV_MAX:
                return (dev, sub)
            return None
    return None


def loopback_device(dev: int, sub: int) -> str:
    """构造标准 plughw Loopback 设备名。"""
    return f"plughw:CARD=Loopback,DEV={dev},SUBDEV={sub}"


def ssh_base_cmd(target: str) -> list[str]:
    """构造无 PTY、无压缩、非交互的 SSH 基础参数。"""
    return [
        "ssh",
        "-T",
        "-o",
        "Compression=no",
        "-o",
        "BatchMode=yes",
        "-o",
        f"ConnectTimeout={SSH_CONNECT_TIMEOUT_S}",
        target,
    ]


def _remote_alsa_cmd(binary: str, alsa_device: str) -> str:
    return (
        f"exec {binary} -q -D {shlex.quote(alsa_device)} "
        f"-f S16_LE -c {CHANNELS} -r {SAMPLE_RATE} -t raw "
        f"--buffer-time={ALSA_BUFFER_US} --period-time={ALSA_PERIOD_US}"
    )


def remote_arecord_cmd(alsa_device: str) -> str:
    """构造远端采集 Loopback 输出并写原始 PCM 到 stdout 的命令。"""
    return _remote_alsa_cmd("arecord", alsa_device)


def remote_aplay_cmd(alsa_device: str) -> str:
    """构造远端从 stdin 读原始 PCM 并写入 Loopback 的命令。"""
    return _remote_alsa_cmd("aplay", alsa_device)


def compute_queue_bounds(buffer_ms: int) -> tuple[int, int]:
    """按 20 ms 帧向上取整计算目标缓冲帧数与有界队列上限。"""
    target_frames = math.ceil(buffer_ms / FRAME_MS)
    max_frames = max(target_frames * 2, MIN_QUEUE_MAX_FRAMES)
    return target_frames, max_frames


class FrameQueue:
    """线程安全的定长 PCM 帧有界队列：满时丢最旧防漂移积压，空时补零。"""

    def __init__(self, target_frames: int, max_frames: int) -> None:
        self._target_frames = target_frames
        self._max_frames = max_frames
        self._buf: deque[bytes] = deque()
        self._lock = threading.Lock()
        self._cv = threading.Condition(self._lock)
        self._primed = False
        self.frames_pushed = 0
        self.frames_popped = 0
        self.dropped = 0
        self.underruns = 0

    def push(self, frame: bytes) -> None:
        with self._lock:
            if len(self._buf) >= self._max_frames:
                self._buf.popleft()
                self.dropped += 1
            self._buf.append(frame)
            self.frames_pushed += 1
            self._cv.notify_all()

    def pop_playback(self) -> bytes:
        """非阻塞出队一帧供音频播放回调使用；预缓冲未满或欠载时返回静音帧。"""
        with self._lock:
            if not self._primed:
                if len(self._buf) < self._target_frames:
                    return SILENCE_FRAME
                self._primed = True
            if not self._buf:
                self.underruns += 1
                self._primed = False
                return SILENCE_FRAME
            self.frames_popped += 1
            return self._buf.popleft()

    def pop_blocking(
        self,
        stop_event: threading.Event,
        timeout: float = 0.1,
    ) -> bytes | None:
        """阻塞等待一帧供上行写线程使用；停止时立即返回 None。"""
        with self._lock:
            while not self._buf:
                if stop_event.is_set():
                    return None
                self._cv.wait(timeout=timeout)
            if stop_event.is_set():
                return None
            self.frames_popped += 1
            return self._buf.popleft()

    def notify_all(self) -> None:
        with self._lock:
            self._cv.notify_all()


def fill_playback_buffer(
    carry: bytearray,
    outdata: bytearray | memoryview,
    next_frame: Callable[[], bytes],
) -> None:
    """把定长 20 ms 队列帧对齐填充到音频后端请求的输出缓冲区。"""
    needed = len(outdata)
    while len(carry) < needed:
        carry.extend(next_frame())
    outdata[:] = carry[:needed]
    del carry[:needed]


def drain_capture_buffer(
    carry: bytearray,
    indata: bytes | bytearray | memoryview,
    push_frame: Callable[[bytes], None],
) -> None:
    """把音频后端送来的采集块切分为严格 20 ms 帧入队。"""
    carry.extend(indata)
    while len(carry) >= FRAME_BYTES:
        push_frame(bytes(carry[:FRAME_BYTES]))
        del carry[:FRAME_BYTES]


Runner = Callable[[list[str], float], subprocess.CompletedProcess[str]]


def _default_runner(cmd: list[str], timeout: float) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise BridgeError(f"执行 SSH 控制命令失败：{exc}") from exc


class RemoteController:
    """通过 SSH 在 Jetson 上检查 snd-aloop 并调用 mozart-ctl。"""

    def __init__(
        self,
        target: str,
        runtime_dir: str,
        remote_config: str,
        runner: Runner | None = None,
    ) -> None:
        self.target = target
        self.runtime_dir = runtime_dir
        self.remote_config = remote_config
        self._runner = _default_runner if runner is None else runner

    def _ctl_prefix(self) -> str:
        ctl_raw = join_remote_path(self.runtime_dir, "bin/mozart-ctl")
        return (
            f"{quote_remote_path(ctl_raw)} "
            f"--config {quote_remote_path(self.remote_config)}"
        )

    def _run_remote(self, remote_cmd: str, timeout: float = SSH_CTL_TIMEOUT_S) -> str:
        cmd = ssh_base_cmd(self.target) + [remote_cmd]
        proc = self._runner(cmd, timeout)
        if proc.returncode != 0:
            detail = proc.stderr.strip() or proc.stdout.strip() or f"exit code {proc.returncode}"
            raise BridgeError(detail)
        return proc.stdout

    def check_aloop_loaded(self) -> None:
        try:
            self._run_remote("[ -d /proc/asound/Loopback ]")
        except BridgeError as exc:
            raise BridgeError(
                f"远端未检测到 snd-aloop（请在 Jetson 执行 sudo modprobe snd-aloop）：{exc}"
            ) from exc

    def query_pre_devices(self) -> tuple[str, str]:
        try:
            out = self._run_remote(f"{self._ctl_prefix()} status --json")
        except BridgeError as exc:
            raise BridgeError(f"查询远端 mozart-ctl status 失败：{exc}") from exc

        try:
            payload = json.loads(out)
            pre = payload["pre"]
            if not isinstance(pre, dict):
                raise BridgeError("远端 mozart-pre 未运行或尚未就绪")
            pb = pre["playback_device"]
            cap = pre["capture_device"]
            if not isinstance(pb, str) or not pb or not isinstance(cap, str) or not cap:
                raise BridgeError("status 中的设备字段缺失或为空")
        except (json.JSONDecodeError, KeyError, TypeError) as exc:
            raise BridgeError(f"解析远端 status JSON 失败：{exc}") from exc
        return pb, cap

    def device_use(self, kind: str, device: str) -> None:
        try:
            self._run_remote(
                f"{self._ctl_prefix()} device use {kind} {shlex.quote(device)} --json"
            )
        except BridgeError as exc:
            raise BridgeError(f"热切 {kind} 到 {device!r} 失败：{exc}") from exc


@dataclass
class DeviceLease:
    playback_peer: str | None
    capture_peer: str | None
    switches: list[tuple[str, str]] = field(default_factory=list)

    def release(self, ctl: RemoteController) -> list[str]:
        """按切换逆序还原远端设备，返回还原失败描述列表。"""
        errors: list[str] = []
        for kind, previous in reversed(self.switches):
            try:
                ctl.device_use(kind, previous)
            except BridgeError as exc:
                errors.append(f"{kind} 无法还原为 {previous!r}（{exc}）")
        return errors


def _plan_single_direction(
    current: str,
    fallback_pre_dev: int,
    fallback_sub: int,
) -> tuple[str | None, str]:
    parsed = parse_loopback_device(current)
    if parsed is not None:
        pre_dev, sub = parsed
        return None, loopback_device(1 - pre_dev, sub)
    target = loopback_device(fallback_pre_dev, fallback_sub)
    peer = loopback_device(1 - fallback_pre_dev, fallback_sub)
    return target, peer


def _plan_duplex_directions(
    cur_pb: str,
    pb_sub: int,
    cur_cap: str,
    cap_sub: int,
) -> tuple[tuple[str | None, str], tuple[str | None, str], bool]:
    parsed_pb = parse_loopback_device(cur_pb)
    parsed_cap = parse_loopback_device(cur_cap)

    pb_reuse = None
    cap_reuse = None
    if parsed_pb is not None and parsed_cap is not None and parsed_pb[1] != parsed_cap[1]:
        pb_reuse = parsed_pb
        cap_reuse = parsed_cap
    elif parsed_pb is not None and parsed_pb[1] != cap_sub:
        pb_reuse = parsed_pb
    elif parsed_cap is not None and parsed_cap[1] != pb_sub:
        cap_reuse = parsed_cap

    pb_plan = (
        (None, loopback_device(1 - pb_reuse[0], pb_reuse[1]))
        if pb_reuse is not None
        else (loopback_device(0, pb_sub), loopback_device(1, pb_sub))
    )
    cap_plan = (
        (None, loopback_device(1 - cap_reuse[0], cap_reuse[1]))
        if cap_reuse is not None
        else (loopback_device(1, cap_sub), loopback_device(0, cap_sub))
    )
    # 若采集端原本占用了目标 pb_sub 且需要改切采集端，先挪走采集端再切播放端，避免瞬态自环。
    capture_first = (
        pb_plan[0] is not None
        and cap_plan[0] is not None
        and parsed_cap is not None
        and parsed_cap[1] == pb_sub
    )
    return pb_plan, cap_plan, capture_first


def acquire_devices(cfg: BridgeConfig, ctl: RemoteController) -> DeviceLease:
    """预检 Loopback 并规划/热切所需设备；duplex 半切失败立即回滚。"""
    ctl.check_aloop_loaded()
    if cfg.no_switch:
        return DeviceLease(
            playback_peer=loopback_device(1, cfg.pb_sub) if cfg.needs_play else None,
            capture_peer=loopback_device(0, cfg.cap_sub) if cfg.needs_mic else None,
            switches=[],
        )

    cur_pb, cur_cap = ctl.query_pre_devices()
    if cfg.mode == MODE_PLAY:
        pb_target, playback_peer = _plan_single_direction(cur_pb, 0, cfg.pb_sub)
        capture_peer = None
        actions = [("playback", cur_pb, pb_target)]
    elif cfg.mode == MODE_MIC:
        cap_target, capture_peer = _plan_single_direction(cur_cap, 1, cfg.cap_sub)
        playback_peer = None
        actions = [("capture", cur_cap, cap_target)]
    else:
        (pb_target, playback_peer), (cap_target, capture_peer), capture_first = (
            _plan_duplex_directions(cur_pb, cfg.pb_sub, cur_cap, cfg.cap_sub)
        )
        pb_action = ("playback", cur_pb, pb_target)
        cap_action = ("capture", cur_cap, cap_target)
        actions = [cap_action, pb_action] if capture_first else [pb_action, cap_action]

    switches: list[tuple[str, str]] = []
    try:
        for kind, previous, target_dev in actions:
            if target_dev is not None:
                ctl.device_use(kind, target_dev)
                switches.append((kind, previous))
    except BridgeError as exc:
        rollback_errors = DeviceLease(None, None, switches).release(ctl)
        if rollback_errors:
            raise BridgeError(
                f"{exc}；且回滚已切设备失败：{'; '.join(rollback_errors)}"
            ) from exc
        raise

    return DeviceLease(
        playback_peer=playback_peer,
        capture_peer=capture_peer,
        switches=switches,
    )


class WavRecorder:
    """下行音频 WAV 同步录制器（只在非音频回调线程写入）。"""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.frames_written = 0
        self._lock = threading.Lock()
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            writer = wave.open(str(path), "wb")
            writer.setnchannels(CHANNELS)
            writer.setsampwidth(SAMPLE_WIDTH)
            writer.setframerate(SAMPLE_RATE)
        except (OSError, wave.Error) as exc:
            raise BridgeError(f"打开录制文件 {path} 失败：{exc}") from exc
        self._writer: wave.Wave_write | None = writer

    def write(self, frame: bytes) -> None:
        with self._lock:
            if self._writer is not None:
                self._writer.writeframesraw(frame)
                self.frames_written += 1

    def close(self) -> None:
        with self._lock:
            if self._writer is not None:
                self._writer.close()
                self._writer = None


def read_exact(stream: BinaryIO, size: int) -> bytes | None:
    """循环读取精确字节数；遇到 EOF 返回 None。"""
    buf = bytearray()
    while len(buf) < size:
        chunk = stream.read(size - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def write_all(stream: BinaryIO, data: bytes) -> None:
    """循环写入全部字节直到完成或抛出 I/O 异常。"""
    view = memoryview(data)
    sent = 0
    while sent < len(view):
        written = stream.write(view[sent:])
        if written is None or written <= 0:
            raise OSError("管道写入无进展")
        sent += written


def _close_pipe(stream: BinaryIO | None) -> None:
    if stream is None:
        return
    try:
        stream.close()
    except OSError:
        pass


def _stop_process(proc: subprocess.Popen[bytes]) -> None:
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=PROCESS_WAIT_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


class BridgeState:
    """线程安全记录桥接故障并触发停止（锁外执行进程清理，保留全部独立错误）。"""

    def __init__(self, stop_event: threading.Event) -> None:
        self._lock = threading.Lock()
        self.stop_event = stop_event
        self.errors: list[str] = []
        self._reported_procs: set[int] = set()

    @property
    def has_errors(self) -> bool:
        with self._lock:
            return bool(self.errors)

    def format_errors(self) -> str:
        with self._lock:
            return "；".join(self.errors)

    def fail(self, message: str) -> None:
        with self._lock:
            if message not in self.errors:
                self.errors.append(message)
            self.stop_event.set()

    def fail_from_proc(self, label: str, proc: subprocess.Popen[bytes]) -> None:
        proc_id = id(proc)
        with self._lock:
            if proc_id in self._reported_procs:
                self.stop_event.set()
                return
            self._reported_procs.add(proc_id)
        _stop_process(proc)
        stderr_text = ""
        if proc.stderr is not None:
            try:
                stderr_text = proc.stderr.read().decode("utf-8", errors="replace").strip()
            except OSError:
                stderr_text = ""
        detail = f"：{stderr_text}" if stderr_text else ""
        self.fail(f"{label}意外断开（exit={proc.returncode}）{detail}")


def import_sounddevice() -> ModuleType:
    """延迟导入 sounddevice，确保纯逻辑单测无需安装音频依赖。"""
    try:
        import sounddevice as sd  # type: ignore[import-not-found]
    except ImportError as exc:
        raise BridgeError(
            "未安装 sounddevice；请使用 `uv run tools/jetson-audio.py ...` 自动加载依赖运行"
        ) from exc
    except OSError as exc:
        raise BridgeError(
            f"加载 PortAudio 音频库失败（Linux 请确认已安装系统包 libportaudio2 / portaudio）：{exc}"
        ) from exc
    return sd


def _downlink_worker(
    proc: subprocess.Popen[bytes],
    queue: FrameQueue,
    recorder: WavRecorder | None,
    state: BridgeState,
) -> None:
    assert proc.stdout is not None
    while not state.stop_event.is_set():
        frame = read_exact(proc.stdout, FRAME_BYTES)
        if frame is None:
            if not state.stop_event.is_set():
                state.fail_from_proc("下行 SSH 音频流", proc)
            return
        if recorder is not None:
            try:
                recorder.write(frame)
            except (OSError, wave.Error) as exc:
                state.fail(f"写入 WAV 文件失败：{exc}")
                return
        queue.push(frame)


def _uplink_worker(
    proc: subprocess.Popen[bytes],
    queue: FrameQueue,
    state: BridgeState,
) -> None:
    assert proc.stdin is not None
    while True:
        frame = queue.pop_blocking(state.stop_event)
        if frame is None:
            return
        try:
            write_all(proc.stdin, frame)
        except OSError:
            if not state.stop_event.is_set():
                state.fail_from_proc("上行 SSH 音频流", proc)
            return


def _spawn_ssh_stream(target: str, remote_cmd: str, *, stdin_pipe: bool) -> subprocess.Popen[bytes]:
    cmd = ssh_base_cmd(target) + [remote_cmd]
    kwargs: dict[str, object] = {
        "stdin": subprocess.PIPE if stdin_pipe else subprocess.DEVNULL,
        "stdout": subprocess.DEVNULL if stdin_pipe else subprocess.PIPE,
        "stderr": subprocess.PIPE,
        "bufsize": 0,
    }
    # 隔离进程组，防止终端 Ctrl-C 先杀死 ssh 导致工作线程误报断线。
    if os.name == "nt":
        kwargs["creationflags"] = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
    else:
        kwargs["start_new_session"] = True
    try:
        return subprocess.Popen(cmd, **kwargs)  # type: ignore[arg-type]
    except OSError as exc:
        raise BridgeError(f"启动 SSH 音频进程失败：{exc}") from exc


def run_bridge(cfg: BridgeConfig) -> int:
    """建立本机↔Jetson 实时音频桥接并在退出时按顺序清理与还原设备。"""
    sd = import_sounddevice()
    ctl = RemoteController(cfg.target, cfg.runtime_dir, cfg.remote_config)
    lease = acquire_devices(cfg, ctl)

    target_frames, max_frames = compute_queue_bounds(cfg.buffer_ms)
    stop_event = threading.Event()
    state = BridgeState(stop_event)
    play_queue = FrameQueue(target_frames, max_frames) if lease.playback_peer else None
    mic_queue = FrameQueue(target_frames, max_frames) if lease.capture_peer else None

    recorder: WavRecorder | None = None
    down_proc: subprocess.Popen[bytes] | None = None
    up_proc: subprocess.Popen[bytes] | None = None
    down_thread: threading.Thread | None = None
    up_thread: threading.Thread | None = None
    out_stream = None
    in_stream = None
    restore_errors: list[str] = []
    interrupted = False
    prev_handlers: dict[int, object] = {}

    def handle_signal(_signum: int, _frame: FrameType | None) -> None:
        nonlocal interrupted
        # 信号处理器只置无锁标志，防止重入唤醒条件锁自死锁。
        interrupted = True

    try:
        for sig_name in ("SIGINT", "SIGTERM"):
            sig = getattr(signal, sig_name, None)
            if sig is not None:
                prev_handlers[sig] = signal.getsignal(sig)
                signal.signal(sig, handle_signal)

        if cfg.record_path is not None:
            recorder = WavRecorder(cfg.record_path)

        if play_queue is not None:
            assert lease.playback_peer is not None
            down_proc = _spawn_ssh_stream(
                cfg.target,
                remote_arecord_cmd(lease.playback_peer),
                stdin_pipe=False,
            )
            down_thread = threading.Thread(
                target=_downlink_worker,
                args=(down_proc, play_queue, recorder, state),
                daemon=True,
            )
            down_thread.start()

            out_carry = bytearray()

            def out_callback(outdata, _frames: int, _time_info, _status) -> None:
                fill_playback_buffer(out_carry, outdata, play_queue.pop_playback)

            def out_finished() -> None:
                if not interrupted and not stop_event.is_set():
                    state.fail("系统默认扬声器音频流意外中断")

            try:
                out_stream = sd.RawOutputStream(
                    samplerate=SAMPLE_RATE,
                    blocksize=FRAME_SAMPLES,
                    device=None,
                    channels=CHANNELS,
                    dtype="int16",
                    callback=out_callback,
                    finished_callback=out_finished,
                )
                out_stream.start()
            except sd.PortAudioError as exc:
                raise BridgeError(f"打开系统默认扬声器失败：{exc}") from exc

        if mic_queue is not None:
            assert lease.capture_peer is not None
            up_proc = _spawn_ssh_stream(
                cfg.target,
                remote_aplay_cmd(lease.capture_peer),
                stdin_pipe=True,
            )
            up_thread = threading.Thread(
                target=_uplink_worker,
                args=(up_proc, mic_queue, state),
                daemon=True,
            )
            up_thread.start()

            in_carry = bytearray()

            def in_callback(indata, _frames: int, _time_info, _status) -> None:
                drain_capture_buffer(in_carry, indata, mic_queue.push)

            def in_finished() -> None:
                if not interrupted and not stop_event.is_set():
                    state.fail("系统默认麦克风音频流意外中断")

            try:
                in_stream = sd.RawInputStream(
                    samplerate=SAMPLE_RATE,
                    blocksize=FRAME_SAMPLES,
                    device=None,
                    channels=CHANNELS,
                    dtype="int16",
                    callback=in_callback,
                    finished_callback=in_finished,
                )
                in_stream.start()
            except sd.PortAudioError as exc:
                raise BridgeError(f"打开系统默认麦克风失败：{exc}") from exc

        summary = [f"mode={cfg.mode}", f"target={cfg.target}", f"buffer={cfg.buffer_ms} ms"]
        if lease.playback_peer is not None:
            summary.append(f"play={lease.playback_peer}")
        if lease.capture_peer is not None:
            summary.append(f"mic={lease.capture_peer}")
        if cfg.record_path is not None:
            summary.append(f"record={cfg.record_path}")
        print(f"jetson-audio: 已连接（{', '.join(summary)}）；按 Ctrl-C 停止", file=sys.stderr)

        while not stop_event.is_set():
            if interrupted:
                stop_event.set()
                break
            if down_proc is not None and down_proc.poll() is not None:
                state.fail_from_proc("下行 SSH 音频流", down_proc)
                break
            if up_proc is not None and up_proc.poll() is not None:
                state.fail_from_proc("上行 SSH 音频流", up_proc)
                break
            if out_stream is not None and not out_stream.active:
                state.fail("系统默认扬声器音频流意外中断")
                break
            if in_stream is not None and not in_stream.active:
                state.fail("系统默认麦克风音频流意外中断")
                break
            stop_event.wait(0.1)
    finally:
        # 1. 先置停止标志避免流关闭回调误报，再停并关本地音频流。
        stop_event.set()
        for stream_name, stream in (("扬声器", out_stream), ("麦克风", in_stream)):
            if stream is None:
                continue
            try:
                stream.stop()
            except sd.PortAudioError as exc:
                state.fail(f"停止系统{stream_name}流失败：{exc}")
            try:
                stream.close()
            except sd.PortAudioError as exc:
                state.fail(f"关闭系统{stream_name}流失败：{exc}")

        # 2. 唤醒上行队列、终止远端 SSH 子进程并等待读写线程退出，之后关闭管道句柄。
        if mic_queue is not None:
            mic_queue.notify_all()
        for proc in (down_proc, up_proc):
            if proc is not None:
                _stop_process(proc)
        for worker in (down_thread, up_thread):
            if worker is not None:
                worker.join(timeout=PROCESS_WAIT_TIMEOUT_S)
        for proc in (down_proc, up_proc):
            if proc is not None:
                _close_pipe(proc.stdin)
                _close_pipe(proc.stdout)
                _close_pipe(proc.stderr)

        # 3. 关闭 WAV 录制文件，写回完整头长度。
        if recorder is not None:
            try:
                recorder.close()
            except (OSError, wave.Error) as exc:
                state.fail(f"关闭 WAV 文件失败：{exc}")

        # 4. 逆序还原热切过的远端设备，并在 finally 内立刻输出告警防异常穿透丢失。
        restore_errors = lease.release(ctl)
        for msg in restore_errors:
            print(f"jetson-audio: 还原设备告警：{msg}", file=sys.stderr)

        for sig, prev in prev_handlers.items():
            if prev is not None:
                signal.signal(sig, prev)

    stats: list[str] = []
    if play_queue is not None:
        stats.append(
            f"play_frames={play_queue.frames_popped} "
            f"drops={play_queue.dropped} underruns={play_queue.underruns}"
        )
    if mic_queue is not None:
        stats.append(
            f"mic_frames={mic_queue.frames_popped} drops={mic_queue.dropped}"
        )
    if recorder is not None:
        stats.append(f"recorded_frames={recorder.frames_written}")
    if stats:
        print(f"jetson-audio: 已退出（{', '.join(stats)}）", file=sys.stderr)

    if state.has_errors:
        print(f"jetson-audio: 错误：{state.format_errors()}", file=sys.stderr)
        return EXIT_ERROR
    if restore_errors:
        return EXIT_ERROR
    return EXIT_OK


def main(argv: list[str] | None = None) -> int:
    try:
        cfg = parse_args(argv)
    except UsageError as exc:
        print(f"jetson-audio: 参数错误：{exc}", file=sys.stderr)
        return EXIT_USAGE
    try:
        return run_bridge(cfg)
    except BridgeError as exc:
        print(f"jetson-audio: {exc}", file=sys.stderr)
        return EXIT_ERROR


if __name__ == "__main__":
    raise SystemExit(main())

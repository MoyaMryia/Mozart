use super::ulunas::{exchange, run, validate, UlunasWorker};
use super::{fanout, DenoiseProcessor, WorkItem};
use crate::{ai, state::Shared};
use mozart_core::config::EnhanceConfig;
use mozart_core::denoise::{DenoiseBackend, DenoiseMode};
use mozart_core::dgram::FrameSocket;
use mozart_core::{
    EnhanceReply, EnhanceRequest, FrameMeta, InputFrame, RawFrame, ENHANCE_PACKET_MAGIC,
    INPUT_PACKET_MAGIC, INPUT_SAMPLES, RAW_SAMPLES,
};
use mozart_dsp::{InputFrameAnalyzer, TrunkProcessor};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const LOOP_DEADLINE: Duration = Duration::from_secs(5);

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + LOOP_DEADLINE;
    while !condition() {
        assert!(Instant::now() < deadline, "worker condition timed out");
        std::thread::yield_now();
    }
}

fn readable(fd: &OwnedFd) {
    let deadline = Instant::now() + LOOP_DEADLINE;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "stub I/O timed out");
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, remaining.as_millis() as i32) };
        if result > 0 {
            return;
        }
        assert!(result == 0 || io::Error::last_os_error().kind() == io::ErrorKind::Interrupted);
    }
}

enum StubCommand {
    Accept,
    Request,
    Reply(Box<EnhanceReply>),
    Disconnect,
    Stop,
}

struct LoopTest {
    dir: PathBuf,
    shared: Arc<Shared>,
    enhance: Arc<UlunasWorker>,
    consumer: FrameSocket<InputFrame>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<(), String>>>,
    stub: Option<JoinHandle<()>>,
    commands: mpsc::Sender<StubCommand>,
    requests: mpsc::Receiver<EnhanceRequest>,
    replies: mpsc::Receiver<isize>,
}

impl LoopTest {
    fn new(tag: &str, timeout_ms: u32) -> Self {
        let dir = std::env::temp_dir().join(format!("mozart-loop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nn.sock");
        let mut addr: libc::sockaddr_un = unsafe { zeroed() };
        addr.sun_family = libc::AF_UNIX as _;
        for (slot, byte) in addr.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
            *slot = *byte as _;
        }
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        assert!(raw >= 0);
        let listener = unsafe { OwnedFd::from_raw_fd(raw) };
        assert_eq!(
            unsafe {
                libc::bind(
                    raw,
                    &addr as *const _ as *const _,
                    size_of::<libc::sockaddr_un>() as _,
                )
            },
            0
        );
        assert_eq!(unsafe { libc::listen(raw, 8) }, 0);
        let (commands, command_rx) = mpsc::channel();
        let (request_tx, requests) = mpsc::channel();
        let (reply_tx, replies) = mpsc::channel();
        let stub = std::thread::spawn(move || {
            let mut peer: Option<OwnedFd> = None;
            while let Ok(command) = command_rx.recv_timeout(LOOP_DEADLINE) {
                match command {
                    StubCommand::Accept => {
                        readable(&listener);
                        let raw = unsafe {
                            libc::accept4(
                                listener.as_raw_fd(),
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                                libc::SOCK_CLOEXEC,
                            )
                        };
                        assert!(raw >= 0);
                        peer = Some(unsafe { OwnedFd::from_raw_fd(raw) });
                    }
                    StubCommand::Request => {
                        let fd = peer.as_ref().unwrap();
                        readable(fd);
                        let mut request: EnhanceRequest = unsafe { zeroed() };
                        assert_eq!(
                            unsafe {
                                libc::recv(
                                    fd.as_raw_fd(),
                                    &mut request as *mut _ as *mut _,
                                    size_of::<EnhanceRequest>(),
                                    libc::MSG_TRUNC,
                                )
                            },
                            size_of::<EnhanceRequest>() as isize
                        );
                        assert_eq!({ request.magic }, INPUT_PACKET_MAGIC);
                        request_tx.send(request).unwrap();
                    }
                    StubCommand::Reply(reply) => {
                        let sent = unsafe {
                            libc::send(
                                peer.as_ref().unwrap().as_raw_fd(),
                                &*reply as *const _ as *const _,
                                size_of::<EnhanceReply>(),
                                libc::MSG_NOSIGNAL,
                            )
                        };
                        if sent < 0 {
                            assert!(matches!(
                                io::Error::last_os_error().raw_os_error(),
                                Some(libc::EPIPE | libc::ECONNRESET)
                            ));
                        }
                        reply_tx.send(sent).unwrap();
                    }
                    StubCommand::Disconnect => peer = None,
                    StubCommand::Stop => {
                        let mut pending = [
                            libc::pollfd {
                                fd: listener.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                            libc::pollfd {
                                fd: peer.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                        ];
                        assert!(unsafe { libc::poll(pending.as_mut_ptr(), 2, 0) } >= 0);
                        assert_eq!(
                            pending[0].revents & libc::POLLIN,
                            0,
                            "unexpected NN connection"
                        );
                        if let Some(fd) = peer {
                            let mut byte = 0u8;
                            let count = unsafe {
                                libc::recv(
                                    fd.as_raw_fd(),
                                    &mut byte as *mut _ as *mut _,
                                    1,
                                    libc::MSG_DONTWAIT,
                                )
                            };
                            assert!(count <= 0, "unexpected NN request or replay");
                        }
                        break;
                    }
                }
            }
        });
        let mut shared = crate::state::test_shared("cap", "play", "boot");
        let mut dsp = shared.dsp_target();
        dsp.denoise = true;
        shared.set_dsp_target(dsp);
        shared.set_mode_asserted(true);
        shared.configure_enhancement(
            &EnhanceConfig {
                model: "protocol-only-unused.onnx".into(),
                socket: "control.sock".into(),
                audio_socket: "nn.sock".into(),
                queue_frames: 16,
                request_timeout_ms: timeout_ms,
            },
            &dir,
        );
        let enhance = shared.enhancement.clone().unwrap();
        let shared = Arc::new(shared);
        let consumer = FrameSocket::bind(&dir.join("consumer.sock"), 16).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (enhance, shared, stop) = (enhance.clone(), shared.clone(), stop.clone());
            let sender = ai::Sender::new(dir.join("consumer.sock"), 1, "asr");
            std::thread::spawn(move || run(enhance, shared, vec![sender], &stop))
        };
        Self {
            dir,
            shared,
            enhance,
            consumer,
            stop,
            worker: Some(worker),
            stub: Some(stub),
            commands,
            requests,
            replies,
        }
    }

    fn push(&self, packed_idx: u32, source_idx: u32) {
        let mut work = item(packed_idx);
        work.source_idx = source_idx;
        work.file_generation = self.shared.file_generation();
        let (mode, epoch) = self.shared.denoise_snapshot();
        work.epoch = epoch;
        work.enhance_backend = match mode {
            DenoiseMode::Backend(b) if b == DenoiseBackend::Ulunas => Some(b),
            _ => None,
        };
        self.enhance.queue.push(work);
    }

    fn request(&self, packed_idx: u32, source_idx: u32, accept: bool) -> InputFrame {
        self.push(packed_idx, source_idx);
        if accept {
            self.commands.send(StubCommand::Accept).unwrap();
        }
        self.commands.send(StubCommand::Request).unwrap();
        let request = self.requests.recv_timeout(LOOP_DEADLINE).unwrap();
        assert_eq!({ request.frame.meta.frame_idx }, packed_idx);
        assert_eq!(
            { request.frame.meta.pts_ns },
            u64::from(packed_idx) * 20_000_000
        );
        request.frame
    }

    fn reply(&self, source: InputFrame, ready: bool, bad: bool) -> isize {
        let mut frame = source;
        frame.pcm = [0.25; INPUT_SAMPLES];
        self.commands
            .send(StubCommand::Reply(Box::new(EnhanceReply {
                magic: if bad { 0 } else { ENHANCE_PACKET_MAGIC },
                ready: u32::from(ready),
                frame,
            })))
            .unwrap();
        self.replies.recv_timeout(LOOP_DEADLINE).unwrap()
    }

    fn session(&self, first: u32, source_first: u32) {
        let source = self.request(first, source_first, true);
        assert_eq!(
            self.reply(source, false, false),
            size_of::<EnhanceReply>() as isize
        );
        let next = self.request(first + 1, source_first + 1, false);
        assert_eq!(
            self.reply(next, false, false),
            size_of::<EnhanceReply>() as isize
        );
        self.request(first + 2, source_first + 2, false);
        assert_eq!(
            self.reply(source, true, false),
            size_of::<EnhanceReply>() as isize
        );
        let output = self.consumer.recv(Some(LOOP_DEADLINE)).unwrap();
        assert_eq!({ output.meta.frame_idx }, first);
        assert_eq!({ output.meta.pts_ns }, u64::from(first) * 20_000_000);
        assert_eq!({ output.pcm }, [0.25; INPUT_SAMPLES]);
        assert_eq!(output.meta.conf, 0);
        assert_eq!(output.meta.vad_flag, 1);
        assert!(output.meta.energy_db > 0);
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let worker = self.worker.take().map(|worker| worker.join());
        let _ = self.commands.send(StubCommand::Stop);
        let stub = self.stub.take().map(|stub| stub.join());
        if !std::thread::panicking() {
            worker.unwrap().unwrap().unwrap();
            stub.unwrap().unwrap();
        }
    }

    fn no_output(&self) {
        assert_eq!(
            self.consumer
                .recv(Some(Duration::ZERO))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }
}

impl Drop for LoopTest {
    fn drop(&mut self) {
        if self.worker.is_some() || self.stub.is_some() {
            self.finish();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn run_priming_and_capture_gap_with_continuous_packed_indices() {
    let mut test = LoopTest::new("capture-gap", 1000);
    test.session(10, 100);
    until(|| test.enhance.status(true).frames_out == 1);
    assert_eq!(test.enhance.status(true).frames_in, 3);
    test.no_output();
    let source = test.request(13, 104, true);
    assert_eq!(
        test.enhance.status(true).last_error.as_deref(),
        Some("enhancement input gap; session reset")
    );
    test.reply(source, false, false);
    let next = test.request(14, 105, false);
    assert!(
        test.enhance.status(true).last_error.is_some(),
        "priming 不清除故障"
    );
    test.reply(next, false, false);
    test.request(15, 106, false);
    test.reply(source, true, false);
    let output = test.consumer.recv(Some(LOOP_DEADLINE)).unwrap();
    assert_eq!({ output.meta.frame_idx }, 13);
    until(|| test.enhance.status(true).frames_out == 2);
    let status = test.enhance.status(true);
    assert_eq!(status.frames_in, 6);
    assert_eq!(status.dropped_frames, 2);
    assert_eq!(status.last_error, None, "真实增强输出后清除当前 gap 故障");
    test.finish();
    test.no_output();
    assert_eq!(test.enhance.status(true).dropped_frames, 4);
}

#[test]
fn run_mid_request_disable_discards_old_reply_and_bypasses_without_nn() {
    let mut test = LoopTest::new("mid-disable", 1000);
    let source = test.request(0, 0, true);
    assert_eq!(
        test.reply(source, false, false),
        size_of::<EnhanceReply>() as isize
    );
    let second = test.request(1, 1, false);
    assert_eq!(
        test.reply(second, false, false),
        size_of::<EnhanceReply>() as isize
    );
    test.request(2, 2, false);
    let epoch = test.shared.denoise_epoch();
    let mut dsp = test.shared.dsp_target();
    dsp.denoise = false;
    test.shared.set_dsp_target(dsp);
    assert_eq!(test.shared.denoise_epoch(), epoch + 1);
    let sent = test.reply(source, true, false);
    assert!(sent == size_of::<EnhanceReply>() as isize || sent == -1);
    test.push(3, 3);
    let output = test.consumer.recv(Some(LOOP_DEADLINE)).unwrap();
    assert_eq!({ output.meta.frame_idx }, 3);
    assert_eq!({ output.meta.pts_ns }, 60_000_000);
    assert_eq!({ output.pcm }, { item(3).frame.pcm });
    test.finish();
    test.no_output();
    let status = test.enhance.status(false);
    assert_eq!(status.frames_in, 3);
    assert_eq!(status.frames_out, 0);
    assert_eq!(status.dropped_frames, 3);
    assert!(!status.connected);
}

#[test]
fn run_disconnect_bad_reply_and_timeout_reconnect_only_on_new_frames() {
    for (tag, reason) in [
        ("disconnect", "bad enhancement reply length or disconnected"),
        ("bad-reply", "bad enhancement magic or priming state"),
        ("timeout", "enhancement request timeout"),
    ] {
        let mut test = LoopTest::new(tag, if tag == "timeout" { 100 } else { 1000 });
        let source = test.request(0, 0, true);
        assert_eq!(
            test.reply(source, false, false),
            size_of::<EnhanceReply>() as isize
        );
        let pending = test.request(1, 1, false);
        match tag {
            "disconnect" => test.commands.send(StubCommand::Disconnect).unwrap(),
            "bad-reply" => assert_eq!(
                test.reply(pending, false, true),
                size_of::<EnhanceReply>() as isize
            ),
            "timeout" => {}
            _ => unreachable!(),
        }
        until(|| test.enhance.status(true).last_error.is_some());
        let status = test.enhance.status(true);
        assert_eq!(
            status.last_error.as_deref(),
            Some(format!("enhancement: {reason}").as_str())
        );
        assert!(!status.connected);
        assert_eq!(status.frames_in, 2);
        assert_eq!(status.frames_out, 0);
        assert_eq!(status.dropped_frames, 2);
        test.no_output();
        test.session(2, 2);
        until(|| test.enhance.status(true).frames_out == 1);
        assert_eq!(test.enhance.status(true).frames_in, 5);
        assert_eq!(test.enhance.status(true).dropped_frames, 2);
        assert_eq!(test.enhance.status(true).last_error, None);
        assert!(test.enhance.status(true).connected);
        test.finish();
        test.no_output();
        assert_eq!(test.enhance.status(true).dropped_frames, 4);
    }
}

#[test]
fn run_disabled_epoch_bypasses_nn_and_reenable_starts_fresh_session() {
    let mut test = LoopTest::new("disabled-epoch", 1000);
    let epoch = test.shared.denoise_epoch();
    let mut dsp = test.shared.dsp_target();
    dsp.denoise = false;
    test.shared.set_dsp_target(dsp);
    assert_eq!(test.shared.denoise_epoch(), epoch + 1);
    test.push(0, 0);
    let frame = test.consumer.recv(Some(LOOP_DEADLINE)).unwrap();
    assert_eq!({ frame.pcm }, { item(0).frame.pcm });
    assert_eq!(test.enhance.status(false).frames_in, 0);
    assert!(!test.enhance.status(false).connected);
    assert_eq!(test.enhance.status(false).last_error, None);
    dsp.denoise = true;
    test.shared.set_dsp_target(dsp);
    assert_eq!(test.shared.denoise_epoch(), epoch + 2);
    test.session(1, 1);
    until(|| test.enhance.status(true).frames_out == 1);
    assert_eq!(test.enhance.status(true).frames_in, 3);
    test.finish();
}

#[test]
fn run_hot_switch_between_wola_and_ulunas_preserves_wola_metadata_and_reconnects() {
    let mut test = LoopTest::new("hot-switch-wola", 1000);
    test.session(0, 0);
    until(|| test.enhance.status(true).frames_out == 1);

    // 运行中用真实 TrunkProcessor + DenoiseProcessor 驱动：UL-UNAS -> WOLA -> Off -> UL-UNAS。
    let mut pipeline = TrunkProcessor::new(false, false, false);
    let mut proc = DenoiseProcessor::new(DenoiseMode::Backend(DenoiseBackend::Ulunas));
    let mut pcm = [0.0f32; RAW_SAMPLES];
    for (i, s) in pcm.iter_mut().enumerate() {
        *s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).sin();
    }
    let raw = |pcm| RawFrame {
        meta: FrameMeta {
            pts_ns: 60_000_000,
            frame_idx: 3,
            vad_flag: 0,
            energy_db: 0,
            conf: 0,
            segment_id: 0,
        },
        pcm,
    };

    // 1) 热切到 WOLA：不走 IPC、保留 WOLA 真实光谱置信度与 VAD，状态报告 wola。
    let prev_epoch = test.shared.denoise_epoch();
    let (enabled, backend) = test
        .shared
        .set_denoise_target(Some(true), Some(DenoiseBackend::Wola))
        .unwrap();
    assert!(enabled);
    assert_eq!(backend, DenoiseBackend::Wola);
    assert_eq!(test.shared.denoise_epoch(), prev_epoch + 1);
    let pre_status = test.shared.status(crate::state::QueueReadout::default());
    assert_eq!(pre_status.denoise_backend, "wola");
    assert!(!pre_status.enhancement.unwrap().enabled);

    proc.sync(DenoiseMode::Backend(DenoiseBackend::Wola), 0);
    // 连续喂帧越过 WOLA 初始化期与 3 帧 VAD 进入滞回，拿真实非零光谱 conf 与 vad_flag=1，逐帧送 worker 扇出。
    let mut saw_positive_conf = false;
    let mut saw_vad_active = false;
    for seq in 0..8u32 {
        let mut trunk_frame = raw(pcm);
        trunk_frame.meta.frame_idx = 3 + seq;
        let conf = proc.process_trunk_frame(&mut pipeline, &mut trunk_frame, |_| {});
        let packed = pipeline.pack_frame(&trunk_frame, conf);
        let work = proc.build_work_item(packed, 3 + seq, true, 1, prev_epoch + 1);
        assert!(work.enhance_backend.is_none());
        test.enhance.queue.push(work);
        let out = test.consumer.recv(Some(LOOP_DEADLINE)).unwrap();
        if out.meta.conf > 0 {
            saw_positive_conf = true;
        }
        if out.meta.vad_flag == 1 {
            saw_vad_active = true;
        }
    }
    assert!(saw_positive_conf, "WOLA 经过初始化窗后应保留真实非零光谱 conf");
    assert!(saw_vad_active, "WOLA 连续有声帧后应触发 vad_flag=1");
    assert!(!test.enhance.status(false).connected);

    // 2) 热切到 Off：同样不走 IPC，conf=0 但能量 VAD 点亮 vad_flag=1。
    test.shared
        .set_denoise_target(Some(false), None)
        .unwrap();
    proc.sync(DenoiseMode::Off, 0);
    let mut off_frame = raw(pcm);
    off_frame.meta.frame_idx = 11;
    let conf_off = proc.process_trunk_frame(&mut pipeline, &mut off_frame, |_| {});
    assert_eq!(conf_off, None);
    let packed_off = pipeline.pack_frame(&off_frame, conf_off);
    test.enhance.queue.push(proc.build_work_item(
        packed_off,
        11,
        true,
        1,
        test.shared.denoise_epoch(),
    ));
    let out_off = test.consumer.recv(Some(LOOP_DEADLINE)).unwrap();
    assert_eq!(out_off.meta.conf, 0);
    assert_eq!(out_off.meta.vad_flag, 1);

    // 3) 运行中热切回 UL-UNAS：开启全新 priming 会话。
    test.shared
        .set_denoise_target(Some(true), Some(DenoiseBackend::Ulunas))
        .unwrap();
    test.session(12, 12);
    until(|| test.enhance.status(true).frames_out == 2);
    test.finish();
}

#[test]
fn denoise_processor_switches_cleanly_between_off_wola_and_ulunas() {
    let mut pipeline = TrunkProcessor::new(false, false, false);
    let mut proc = DenoiseProcessor::new(DenoiseMode::Off);
    let mut pcm = [0.0f32; RAW_SAMPLES];
    for (i, s) in pcm.iter_mut().enumerate() {
        *s = 0.4 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).sin();
    }
    let mut frame = RawFrame {
        meta: FrameMeta {
            pts_ns: 0,
            frame_idx: 0,
            vad_flag: 0,
            energy_db: 0,
            conf: 0,
            segment_id: 0,
        },
        pcm,
    };

    // Off：返回 None、conf=0、能量 VAD 正常置位。
    let conf_off = proc.process_trunk_frame(&mut pipeline, &mut frame, |_| {});
    assert_eq!(conf_off, None);
    let packed_off = pipeline.pack_frame(&frame, conf_off);
    assert_eq!(packed_off.meta.conf, 0);
    assert_eq!(packed_off.meta.vad_flag, 1);

    // 切到 WOLA：返回 Some(conf)，且首次有 64 样本延迟补零（说明已重置启动）。
    proc.sync(DenoiseMode::Backend(DenoiseBackend::Wola), 0);
    let mut wola_frame = frame;
    let conf_wola = proc.process_trunk_frame(&mut pipeline, &mut wola_frame, |_| {});
    assert!(conf_wola.is_some());
    let wola_pcm = wola_frame.pcm;
    assert_eq!(wola_pcm[896..], [0.0; 64]);

    // 切到 UL-UNAS：主干旁路返回 None，WorkItem 标记 enhance_backend = Some(Ulunas)。
    proc.sync(DenoiseMode::Backend(DenoiseBackend::Ulunas), 0);
    let conf_nn = proc.process_trunk_frame(&mut pipeline, &mut frame, |_| {});
    assert_eq!(conf_nn, None);
    let packed_nn = pipeline.pack_frame(&frame, conf_nn);
    let work = proc.build_work_item(packed_nn, 1, true, 1, 2);
    assert_eq!(work.enhance_backend, Some(DenoiseBackend::Ulunas));
}

#[test]
fn fanout_rejects_old_file_generation_only_for_affected_consumer() {
    let dir = std::env::temp_dir().join(format!("mozart-generation-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let shared = crate::state::test_shared("cap", "play", "boot");
    shared.set_mode_asserted(true);
    let rvc = FrameSocket::<InputFrame>::bind(&dir.join("rvc.sock"), 8).unwrap();
    let asr = FrameSocket::<InputFrame>::bind(&dir.join("asr.sock"), 8).unwrap();
    let mut senders = vec![
        ai::Sender::new(dir.join("rvc.sock"), 0, "rvc"),
        ai::Sender::new(dir.join("asr.sock"), 1, "asr"),
    ];
    let mut source = item(0);
    source.file_generation = shared.file_generation();
    shared.set_file_mode(true);
    shared.set_file_mode(false);
    fanout(&source, &shared, &mut senders);
    asr.recv(Some(LOOP_DEADLINE)).unwrap();
    assert!(rvc.recv(Some(Duration::ZERO)).is_err());
    source.file_generation = shared.file_generation();
    fanout(&source, &shared, &mut senders);
    asr.recv(Some(LOOP_DEADLINE)).unwrap();
    rvc.recv(Some(LOOP_DEADLINE)).unwrap();
    assert!(shared.sends_idle());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn historical_queue_drops_do_not_recreate_current_error() {
    let enhance = UlunasWorker::new(
        &EnhanceConfig {
            model: "unused.onnx".into(),
            socket: "enhance.sock".into(),
            audio_socket: "audio.sock".into(),
            queue_frames: 1,
            request_timeout_ms: 100,
        },
        Path::new("/tmp"),
    );
    enhance.queue.push(item(0));
    enhance.queue.push(item(1));
    let status = enhance.status(true);
    assert_eq!(status.dropped_frames, 1);
    assert_eq!(status.last_error, None);
}

fn item(idx: u32) -> WorkItem {
    let mut frame: InputFrame = unsafe { zeroed() };
    frame.meta.frame_idx = idx;
    frame.meta.pts_ns = u64::from(idx) * 20_000_000;
    frame.pcm = [0.1; INPUT_SAMPLES];
    WorkItem {
        frame,
        source_idx: idx,
        rvc_ai_active: true,
        file_generation: 1,
        epoch: 0,
        enhance_backend: Some(DenoiseBackend::Ulunas),
    }
}

fn pair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        },
        0
    );
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn raw_send(fd: &OwnedFd, reply: &EnhanceReply, extra: usize) {
    let mut bytes = vec![0u8; size_of::<EnhanceReply>() + extra];
    unsafe {
        std::ptr::copy_nonoverlapping(
            reply as *const _ as *const u8,
            bytes.as_mut_ptr(),
            size_of::<EnhanceReply>(),
        );
    }
    assert_eq!(
        unsafe {
            libc::send(
                fd.as_raw_fd(),
                bytes.as_ptr() as *const _,
                bytes.len(),
                libc::MSG_NOSIGNAL,
            )
        },
        bytes.len() as isize
    );
}

#[test]
fn raw_protocol_priming_metadata_and_bad_packets() {
    let shared = crate::state::test_shared("cap", "play", "boot");
    let stop = AtomicBool::new(false);
    let (client, server) = pair();
    let source = item(7);
    for ready in [false, false, true] {
        let reply = EnhanceReply {
            magic: ENHANCE_PACKET_MAGIC,
            ready: u32::from(ready),
            frame: source.frame,
        };
        raw_send(&server, &reply, 0);
        let reply = exchange(
            &client,
            &source.frame,
            Instant::now() + Duration::from_millis(100),
            &stop,
            &shared,
            0,
        )
        .unwrap();
        validate(&reply, &source, ready).unwrap();
        if ready {
            let mut analyzer = InputFrameAnalyzer::default();
            let mut output = reply.frame;
            analyzer.analyze(&mut output);
            let meta = output.meta;
            assert_eq!({ meta.frame_idx }, 7);
            assert_eq!({ meta.pts_ns }, 140_000_000);
            assert_eq!(meta.conf, 0);
            assert_eq!(meta.vad_flag, 1);
            assert!(meta.energy_db > 0);
        }
    }
    for kind in 0..4 {
        let mut reply = EnhanceReply {
            magic: ENHANCE_PACKET_MAGIC,
            ready: 1,
            frame: source.frame,
        };
        match kind {
            0 => reply.magic = 0,
            1 => reply.ready = 4,
            2 => reply.frame.pcm = [f32::NAN; INPUT_SAMPLES],
            _ => reply.frame.meta.pts_ns = 1,
        }
        raw_send(&server, &reply, 0);
        let reply = exchange(
            &client,
            &source.frame,
            Instant::now() + Duration::from_millis(100),
            &stop,
            &shared,
            0,
        )
        .unwrap();
        assert!(validate(&reply, &source, true).is_err());
    }
    let reply = EnhanceReply {
        magic: ENHANCE_PACKET_MAGIC,
        ready: 1,
        frame: source.frame,
    };
    raw_send(&server, &reply, 1);
    assert!(exchange(
        &client,
        &source.frame,
        Instant::now() + Duration::from_millis(100),
        &stop,
        &shared,
        0
    )
    .is_err());
}

#[test]
fn timeout_and_peer_disconnect_are_bounded() {
    let shared = crate::state::test_shared("cap", "play", "boot");
    let stop = AtomicBool::new(false);
    let (client, server) = pair();
    let start = Instant::now();
    let result = exchange(
        &client,
        &item(1).frame,
        start + Duration::from_millis(25),
        &stop,
        &shared,
        0,
    );
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(server);
    assert!(exchange(
        &client,
        &item(2).frame,
        Instant::now() + Duration::from_millis(25),
        &stop,
        &shared,
        0
    )
    .is_err());
}

#[test]
fn disabled_worker_does_not_connect_and_preserves_source_gate() {
    let dir = std::env::temp_dir().join(format!("mozart-enhance-disable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut shared = crate::state::test_shared("cap", "play", "boot");
    shared.set_dsp_target(crate::state::DspSwitches {
        denoise: false,
        gate: false,
        agc: false,
        limiter: false,
    });
    shared.set_mode_asserted(true);
    let config = EnhanceConfig {
        model: "unused.onnx".into(),
        socket: "enhance.sock".into(),
        audio_socket: "missing.sock".into(),
        queue_frames: 2,
        request_timeout_ms: 20,
    };
    shared.configure_enhancement(&config, &dir);
    let enhance = shared.enhancement.clone().unwrap();
    let shared = Arc::new(shared);
    let rvc = FrameSocket::<InputFrame>::bind(&dir.join("rvc.sock"), 8).unwrap();
    let asr = FrameSocket::<InputFrame>::bind(&dir.join("asr.sock"), 8).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    for idx in 0..4 {
        let mut work = item(idx);
        work.enhance_backend = None;
        work.epoch = shared.denoise_epoch();
        work.rvc_ai_active = false;
        enhance.queue.push(work);
    }
    let handle = {
        let (enhance, shared, stop) = (enhance.clone(), shared.clone(), stop.clone());
        let senders = vec![
            ai::Sender::new(dir.join("rvc.sock"), 0, "rvc"),
            ai::Sender::new(dir.join("asr.sock"), 1, "asr"),
        ];
        std::thread::spawn(move || run(enhance, shared, senders, &stop))
    };
    for _ in 0..2 {
        asr.recv(Some(Duration::from_secs(1))).unwrap();
    }
    assert!(rvc.recv(Some(Duration::from_millis(30))).is_err());
    let status = enhance.status(false);
    assert_eq!(status.frames_in, 0);
    assert!(!status.connected);
    assert_eq!(status.dropped_frames, 2);
    assert!(status.last_error.is_some());
    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

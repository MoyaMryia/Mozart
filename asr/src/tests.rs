//! 进程内测试：线程交接、队列满丢弃、SIGTERM 收尾、输出断连补发。
//! 识别器用假实现经 run_with 注入；socket 用真实临时路径，事件按 JSON 校验。
//! 停止标志每用例独立（'static 泄漏），并行用例互不置位；操纵全局 STOP 的
//! 用例用互斥锁串行（同 pre 的纪律）。

use crate::decoder::{self, Decoder, MAX_UTTERANCE_FRAMES};
use crate::event::{Event, FINAL, PARTIAL, REFINED, REVIEW_COMPLETED, REVIEW_FAILED, REVIEW_SKIPPED};
use crate::ffi::{OfflineRecognizer, Recognizer, Stream};
use crate::output::{self, EventListener};
use crate::receiver::{self, Inbox};
use crate::review::{self, ReviewJob};
use crate::state::Shared;
use mozart_core::dgram::FrameSocket;
use mozart_core::ring::Ring;
use mozart_core::{FrameMeta, InputFrame, INPUT_SAMPLES};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FRAME_NS: u64 = mozart_core::FRAME_MS as u64 * 1_000_000;

/// 每用例独立的停止标志：run_with 系列要求 &'static，泄漏换取隔离。
fn stop_flag() -> &'static AtomicBool {
    Box::leak(Box::new(AtomicBool::new(false)))
}

/// 用例独立临时目录（pid + 序号防并行碰撞）。
fn temp_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "mozart-asr-test-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn frame(idx: u32) -> InputFrame {
    InputFrame {
        meta: FrameMeta {
            pts_ns: idx as u64 * FRAME_NS,
            frame_idx: idx,
            vad_flag: 1,
            energy_db: 0,
            conf: 0,
            segment_id: 0,
        },
        pcm: [0.25; INPUT_SAMPLES],
    }
}

/// 轮询等待条件成立，超时失败（线程交接的确定性等待）。
fn wait_until(what: &str, timeout: Duration, pred: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !pred() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 收集通道中当前全部事件（发送方已释放后调用）。
fn events_now(rx: &Receiver<Event>) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn final_event(seq: u64) -> Event {
    Event::final_utterance(
        "teststart",
        seq,
        format!("在线{seq}"),
        Some(0),
        Some(FRAME_NS),
        true,
    )
}

fn review_job(seq: u64) -> ReviewJob {
    ReviewJob {
        final_event: final_event(seq),
        pcm: vec![0.1; INPUT_SAMPLES],
    }
}

/// 构造 sockaddr_un（同 core 约定），供原始套接字与事件客户端使用。
fn sockaddr(path: &Path) -> (libc::sockaddr_un, libc::socklen_t) {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: sockaddr_un 可由全零位模式表示。
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    assert!(bytes.len() < addr.sun_path.len());
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    (addr, len as libc::socklen_t)
}

/// 最小 SEQPACKET 事件客户端：连接 + 带超时收一条完整消息。
struct EventClient {
    fd: OwnedFd,
}

impl EventClient {
    fn connect(path: &Path) -> Self {
        // SAFETY: socket() 不访问其他内存。
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        assert!(raw >= 0, "socket: {}", io::Error::last_os_error());
        let (addr, len) = sockaddr(path);
        // SAFETY: fd 有效；addr 指向已初始化的 sockaddr_un。
        let rc = unsafe { libc::connect(raw, (&addr as *const libc::sockaddr_un).cast(), len) };
        assert_eq!(rc, 0, "connect: {}", io::Error::last_os_error());
        // SAFETY: raw 是刚创建且未复制的文件描述符。
        Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        }
    }

    fn recv(&self, timeout: Duration) -> io::Result<Vec<u8>> {
        let mut pollfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        // SAFETY: pollfd 指向单个已初始化结构。
        let rc = unsafe { libc::poll(&mut pollfd, 1, ms) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if rc == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "事件等待超时"));
        }
        let mut buf = vec![0u8; 65536];
        // SAFETY: buf 有效且长度与传入一致。
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }
}

fn read_event(client: &EventClient) -> serde_json::Value {
    let bytes = client.recv(Duration::from_secs(2)).expect("读事件");
    serde_json::from_slice(&bytes).expect("事件是 JSON")
}

// ---------------------------------------------------------------------------
// 假识别器
// ---------------------------------------------------------------------------

/// 假流式识别器状态：句号从 1 起，reset 递增；记录收尾喂入的零样本与 finished。
struct FakeState {
    utterance: u64,
    fed: usize,
    tail_zeros: usize,
    finished: bool,
}

/// 每句文本「第N句」，喂满 frames_per_utterance 帧触发 endpoint。
struct FakeOnline {
    state: Arc<Mutex<FakeState>>,
    frames_per_utterance: usize,
}

impl FakeOnline {
    fn new(frames_per_utterance: usize) -> (Box<dyn Recognizer>, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState {
            utterance: 1,
            fed: 0,
            tail_zeros: 0,
            finished: false,
        }));
        (
            Box::new(Self {
                state: state.clone(),
                frames_per_utterance,
            }),
            state,
        )
    }
}

impl Recognizer for FakeOnline {
    fn create_stream(&self) -> Box<dyn Stream> {
        Box::new(FakeStream {
            state: self.state.clone(),
            frames_per_utterance: self.frames_per_utterance,
        })
    }
}

struct FakeStream {
    state: Arc<Mutex<FakeState>>,
    frames_per_utterance: usize,
}

impl Stream for FakeStream {
    fn accept_waveform(&mut self, samples: &[f32]) {
        let mut state = self.state.lock().unwrap();
        state.fed += 1;
        if samples.iter().all(|&sample| sample == 0.0) {
            state.tail_zeros += samples.len();
        } else {
            state.tail_zeros = 0;
        }
    }
    fn decode(&mut self) {}
    fn get_result(&self) -> String {
        format!("第{}句", self.state.lock().unwrap().utterance)
    }
    fn reset(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.utterance += 1;
        state.fed = 0;
    }
    fn is_endpoint(&self) -> bool {
        self.state.lock().unwrap().fed >= self.frames_per_utterance
    }
    fn input_finished(&mut self) {
        self.state.lock().unwrap().finished = true;
    }
}

/// 假离线识别器：固定文本，可配延迟；started 计数供测试等待首句开解码。
struct FakeOffline {
    text: String,
    delay: Duration,
    started: Arc<AtomicU64>,
}

impl FakeOffline {
    fn new(text: &str, delay: Duration) -> (Box<dyn OfflineRecognizer>, Arc<AtomicU64>) {
        let started = Arc::new(AtomicU64::new(0));
        (
            Box::new(Self {
                text: text.to_string(),
                delay,
                started: started.clone(),
            }),
            started,
        )
    }
}

impl OfflineRecognizer for FakeOffline {
    fn decode(&self, _samples: &[f32]) -> Result<String, String> {
        self.started.fetch_add(1, Relaxed);
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        Ok(self.text.clone())
    }
}

// ---------------------------------------------------------------------------
// 解码线程
// ---------------------------------------------------------------------------

#[test]
fn decoder_endpoint_cuts_final_and_submits_review() {
    let (fake, _state) = FakeOnline::new(2);
    let (review_tx, review_rx) = mpsc::sync_channel::<ReviewJob>(2);
    let (tx, rx) = mpsc::sync_channel(crate::event::EVENT_QUEUE_CAPACITY);
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let mut dec = Decoder::new(fake, Some(review_tx), tx, shared.clone());

    dec.feed(frame(0)).unwrap();
    dec.feed(frame(1)).unwrap(); // endpoint → 第 1 句
    dec.feed(frame(2)).unwrap();
    dec.feed(frame(3)).unwrap(); // endpoint → 第 2 句
    drop(dec);

    let events = events_now(&rx);
    let finals: Vec<&Event> = events.iter().filter(|e| e.kind == FINAL).collect();
    assert_eq!(finals.len(), 2);
    assert_eq!(finals[0].seq, 1);
    assert_eq!(finals[0].utterance_id, "teststart:1");
    assert_eq!(finals[0].text, "第1句");
    assert_eq!(finals[0].audio_start_pts_ns, Some(0));
    assert_eq!(finals[0].audio_end_pts_ns, Some(2 * FRAME_NS));
    assert_eq!(finals[0].refinement_status.as_deref(), Some("pending"));
    // 第二句起点接在第一句之后（流复用，缓冲已清空）。
    assert_eq!(finals[1].seq, 2);
    assert_eq!(finals[1].utterance_id, "teststart:2");
    assert_eq!(finals[1].text, "第2句");
    assert_eq!(finals[1].audio_start_pts_ns, Some(2 * FRAME_NS));
    assert_eq!(finals[1].audio_end_pts_ns, Some(4 * FRAME_NS));

    // 整句 PCM 所有权移交复核：每句 2 帧。
    let job1 = review_rx.try_recv().unwrap();
    assert_eq!(job1.final_event.seq, 1);
    assert_eq!(job1.pcm.len(), 2 * INPUT_SAMPLES);
    let job2 = review_rx.try_recv().unwrap();
    assert_eq!(job2.final_event.seq, 2);
    assert!(review_rx.try_recv().is_err());
    assert_eq!(shared.status(0).finals, 2);
}

#[test]
fn decoder_partial_throttled_and_deduplicated() {
    let (fake, _state) = FakeOnline::new(1000);
    let (tx, rx) = mpsc::sync_channel(crate::event::EVENT_QUEUE_CAPACITY);
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let mut dec = Decoder::new(fake, None, tx, shared);

    // 首帧距构造 ≥ 100 ms 才发 partial。
    std::thread::sleep(Duration::from_millis(110));
    dec.feed(frame(0)).unwrap();
    let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(event.kind, PARTIAL);
    assert_eq!(event.seq, 1);
    assert_eq!(event.utterance_id, "teststart:1");
    assert_eq!(event.text, "第1句");
    assert!(event.refinement_status.is_none());

    // 节流窗口内同文本：不发。
    dec.feed(frame(1)).unwrap();
    assert!(rx.try_recv().is_err());
    // 窗口外同文本：去重不发。
    std::thread::sleep(Duration::from_millis(110));
    dec.feed(frame(2)).unwrap();
    assert!(rx.try_recv().is_err());
}

#[test]
fn decoder_utterance_cap_is_fatal() {
    let (fake, _state) = FakeOnline::new(usize::MAX); // 永不 endpoint
    let ring = Arc::new(Ring::new(MAX_UTTERANCE_FRAMES + 16));
    for idx in 0..=(MAX_UTTERANCE_FRAMES as u32) {
        ring.push(Inbox::Frame(frame(idx)));
    }
    let (tx, _rx) = mpsc::sync_channel(crate::event::EVENT_QUEUE_CAPACITY);
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let result = decoder::run_with(fake, ring, None, tx, shared, stop_flag());
    let message = result.unwrap_err();
    assert!(message.contains("700"), "致命错误应带上限：{message}");
}

#[test]
fn sigterm_flush_feeds_silence_and_cuts_final() {
    let (fake, state) = FakeOnline::new(1000); // 不 endpoint，靠收尾切句
    let ring = Arc::new(Ring::new(8));
    ring.push(Inbox::Frame(frame(0)));
    ring.push(Inbox::Frame(frame(1)));
    let (review_tx, review_rx) = mpsc::sync_channel::<ReviewJob>(2);
    let (tx, rx) = mpsc::sync_channel(crate::event::EVENT_QUEUE_CAPACITY);
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let stop = stop_flag();
    let worker_shared = shared.clone();
    let handle = std::thread::spawn(move || {
        decoder::run_with(fake, ring, Some(review_tx), tx, worker_shared, stop)
    });

    wait_until("解码消费两帧", Duration::from_secs(2), || {
        state.lock().unwrap().fed >= 2
    });
    stop.store(true, Relaxed);
    handle.join().unwrap().unwrap();

    // 收尾喂 0.3 s 静音（4800 零样本）并标记输入结束。
    let state = state.lock().unwrap();
    assert_eq!(state.tail_zeros, 4800);
    assert!(state.finished);
    drop(state);

    let events = events_now(&rx);
    let final_event = events.iter().find(|e| e.kind == FINAL).expect("收尾 final");
    assert_eq!(final_event.seq, 1);
    assert_eq!(final_event.text, "第1句");
    // 静音只喂识别器，不进句音频。
    let job = review_rx.try_recv().unwrap();
    assert_eq!(job.pcm.len(), 2 * INPUT_SAMPLES);
    assert_eq!(shared.status(0).state, "idle");
}

// ---------------------------------------------------------------------------
// 复核线程
// ---------------------------------------------------------------------------

#[test]
fn review_queue_full_marks_skipped() {
    let (fake, _state) = FakeOnline::new(1); // 每帧 endpoint
    let (review_tx, review_rx) = mpsc::sync_channel::<ReviewJob>(2);
    let (tx, rx) = mpsc::sync_channel(crate::event::EVENT_QUEUE_CAPACITY);
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let mut dec = Decoder::new(fake, Some(review_tx), tx, shared.clone());

    // 复核队列容量 2 且不消费：第 3 句队满标 skipped。
    for idx in 0..3 {
        dec.feed(frame(idx)).unwrap();
    }
    drop(dec);

    let events = events_now(&rx);
    let kinds: Vec<(&str, u64)> = events.iter().map(|e| (e.kind, e.seq)).collect();
    assert_eq!(
        kinds,
        vec![(FINAL, 1), (FINAL, 2), (FINAL, 3), (REFINED, 3)]
    );
    let skipped = &events[3];
    assert_eq!(skipped.refinement_status.as_deref(), Some(REVIEW_SKIPPED));
    assert!(skipped
        .refinement_error
        .as_deref()
        .unwrap()
        .contains("队列"));
    assert_eq!(shared.status(0).review_skipped, 1);
    assert_eq!(shared.status(0).finals, 3);
    // 前两句仍在队列，所有权已移交。
    assert_eq!(review_rx.try_recv().unwrap().final_event.seq, 1);
    assert_eq!(review_rx.try_recv().unwrap().final_event.seq, 2);
    assert!(review_rx.try_recv().is_err());
}

#[test]
fn review_load_failure_reports_each_utterance() {
    let (job_tx, job_rx) = mpsc::sync_channel::<ReviewJob>(2);
    let (tx, rx) = mpsc::sync_channel(crate::event::EVENT_QUEUE_CAPACITY);
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let worker_shared = shared.clone();
    let handle = std::thread::spawn(move || {
        review::run_with(
            job_rx,
            Err("模型加载失败".to_string()),
            tx,
            worker_shared,
            stop_flag(),
        )
    });
    job_tx.send(review_job(1)).unwrap();
    job_tx.send(review_job(2)).unwrap();
    drop(job_tx);
    handle.join().unwrap();

    let events = events_now(&rx);
    assert_eq!(events.len(), 2);
    for (event, seq) in events.iter().zip([1, 2]) {
        assert_eq!(event.kind, REFINED);
        assert_eq!(event.seq, seq);
        assert_eq!(event.refinement_status.as_deref(), Some(REVIEW_FAILED));
        assert_eq!(event.refinement_error.as_deref(), Some("模型加载失败"));
        // 在线文本保留。
        assert_eq!(event.text, format!("在线{seq}"));
    }
    let status = shared.status(0);
    assert_eq!(status.review_failed, 2);
    assert_eq!(status.last_error.as_deref(), Some("模型加载失败"));
}

#[test]
fn review_completed_event() {
    let (fake, _started) = FakeOffline::new("复核文本", Duration::ZERO);
    let (job_tx, job_rx) = mpsc::sync_channel::<ReviewJob>(2);
    let (tx, rx) = mpsc::sync_channel(crate::event::EVENT_QUEUE_CAPACITY);
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let worker_shared = shared.clone();
    let handle = std::thread::spawn(move || {
        review::run_with(job_rx, Ok(fake), tx, worker_shared, stop_flag())
    });
    job_tx.send(review_job(1)).unwrap();
    drop(job_tx);
    handle.join().unwrap();

    let events = events_now(&rx);
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.kind, REFINED);
    assert_eq!(event.refinement_status.as_deref(), Some(REVIEW_COMPLETED));
    assert_eq!(event.refined_text.as_deref(), Some("复核文本"));
    assert_eq!(event.text, "在线1");
    assert!(event.final_decode_ms.is_some());
    assert_eq!(shared.status(0).review_failed, 0);
}

#[test]
fn review_empty_text_is_failed() {
    let (fake, _started) = FakeOffline::new("", Duration::ZERO);
    let (job_tx, job_rx) = mpsc::sync_channel::<ReviewJob>(2);
    let (tx, rx) = mpsc::sync_channel(crate::event::EVENT_QUEUE_CAPACITY);
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let worker_shared = shared.clone();
    let handle = std::thread::spawn(move || {
        review::run_with(job_rx, Ok(fake), tx, worker_shared, stop_flag())
    });
    job_tx.send(review_job(1)).unwrap();
    drop(job_tx);
    handle.join().unwrap();

    let events = events_now(&rx);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].refinement_status.as_deref(), Some(REVIEW_FAILED));
    assert!(events[0].refinement_error.is_some());
    assert_eq!(shared.status(0).review_failed, 1);
}

#[test]
fn review_drain_deadline_skips_remaining() {
    // 每句解码 1.5 s：停止后第 1 句在途完成，限时窗口（1 s）内第 2 句完成，
    // 第 3 句出窗时已超时限 → skipped。
    let (fake, started) = FakeOffline::new("慢复核", Duration::from_millis(1500));
    let (job_tx, job_rx) = mpsc::sync_channel::<ReviewJob>(2);
    let (tx, rx) = mpsc::sync_channel(crate::event::EVENT_QUEUE_CAPACITY);
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let stop = stop_flag();
    let worker_shared = shared.clone();
    let handle = std::thread::spawn(move || {
        review::run_with(job_rx, Ok(fake), tx, worker_shared, stop)
    });

    job_tx.try_send(review_job(1)).unwrap();
    wait_until("首句开解码", Duration::from_secs(2), || {
        started.load(Relaxed) == 1
    });
    job_tx.try_send(review_job(2)).unwrap();
    job_tx.try_send(review_job(3)).unwrap();
    stop.store(true, Relaxed);
    drop(job_tx);
    handle.join().unwrap();

    let events = events_now(&rx);
    let statuses: Vec<(u64, &str)> = events
        .iter()
        .map(|e| (e.seq, e.refinement_status.as_deref().unwrap()))
        .collect();
    assert_eq!(
        statuses,
        vec![(1, REVIEW_COMPLETED), (2, REVIEW_COMPLETED), (3, REVIEW_SKIPPED)]
    );
    assert_eq!(shared.status(0).review_skipped, 1);
}

// ---------------------------------------------------------------------------
// 收包线程
// ---------------------------------------------------------------------------

#[test]
fn receiver_counts_rejects_and_idle_cuts() {
    let dir = temp_dir("recv");
    let path = dir.join("in.sock");
    let socket = FrameSocket::<InputFrame>::bind(&path, 8).unwrap();
    let ring = Arc::new(Ring::new(8));
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let stop = stop_flag();
    let worker_shared = shared.clone();
    let worker_ring = ring.clone();
    let handle = std::thread::spawn(move || {
        receiver::run(socket, worker_ring, worker_shared, stop)
    });

    let sender = FrameSocket::<InputFrame>::connect(&path, 8).unwrap();
    sender.send(&frame(0)).unwrap();
    match ring.pop(Some(Duration::from_secs(2))).unwrap() {
        Inbox::Frame(got) => {
            let meta = got.meta;
            let idx = meta.frame_idx;
            assert_eq!(idx, 0);
        }
        Inbox::EndOfInput => panic!("首包应是音频帧"),
    }
    let status = shared.status(0);
    assert_eq!(status.frames_received, 1);
    assert!(status.last_packet_unix_ms > 0);

    // 坏魔数包：拒收计数，不影响后续好包。
    // SAFETY: socket() 不访问其他内存。
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    assert!(raw >= 0);
    let (addr, len) = sockaddr(&path);
    let bad = [0xFFu8; 1300];
    // SAFETY: bad 有效；addr 指向已初始化的 sockaddr_un。
    let sent = unsafe {
        libc::sendto(
            raw,
            bad.as_ptr().cast(),
            bad.len(),
            libc::MSG_NOSIGNAL,
            (&addr as *const libc::sockaddr_un).cast(),
            len,
        )
    };
    assert_eq!(sent as usize, bad.len());
    // SAFETY: raw 是本用例创建且未复制的文件描述符。
    drop(unsafe { OwnedFd::from_raw_fd(raw) });
    wait_until("拒收计数", Duration::from_secs(2), || {
        shared.status(0).frames_rejected == 1
    });

    // 1 s 空闲兜底：环中出现切句标记。
    match ring.pop(Some(Duration::from_secs(2))).unwrap() {
        Inbox::EndOfInput => {}
        Inbox::Frame(_) => panic!("空闲后应是切句标记"),
    }

    // 新帧恢复收包，停止后线程有序退出。
    sender.send(&frame(1)).unwrap();
    match ring.pop(Some(Duration::from_secs(2))).unwrap() {
        Inbox::Frame(got) => {
            let meta = got.meta;
            let idx = meta.frame_idx;
            assert_eq!(idx, 1);
        }
        Inbox::EndOfInput => panic!("恢复后应是音频帧"),
    }
    stop.store(true, Relaxed);
    handle.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn receiver_ring_overflow_drops_oldest() {
    let dir = temp_dir("overflow");
    let path = dir.join("in.sock");
    let socket = FrameSocket::<InputFrame>::bind(&path, 8).unwrap();
    let ring = Arc::new(Ring::new(2));
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let stop = stop_flag();
    let worker_shared = shared.clone();
    let worker_ring = ring.clone();
    let handle = std::thread::spawn(move || {
        receiver::run(socket, worker_ring, worker_shared, stop)
    });

    // 无消费者：5 帧入容量 2 的环，丢最旧 3 帧。
    let sender = FrameSocket::<InputFrame>::connect(&path, 8).unwrap();
    for idx in 0..5 {
        sender.send(&frame(idx)).unwrap();
    }
    wait_until("收齐 5 帧", Duration::from_secs(2), || {
        shared.status(0).frames_received == 5
    });
    assert_eq!(ring.dropped(), 3);
    assert_eq!(ring.depth(), 2);
    for expect in [3, 4] {
        match ring.pop(Some(Duration::from_secs(2))).unwrap() {
            Inbox::Frame(got) => {
                let meta = got.meta;
                let idx = meta.frame_idx;
                assert_eq!(idx, expect);
            }
            Inbox::EndOfInput => panic!("应是音频帧"),
        }
    }
    stop.store(true, Relaxed);
    handle.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 输出线程
// ---------------------------------------------------------------------------

#[test]
fn output_buffers_replays_and_drops_oldest() {
    let dir = temp_dir("output");
    let path = dir.join("events.sock");
    let listener = EventListener::bind(&path).unwrap();
    let (tx, rx) = mpsc::sync_channel(crate::event::EVENT_QUEUE_CAPACITY);
    let shared = Arc::new(Shared::new("teststart".to_string()));
    let worker_shared = shared.clone();
    let handle = std::thread::spawn(move || output::run(listener, rx, worker_shared, 4));

    // 桥未连接：final 入缓冲，partial 丢。
    tx.send(final_event(1)).unwrap();
    tx.send(Event::partial("teststart", 2, "中间".to_string(), None, None))
        .unwrap();
    tx.send(final_event(2)).unwrap();
    std::thread::sleep(Duration::from_millis(150));

    let client = EventClient::connect(&path);
    wait_until("桥连接", Duration::from_secs(2), || {
        shared.status(0).bridge_connected
    });
    // 按序补发 1、2；partial 不出现。
    assert_eq!(read_event(&client)["seq"], 1);
    assert_eq!(read_event(&client)["seq"], 2);

    // 断连：final 3 转缓冲；重连补发。
    drop(client);
    tx.send(final_event(3)).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let client = EventClient::connect(&path);
    wait_until("桥重连", Duration::from_secs(2), || {
        shared.status(0).bridge_connected
    });
    assert_eq!(read_event(&client)["seq"], 3);

    // 超限丢最旧：容量 4，入 5 句（4..8）丢 4，计数 1。
    drop(client);
    for seq in 4..=8 {
        tx.send(final_event(seq)).unwrap();
    }
    wait_until("丢句计数", Duration::from_secs(2), || {
        shared.status(0).event_buffer_dropped == 1
    });
    let client = EventClient::connect(&path);
    for seq in 5..=8 {
        assert_eq!(read_event(&client)["seq"], seq);
    }

    // 发送方全释放：输出线程排空后退出。
    drop(tx);
    handle.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 主线程传播链与 stub 契约
// ---------------------------------------------------------------------------

/// 操纵全局 STOP 的用例串行（同 pre 的纪律）。
static STOP_TEST_MUTEX: Mutex<()> = Mutex::new(());

#[test]
fn fatal_and_panic_propagate_to_stop_flag() {
    let _guard = STOP_TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    crate::STOP.store(false, Relaxed);
    assert!(crate::stop_on_fatal(Err("致命".to_string())).is_err());
    assert!(crate::STOP.load(Relaxed));
    crate::STOP.store(false, Relaxed);
    assert!(crate::stop_on_fatal(Ok(())).is_ok());
    assert!(!crate::STOP.load(Relaxed));

    let handle = std::thread::spawn(|| Err::<(), String>("线程失败".to_string()));
    assert_eq!(
        crate::join_threads(vec![("worker", handle)]),
        Some("worker: 线程失败".to_string())
    );
    assert!(crate::STOP.load(Relaxed));
    crate::STOP.store(false, Relaxed);

    // panic 转 Err，走同一条传播链。
    let result = crate::guard_panic(|| panic!("解码 panic {}", 42));
    assert_eq!(result, Err("解码 panic 42".to_string()));
    assert!(!crate::STOP.load(Relaxed), "guard_panic 本身不置位");
}

#[cfg(not(has_sherpa))]
#[test]
fn stub_returns_empty_results_by_default() {
    // 本进程内只有本用例读 stub 注入口，清除以固定默认契约。
    std::env::remove_var("MOZART_ASR_STUB_TEXT");
    let online = crate::ffi::load_online(Path::new(""), 1).unwrap();
    let stream = online.create_stream();
    assert_eq!(stream.get_result(), "");
    assert!(!stream.is_endpoint());
    let offline = crate::ffi::load_offline(Path::new(""), 1).unwrap();
    assert_eq!(offline.decode(&[0.0; INPUT_SAMPLES]).unwrap(), "");
}
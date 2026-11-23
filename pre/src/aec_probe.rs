//! Optional paired ALSA trace. No recording, clock queries, or cancellation on the normal path.

use alsa::pcm::{State, PCM};
use mozart_core::{RAW_SAMPLES, RAW_SAMPLE_RATE};
use rtrb::{Consumer, Producer, RingBuffer};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"MZAEC01\n";
const QUEUE_PACKETS: usize = 256;
const MAX_SAMPLES: u64 = 60 * RAW_SAMPLE_RATE as u64;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

struct Packet {
    sample: u64,
    epoch: u64,
    stamp_ns: u64,
    delay: i64,
    len: usize,
    pcm: [i16; RAW_SAMPLES],
}

pub struct Stream {
    queue: Producer<Packet>,
    lost_samples: Arc<AtomicU64>,
    sample: u64,
    epoch: u64,
}

impl Stream {
    pub fn discontinuity(&mut self) {
        self.epoch += 1;
    }

    pub fn observe(&mut self, pcm: &PCM, stereo: &[i16]) {
        if self.sample >= MAX_SAMPLES || self.queue.is_abandoned() {
            return;
        }
        // This opt-in measurement takes a synchronous ALSA snapshot on the
        // owning thread. Audio threads never wait on the trace queue or disk.
        let clock = pcm.status().ok().and_then(|status| {
            if status.get_state() != State::Running {
                return None;
            }
            let stamp = status.get_htstamp();
            let delay = status.get_delay();
            if stamp.tv_sec < 0 || !(0..1_000_000_000).contains(&stamp.tv_nsec) || delay < 0 {
                return None;
            }
            let ns = stamp.tv_sec as u64 * 1_000_000_000 + stamp.tv_nsec as u64;
            (ns != 0).then_some((ns, delay))
        });
        self.record(clock, stereo);
    }

    fn record(&mut self, clock: Option<(u64, i64)>, stereo: &[i16]) {
        let len = stereo.len() / 2;
        let (stamp_ns, delay) = clock.unwrap_or_default();
        let mut packet = Packet {
            sample: self.sample,
            epoch: self.epoch,
            stamp_ns,
            delay,
            len,
            pcm: [0; RAW_SAMPLES],
        };
        for (dst, pair) in packet.pcm[..len].iter_mut().zip(stereo.chunks_exact(2)) {
            *dst = pair[0];
        }
        self.sample += len as u64;
        if self.queue.push(packet).is_err() {
            self.lost_samples.fetch_add(len as u64, Ordering::Relaxed);
        }
    }
}

struct Sink {
    output: BufWriter<File>,
    queue: Consumer<Packet>,
    lost_samples: Arc<AtomicU64>,
    written: u64,
    invalid: u64,
}

fn pair(file: File) -> io::Result<(Stream, Sink)> {
    let (producer, consumer) = RingBuffer::new(QUEUE_PACKETS);
    let lost_samples = Arc::new(AtomicU64::new(0));
    let mut output = BufWriter::new(file);
    output.write_all(MAGIC)?;
    output.write_all(&RAW_SAMPLE_RATE.to_le_bytes())?;
    Ok((
        Stream {
            queue: producer,
            lost_samples: lost_samples.clone(),
            sample: 0,
            epoch: 0,
        },
        Sink {
            output,
            queue: consumer,
            lost_samples,
            written: 0,
            invalid: 0,
        },
    ))
}

impl Sink {
    fn drain(&mut self) -> io::Result<bool> {
        // Bound each turn so sustained capture traffic cannot starve playback.
        for _ in 0..QUEUE_PACKETS {
            let Ok(packet) = self.queue.pop() else { break };
            self.output.write_all(&packet.sample.to_le_bytes())?;
            self.output.write_all(&packet.epoch.to_le_bytes())?;
            self.output.write_all(&packet.stamp_ns.to_le_bytes())?;
            self.output.write_all(&packet.delay.to_le_bytes())?;
            self.output.write_all(&(packet.len as u32).to_le_bytes())?;
            for sample in &packet.pcm[..packet.len] {
                self.output.write_all(&sample.to_le_bytes())?;
            }
            self.written += packet.len as u64;
            if packet.stamp_ns == 0 {
                self.invalid += packet.len as u64;
            }
        }
        Ok(self.queue.is_abandoned() && self.queue.is_empty())
    }

    fn finish(&mut self) -> io::Result<[u64; 3]> {
        self.output.flush()?;
        Ok([
            self.written,
            self.invalid,
            self.lost_samples.load(Ordering::Relaxed),
        ])
    }
}

pub fn start(
    dir: &Path,
    report_error: impl FnOnce(&str) + Send + 'static,
) -> io::Result<(Stream, Stream, JoinHandle<io::Result<()>>)> {
    // An explicitly requested new, private directory: never overwrite previous recordings.
    DirBuilder::new().mode(0o700).create(dir)?;
    let create = |name: &str| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(name))
    };
    let (capture, mut capture_sink) = pair(create("capture.mzaec")?)?;
    let (playback, mut playback_sink) = pair(create("playback.mzaec")?)?;
    let mut summary = create("summary.partial")?;
    let dir = dir.to_owned();
    let writer = thread::Builder::new().name("mozart-aec-probe".into()).spawn(move || {
        let mut write = || -> io::Result<()> {
            loop {
                let capture_done = capture_sink.drain()?;
                let playback_done = playback_sink.drain()?;
                if capture_done && playback_done { break; }
                thread::sleep(Duration::from_millis(2));
            }
            let stats = |[written, invalid, dropped]: [u64; 3]| serde_json::json!({
                "samples_recorded": written,
                "invalid_clock_samples": invalid,
                "queue_dropped_samples": dropped,
                "reached_sample_limit": written + dropped >= MAX_SAMPLES,
            });
            serde_json::to_writer(&mut summary, &serde_json::json!({
                "capture": stats(capture_sink.finish()?), "playback": stats(playback_sink.finish()?),
            }))?;
            summary.flush()?;
            fs::rename(dir.join("summary.partial"), dir.join("summary.json"))
        };
        let result = write();
        if let Err(error) = &result {
            let message = format!("AEC probe writer: {error}");
            report_error(&message);
            eprintln!("mozart-pre: {message}");
        }
        result
    })?;
    Ok((capture, playback, writer))
}

pub fn finish(writer: JoinHandle<io::Result<()>>) -> Result<(), String> {
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while !writer.is_finished() {
        if Instant::now() >= deadline {
            return Err("AEC probe incomplete: writer did not finish before shutdown".into());
        }
        thread::sleep(Duration::from_millis(2));
    }
    writer
        .join()
        .map_err(|_| "AEC probe writer panicked".to_string())?
        .map_err(|error| format!("AEC probe incomplete: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn writes_transferred_left_samples_and_invalid_clock_and_epochs() {
        let dir = std::env::temp_dir().join(format!("mozart-aec-test-{}", std::process::id()));
        let (mut c, mut p, writer) = start(&dir, |_| {}).unwrap();
        assert!(start(&dir, |_| {}).is_err());
        c.record(Some((20_000, 91)), &[1, 200, -2, -200]);
        c.discontinuity();
        c.record(None, &[3, 400]);
        p.record(Some((40_000, 10)), &[-4, 700]);
        drop((c, p));
        finish(writer).unwrap();
        let bytes = fs::read(dir.join("capture.mzaec")).unwrap();
        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(&bytes[48..52], &[1, 0, 254, 255]);
        assert_eq!(&bytes[52..60], &2u64.to_le_bytes());
        assert_eq!(&bytes[60..68], &1u64.to_le_bytes());
        assert_eq!(&bytes[68..76], &0u64.to_le_bytes());
        let summary: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("summary.json")).unwrap()).unwrap();
        assert_eq!(summary["capture"]["samples_recorded"], 3);
        assert_eq!(summary["capture"]["invalid_clock_samples"], 1);
        assert_eq!(summary["playback"]["samples_recorded"], 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_writer_cannot_publish_completion_summary() {
        let dir = std::env::temp_dir().join(format!("mozart-aec-failed-{}", std::process::id()));
        let (report, errors) = std::sync::mpsc::channel();
        let (c, p, writer) = start(&dir, move |message| {
            report.send(message.to_owned()).unwrap()
        })
        .unwrap();
        fs::rename(dir.join("summary.partial"), dir.join("interrupted")).unwrap();
        drop((c, p));
        assert!(finish(writer).unwrap_err().contains("AEC probe incomplete"));
        assert!(errors.try_recv().unwrap().starts_with("AEC probe writer:"));
        assert!(!dir.join("summary.json").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn queue_overload_keeps_a_sample_index_gap() {
        let (producer, mut consumer) = RingBuffer::new(1);
        let mut stream = Stream {
            queue: producer,
            lost_samples: Arc::default(),
            sample: 0,
            epoch: 0,
        };
        stream.record(None, &[1, 10]);
        stream.record(None, &[2, 20]);
        assert_eq!(consumer.pop().unwrap().sample, 0);
        stream.record(None, &[3, 30]);
        assert_eq!(consumer.pop().unwrap().sample, 2);
        assert_eq!(stream.lost_samples.load(Ordering::Relaxed), 1);
    }
}

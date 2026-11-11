//! 进程内 SPSC 环形队列：采集 → DSP（SPEC-01）。
//! 满时丢最旧并计数；空时消费者等待，带超时返回供优雅退出。
//! push/pop 热路径只做索引更新与一次帧拷贝：不分配内存，锁不跨 I/O。

use std::mem::MaybeUninit;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// 固定容量环形队列，单个生产者与单个消费者。
/// 互斥锁只覆盖索引更新与一次帧拷贝；消费者等待期间锁已释放。
pub struct Ring<T: Copy> {
    queue: Mutex<Queue<T>>,
    /// 队列空时消费者在此等待 push 唤醒。
    ready: Condvar,
}

struct Queue<T> {
    /// 构造时一次性预分配；槽位按序号对容量取模复用。
    slots: Box<[MaybeUninit<T>]>,
    /// 单调写入序号。
    head: u64,
    /// 单调读取序号。
    tail: u64,
    /// 满时丢最旧的累计计数。
    dropped: u64,
}

impl<T: Copy> Ring<T> {
    /// 预分配 `capacity` 帧，容量至少 1。
    pub fn new(capacity: usize) -> Self {
        assert!(capacity >= 1, "ring capacity must be at least 1 frame");
        Self {
            queue: Mutex::new(Queue {
                slots: vec![MaybeUninit::uninit(); capacity].into_boxed_slice(),
                head: 0,
                tail: 0,
                dropped: 0,
            }),
            ready: Condvar::new(),
        }
    }

    /// 入队一帧。满时丢最旧并计数；新帧总是入队。
    pub fn push(&self, frame: T) {
        let mut queue = self.queue.lock().unwrap();
        let capacity = queue.slots.len() as u64;
        if queue.head - queue.tail == capacity {
            queue.tail += 1;
            queue.dropped += 1;
        }
        let index = (queue.head % capacity) as usize;
        queue.slots[index].write(frame);
        queue.head += 1;
        drop(queue);
        self.ready.notify_one();
    }

    /// 出队一帧。空时等待：`None` 阻塞到有帧，`Some(t)` 超时后返回 None。
    pub fn pop(&self, timeout: Option<Duration>) -> Option<T> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut queue = self.queue.lock().unwrap();
        loop {
            if queue.head > queue.tail {
                let capacity = queue.slots.len() as u64;
                // SAFETY: 序号落在 [tail, head) 内的槽位都经 push 写入过；
                // 每个槽位只被固定序号复用，不会读到未初始化数据。
                let index = (queue.tail % capacity) as usize;
                let frame = unsafe { queue.slots[index].assume_init() };
                queue.tail += 1;
                return Some(frame);
            }
            let wait = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            queue = match wait {
                None => self.ready.wait(queue).unwrap(),
                Some(wait) => {
                    let (guard, result) = self.ready.wait_timeout(queue, wait).unwrap();
                    if result.timed_out() && guard.head == guard.tail {
                        return None;
                    }
                    guard
                }
            };
        }
    }

    /// 当前深度（队列内帧数），供状态上报。
    pub fn depth(&self) -> usize {
        let queue = self.queue.lock().unwrap();
        (queue.head - queue.tail) as usize
    }

    /// 满时丢最旧的累计计数，供状态上报。
    pub fn dropped(&self) -> u64 {
        self.queue.lock().unwrap().dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_ring_drops_oldest_and_counts() {
        let ring = Ring::new(4);
        for value in 0u64..6 {
            ring.push(value);
        }
        assert_eq!(ring.dropped(), 2);
        assert_eq!(ring.depth(), 4);
        // 满时丢最旧：队列保留最新的 2、3、4、5。
        let timeout = Some(Duration::from_secs(1));
        assert_eq!(ring.pop(timeout), Some(2));
        assert_eq!(ring.pop(timeout), Some(3));
        assert_eq!(ring.pop(timeout), Some(4));
        assert_eq!(ring.pop(timeout), Some(5));
        assert_eq!(ring.depth(), 0);
        assert_eq!(ring.dropped(), 2);
        assert_eq!(ring.pop(Some(Duration::from_millis(20))), None);
    }

    #[test]
    fn pop_times_out_when_empty() {
        let ring: Ring<u64> = Ring::new(8);
        let started = Instant::now();
        assert_eq!(ring.pop(Some(Duration::from_millis(120))), None);
        // condvar 毫秒超时留 5 ms 容差（同 ctl 用例）。
        assert!(started.elapsed() >= Duration::from_millis(115));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn pop_blocks_until_push_wakes_it() {
        let ring = std::sync::Arc::new(Ring::new(8));
        let consumer = ring.clone();
        let worker = std::thread::spawn(move || consumer.pop(None));
        std::thread::sleep(Duration::from_millis(100));
        ring.push(42u64);
        let started = Instant::now();
        assert_eq!(worker.join().unwrap(), Some(42));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(ring.depth(), 0);
    }
}

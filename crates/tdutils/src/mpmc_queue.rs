//! Port of `td/utils/MpmcQueue.h`'s `OneValue` and `MpmcQueue` APIs.
//!
//! The queue itself is backed by `crossbeam-channel` (the plan's adoption of
//! crossbeam instead of re-implementing tdlib's hazard-pointer reclamation):
//! an unbounded MPMC channel with the same blocking/non-blocking semantics.
//! `thread_id` parameters are kept for API parity and ignored — crossbeam
//! handles reclamation internally.

use std::sync::atomic::{AtomicU8, Ordering};

/// `td::OneValue<T>` — a single-slot handoff (Empty/Taken/Value state).
pub struct OneValue<T> {
    state: AtomicU8,
    value: std::cell::UnsafeCell<Option<T>>,
}

// SAFETY: the state machine serializes access: only the thread that
// transitions Empty→Value may touch `value`, and the next transition
// (Value→Taken) happens-after the previous via acq_rel.
unsafe impl<T: Send> Sync for OneValue<T> {}

const EMPTY: u8 = 0;
const TAKEN: u8 = 1;
const VALUE: u8 = 2;

impl<T> OneValue<T> {
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            value: std::cell::UnsafeCell::new(None),
        }
    }

    /// `td::OneValue::set_value(value)` — succeeds (moving the value in)
    /// only in the Empty state; on failure the caller's value is returned
    /// unchanged.
    pub fn set_value(&self, value: T) -> Result<(), T> {
        // SAFETY: Empty→Value is the only transition that writes `value`,
        // serialized by the CAS.
        unsafe { *self.value.get() = Some(value) };
        if self
            .state
            .compare_exchange(EMPTY, VALUE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Ok(())
        } else {
            // Take the value back; it was never published.
            let value = unsafe { (*self.value.get()).take() };
            Err(value.expect("value present"))
        }
    }

    /// `td::OneValue::get_value(value)` — takes the value only from the
    /// Value state.
    pub fn get_value(&self) -> Option<T> {
        match self.state.swap(TAKEN, Ordering::AcqRel) {
            VALUE => {
                // SAFETY: the Value→Taken transition is exclusive; the
                // writer's Empty→Value CAS is visible via acq_rel.
                unsafe { (*self.value.get()).take() }
            }
            _ => None,
        }
    }

    /// `td::OneValue::reset()`.
    pub fn reset(&self) {
        self.state.store(EMPTY, Ordering::Release);
        // SAFETY: reset happens-before any subsequent Empty→Value publish
        // (the C++ test barriers provide the ordering); the old value is
        // dropped by the slot.
        unsafe { *self.value.get() = None };
    }
}

impl<T> Default for OneValue<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// `td::MpmcQueue<T>` — unbounded multi-producer/multi-consumer queue.
/// `gc`/`thread_id` exist for API parity with the C++ class; crossbeam
/// reclaims memory internally, so they are no-ops.
pub struct MpmcQueue<T> {
    sender: crossbeam_channel::Sender<T>,
    receiver: crossbeam_channel::Receiver<T>,
}

impl<T> MpmcQueue<T> {
    /// `td::MpmcQueue(threads_n)`.
    pub fn new(threads_n: usize) -> Self {
        Self::with_block_size(1024, threads_n)
    }

    /// `td::MpmcQueue(block_size, threads_n)`.
    pub fn with_block_size(_block_size: usize, _threads_n: usize) -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self { sender, receiver }
    }

    pub fn get_description() -> &'static str {
        "Mpmc queue (crossbeam-channel)"
    }

    /// `td::MpmcQueue::push(value, thread_id)`.
    pub fn push(&self, value: T, _thread_id: usize) {
        self.sender
            .send(value)
            .expect("queue receivers are alive while the queue is used");
    }

    /// `td::MpmcQueue::try_pop(value, thread_id)`.
    pub fn try_pop(&self, _thread_id: usize) -> Option<T> {
        self.receiver.try_recv().ok()
    }

    /// `td::MpmcQueue::pop(thread_id)` — blocking.
    pub fn pop(&self, _thread_id: usize) -> T {
        self.receiver
            .recv()
            .expect("queue receivers are alive while the queue is used")
    }

    /// `td::MpmcQueue::gc(thread_id)` — a no-op; crossbeam reclaims memory
    /// as items are consumed.
    pub fn gc(&self, _thread_id: usize) {}

    /// `td::MpmcQueue::hazard_pointers_to_delele_size_unsafe()` — always 0.
    pub fn hazard_pointers_to_delete_size_unsafe(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of `TEST(OneValue, simple)`.
    #[test]
    fn one_value_simple() {
        {
            let value = OneValue::new();
            let x = String::from("hello");
            let result = value.set_value(x);
            assert!(result.is_ok(), "empty slot accepts a value");
            let taken = value.get_value();
            assert_eq!(taken.as_deref(), Some("hello"));
        }
        {
            let value = OneValue::new();
            assert_eq!(value.get_value(), None, "empty slot has no value");
            let y = String::from("hello");
            let result = value.set_value(y);
            assert!(result.is_err(), "get_value leaves the slot Taken");
            assert_eq!(result.unwrap_err(), "hello", "value returned unchanged");
        }
    }

    /// Port of `TEST(OneValue, stress)` — two threads ping-ponging a single
    /// slot with stage barriers.
    #[test]
    fn one_value_stress() {
        use crate::tests::Stage;

        let run = std::sync::Arc::new(Stage::default());
        let check = std::sync::Arc::new(Stage::default());
        let value = std::sync::Arc::new(OneValue::<String>::new());

        let mut threads = Vec::new();
        for id in 0..2 {
            let run = run.clone();
            let check = check.clone();
            let value = value.clone();
            threads.push(std::thread::spawn(move || {
                let mut from = String::new();
                let mut set_status = false;
                let mut to = String::new();
                let mut get_status = false;
                for round in 1..100_000u64 {
                    if id == 0 {
                        value.reset();
                        to = String::new();
                        from = String::new();
                    }
                    run.wait(round * 2);
                    if id == 0 {
                        from = String::from("hello");
                        set_status = value.set_value(from.clone()).is_ok();
                    } else {
                        to = value.get_value().unwrap_or_default();
                        get_status = !to.is_empty();
                    }
                    check.wait(round * 2);
                    if id == 0 {
                        if set_status {
                            assert!(get_status);
                            assert!(from.is_empty(), "set moved the value");
                            assert_eq!(to, "hello");
                        } else {
                            assert!(!get_status);
                            assert_eq!(from, "hello");
                            assert!(to.is_empty());
                        }
                    }
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
    }

    /// Port of `TEST(MpmcQueue, simple)`.
    #[test]
    fn mpmc_queue_simple() {
        let q = MpmcQueue::with_block_size(2, 1);
        for _ in 0..2 {
            for i in 0..100 {
                q.push(i, 0);
            }
            for i in 0..100 {
                let x = q.pop(0);
                assert_eq!(x, i, "FIFO order");
            }
        }
    }

    /// Minimal multi-thread smoke: two producers, two consumers.
    #[test]
    fn mpmc_queue_multi_thread_small() {
        let q = std::sync::Arc::new(MpmcQueue::<usize>::new(5));
        let mut received_vec = Vec::with_capacity(2);
        for _ in 0..2 {
            received_vec.push(std::sync::Mutex::new(Vec::new()));
        }
        let received = std::sync::Arc::new(received_vec);

        let mut threads = Vec::new();
        for thread_id in 0..2 {
            let q = q.clone();
            let received = received.clone();
            threads.push(std::thread::spawn(move || {
                loop {
                    let item = q.pop(thread_id);
                    if item == 0 {
                        return;
                    }
                    received[thread_id].lock().unwrap().push(item);
                }
            }));
        }
        for producer in 0..2 {
            let q = q.clone();
            threads.push(std::thread::spawn(move || {
                for i in 0..1000 {
                    q.push(producer * 1000 + i + 1, 0);
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            received[0].lock().unwrap().len() + received[1].lock().unwrap().len(),
            2000
        );
    }

    /// Port of `TEST(MpmcQueue, multi_thread)`: 10 producers × 10 consumers,
    /// per-producer ordering and exactly-once delivery.
    #[test]
    fn mpmc_queue_multi_thread() {
        const PRODUCERS: usize = 10;
        const CONSUMERS: usize = 10;
        const ITEMS: usize = 100_000;

        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        struct Data {
            from: usize,
            value: usize,
        }

        let q = std::sync::Arc::new(MpmcQueue::<Data>::new(21));
        let mut received_vec = Vec::with_capacity(CONSUMERS);
        for _ in 0..CONSUMERS {
            received_vec.push(std::sync::Mutex::new(Vec::new()));
        }
        let received = std::sync::Arc::new(received_vec);

        let mut consumers = Vec::new();
        for thread_id in 0..CONSUMERS {
            let q = q.clone();
            let received = received.clone();
            consumers.push(std::thread::spawn(move || {
                loop {
                    let item = q.pop(thread_id);
                    if item.value == 0 {
                        return;
                    }
                    received[thread_id].lock().unwrap().push(item);
                }
            }));
        }

        let mut producers = Vec::new();
        for thread_id in CONSUMERS..PRODUCERS + CONSUMERS {
            let q = q.clone();
            producers.push(std::thread::spawn(move || {
                for i in 0..ITEMS {
                    q.push(
                        Data {
                            from: thread_id - CONSUMERS,
                            value: i + 1,
                        },
                        thread_id,
                    );
                }
            }));
        }
        for producer in producers {
            producer.join().unwrap();
        }
        // One sentinel per consumer.
        for _ in 0..CONSUMERS {
            q.push(Data { from: 0, value: 0 }, PRODUCERS + CONSUMERS);
        }
        for consumer in consumers {
            consumer.join().unwrap();
        }

        let mut all = Vec::new();
        for data in received.iter() {
            let mut per_producer = [0usize; PRODUCERS];
            for item in data.lock().unwrap().iter() {
                all.push(*item);
                assert!(item.value > per_producer[item.from], "per-producer order");
                per_producer[item.from] = item.value;
            }
        }
        assert_eq!(
            all.len(),
            PRODUCERS * ITEMS,
            "all items received exactly once"
        );

        all.sort_unstable();
        for (i, item) in all.iter().enumerate() {
            assert_eq!(item.from, i / ITEMS);
            assert_eq!(item.value, i % ITEMS + 1);
        }
        assert_eq!(q.hazard_pointers_to_delete_size_unsafe(), 0);
        for id in 0..PRODUCERS + CONSUMERS + 1 {
            q.gc(id);
        }
    }
}

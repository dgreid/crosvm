// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A set of futures polled by one task without allocating per future.

use std::future::poll_fn;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Wake;
use std::task::Waker;

use futures::task::AtomicWaker;

/// Slots per chunk, one bit of a `u64` each.
const CHUNK: usize = u64::BITS as usize;

/// How many times `poll` looks again for futures woken while it was polling. Stops a future that
/// always wakes itself from keeping the parent task from its other work.
const MAX_SWEEPS: u32 = 8;

/// Wake state shared by the slots of a chunk.
struct ChunkWake {
    /// Bit `n` is set when slot `n` has been woken.
    ready: AtomicU64,
    parent: Arc<AtomicWaker>,
}

/// Waker for one slot. It's created with the chunk and reused by every future in the slot.
struct SlotWaker {
    chunk: Arc<ChunkWake>,
    bit: u64,
}

impl Wake for SlotWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.chunk.ready.fetch_or(self.bit, Ordering::Release);
        self.chunk.parent.wake();
    }
}

struct Chunk<F> {
    /// Boxed so adding chunks doesn't move futures that have been pinned.
    futures: Box<[Option<F>]>,
    wakers: Box<[Waker]>,
    wake: Arc<ChunkWake>,
}

impl<F> Chunk<F> {
    fn new(parent: Arc<AtomicWaker>) -> Self {
        let wake = Arc::new(ChunkWake {
            ready: AtomicU64::new(0),
            parent,
        });
        let wakers = (0..CHUNK)
            .map(|slot| {
                Waker::from(Arc::new(SlotWaker {
                    chunk: wake.clone(),
                    bit: 1 << slot,
                }))
            })
            .collect();
        let futures = (0..CHUNK).map(|_| None).collect();
        Chunk {
            futures,
            wakers,
            wake,
        }
    }
}

/// A set of futures polled by the task that owns it, like `FuturesUnordered`.
///
/// `FuturesUnordered` allocates a task for every future added to it. `FutureSlab` keeps futures in
/// slots that are allocated a chunk at a time and reused, so it stops allocating once it has
/// enough slots for the futures in flight.
pub struct FutureSlab<F> {
    chunks: Vec<Chunk<F>>,
    /// Unoccupied slots, used as a stack.
    free: Vec<usize>,
    parent: Arc<AtomicWaker>,
}

impl<F> FutureSlab<F> {
    pub fn new() -> Self {
        FutureSlab {
            chunks: Vec::new(),
            free: Vec::new(),
            parent: Arc::new(AtomicWaker::new()),
        }
    }

    /// Returns the number of futures that haven't completed.
    pub fn len(&self) -> usize {
        self.chunks.len() * CHUNK - self.free.len()
    }

    /// Returns true if every future has completed.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<F: Future<Output = ()>> FutureSlab<F> {
    /// Adds `f` to the set. It's first polled by the next call to `poll`.
    pub fn push(&mut self, f: F) {
        let slot = self.free.pop().unwrap_or_else(|| self.grow());
        self.chunks[slot / CHUNK].futures[slot % CHUNK] = Some(f);
        self.set_ready(slot);
    }

    /// Polls the futures that were woken or added since the last call.
    ///
    /// Call this from the owning task's poll. `cx` is woken when any of the futures are woken.
    pub fn poll(&mut self, cx: &mut Context) {
        self.parent.register(cx.waker());

        // Futures can be woken from other threads while this runs, so keep going until nothing is
        // ready. A wake after the last check isn't lost, it wakes `cx` through `parent`.
        for _ in 0..MAX_SWEEPS {
            let mut progressed = false;
            for chunk in 0..self.chunks.len() {
                let mut ready = self.chunks[chunk].wake.ready.swap(0, Ordering::Acquire);
                while ready != 0 {
                    let slot = chunk * CHUNK + ready.trailing_zeros() as usize;
                    ready &= ready - 1;
                    progressed = true;
                    self.poll_slot(slot);
                }
            }
            if !progressed {
                return;
            }
        }
        // Futures are still ready. Return so the owner can do its other work, but have it poll
        // again.
        cx.waker().wake_by_ref();
    }

    /// Polls the futures until they have all completed.
    pub async fn drain(&mut self) {
        poll_fn(|cx| {
            self.poll(cx);
            if self.is_empty() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }

    fn poll_slot(&mut self, slot: usize) {
        let chunk = &mut self.chunks[slot / CHUNK];
        let index = slot % CHUNK;
        let Some(future) = chunk.futures[index].as_mut() else {
            return;
        };
        // SAFETY: `futures` is a boxed slice that is never reallocated, and a future only leaves
        // its slot by being dropped, so it doesn't move while it's alive.
        let future = unsafe { Pin::new_unchecked(future) };
        if future
            .poll(&mut Context::from_waker(&chunk.wakers[index]))
            .is_pending()
        {
            return;
        }
        chunk.futures[index] = None;
        self.free.push(slot);
    }

    /// Adds a chunk and returns one of its slots.
    fn grow(&mut self) -> usize {
        let base = self.chunks.len() * CHUNK;
        self.chunks.push(Chunk::new(self.parent.clone()));
        // Reversed so the new slots are handed out in order.
        self.free.extend((base + 1..base + CHUNK).rev());
        base
    }

    fn set_ready(&self, slot: usize) {
        self.chunks[slot / CHUNK]
            .wake
            .ready
            .fetch_or(1 << (slot % CHUNK), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use futures::channel::oneshot;

    use super::*;

    fn run<F: Future<Output = ()>>(slab: &mut FutureSlab<F>) {
        cros_async::block_on(slab.drain());
    }

    #[test]
    fn runs_ready_futures() {
        let count = Rc::new(Cell::new(0));
        let mut slab = FutureSlab::new();
        for _ in 0..200 {
            let count = count.clone();
            slab.push(async move {
                count.set(count.get() + 1);
            });
        }
        assert_eq!(slab.len(), 200);
        run(&mut slab);
        assert_eq!(count.get(), 200);
        assert!(slab.is_empty());
    }

    #[test]
    fn wakes_parent_when_a_future_completes() {
        let (tx, rx) = oneshot::channel::<()>();
        let done = Rc::new(Cell::new(false));
        let mut slab = FutureSlab::new();
        {
            let done = done.clone();
            slab.push(async move {
                let _ = rx.await;
                done.set(true);
            });
        }

        cros_async::block_on(async {
            // The future is waiting on `rx`, so it should still be in the set.
            poll_fn(|cx| {
                slab.poll(cx);
                Poll::Ready(())
            })
            .await;
            assert_eq!(slab.len(), 1);
            assert!(!done.get());

            tx.send(()).unwrap();
            slab.drain().await;
        });
        assert!(done.get());
    }

    #[test]
    fn reuses_freed_slots() {
        let mut slab = FutureSlab::new();
        for _ in 0..CHUNK {
            slab.push(std::future::ready(()));
        }
        run(&mut slab);
        assert_eq!(slab.chunks.len(), 1);

        // All the slots are free again, so another chunk's worth shouldn't need a new chunk.
        for _ in 0..CHUNK {
            slab.push(std::future::ready(()));
        }
        assert_eq!(slab.chunks.len(), 1);
        run(&mut slab);
    }

    #[test]
    fn bounds_a_self_waking_future() {
        struct SelfWake(u32);
        impl Future for SelfWake {
            type Output = ();
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
                self.0 += 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }

        let mut slab = FutureSlab::new();
        slab.push(SelfWake(0));
        let woken = Arc::new(CountingWaker(AtomicU64::new(0)));
        let waker = Waker::from(woken.clone());
        slab.poll(&mut Context::from_waker(&waker));

        // `poll` should give up after `MAX_SWEEPS` and leave the caller woken.
        let polls = slab.chunks[0].futures[0].as_ref().unwrap().0;
        assert_eq!(polls, MAX_SWEEPS);
        assert!(woken.0.load(Ordering::Relaxed) >= 1);
    }

    struct CountingWaker(AtomicU64);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref()
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn grows_past_one_chunk() {
        let mut slab = FutureSlab::new();
        let (txs, rxs): (Vec<_>, Vec<_>) =
            (0..CHUNK * 3 + 1).map(|_| oneshot::channel::<()>()).unzip();
        for rx in rxs {
            slab.push(async move {
                let _ = rx.await;
            });
        }
        assert_eq!(slab.len(), CHUNK * 3 + 1);
        assert_eq!(slab.chunks.len(), 4);
        for tx in txs {
            tx.send(()).unwrap();
        }
        run(&mut slab);
        assert!(slab.is_empty());
    }
}

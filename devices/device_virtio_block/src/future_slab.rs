// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A set of futures driven by one parent task, without a per-future allocation.

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

/// Slots per chunk. One `u64` of ready bits covers exactly one chunk.
const CHUNK: usize = u64::BITS as usize;

/// How many times [`FutureSlab::poll`] re-sweeps for futures woken during the sweep before
/// handing control back to its caller. A future that wakes itself every time it is polled
/// would otherwise keep the owning task from ever reaching its other work.
const MAX_SWEEPS: u32 = 8;

/// Wake state for one chunk, shared with the wakers of the slots in it.
struct ChunkWake {
    /// Bit `n` is set when slot `n` of this chunk has been woken.
    ready: AtomicU64,
    parent: Arc<AtomicWaker>,
}

/// The waker for a single slot. One is built per slot when its chunk is created and then
/// reused by every future that occupies the slot, so running a future costs no allocation.
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
    /// Boxed so that growing the slab never moves a future that is already being polled.
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

/// A set of `()`-producing futures polled together by whichever task owns the slab.
///
/// This covers the same ground as `FuturesUnordered` for callers that only need their futures
/// run to completion, but `FuturesUnordered` allocates an `Arc<Task<_>>` per future and links
/// it into an intrusive list. At the hundreds of thousands of requests per second a block
/// device sustains, that allocation and its list and refcount traffic are a measurable share
/// of the device's CPU time. Here the futures live in slots that are allocated a chunk at a
/// time and reused, and each slot's waker is built once and shared by every future that
/// occupies it, so steady-state operation does not allocate at all.
///
/// Slots are stored in boxed chunks rather than one flat `Vec` because adding capacity must
/// not move futures that have already been polled.
pub struct FutureSlab<F> {
    chunks: Vec<Chunk<F>>,
    /// Indices of the unoccupied slots, most recently freed first.
    free: Vec<usize>,
    parent: Arc<AtomicWaker>,
    len: usize,
}

impl<F> Default for FutureSlab<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> FutureSlab<F> {
    pub fn new() -> Self {
        FutureSlab {
            chunks: Vec::new(),
            free: Vec::new(),
            parent: Arc::new(AtomicWaker::new()),
            len: 0,
        }
    }

    /// True once every future added to the set has completed.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<F: Future<Output = ()>> FutureSlab<F> {
    /// Adds `f` to the set. It is first polled by the next call to [`FutureSlab::poll`].
    pub fn push(&mut self, f: F) {
        let slot = self.free.pop().unwrap_or_else(|| self.grow());
        self.chunks[slot / CHUNK].futures[slot % CHUNK] = Some(f);
        self.len += 1;
        self.set_ready(slot);
    }

    /// Polls every future woken since the last call, and the futures added since it as well.
    ///
    /// Never completes: the futures produce no output, so there is nothing to return. Call it
    /// from the parent task's own poll so that the parent is rescheduled whenever one of the
    /// futures is woken.
    pub fn poll(&mut self, cx: &mut Context) {
        self.parent.register(cx.waker());

        // A waker may fire on another thread while this sweep is in progress, so keep
        // sweeping until a full pass finds nothing ready. A wake that lands after the last
        // pass is not lost: it was preceded by the `register` above, so it reschedules the
        // parent instead.
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
        // Still ready after `MAX_SWEEPS`. Yield, but ask to be polled again right away so the
        // remaining futures are not left waiting on an unrelated wakeup.
        cx.waker().wake_by_ref();
    }

    /// Polls the futures already in the set until all of them have completed.
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
        // SAFETY: `futures` is a boxed slice that is never reallocated, and an occupied slot is
        // only ever cleared by dropping the future where it sits, so a future stored here never
        // moves for as long as it is alive.
        let future = unsafe { Pin::new_unchecked(future) };
        if future
            .poll(&mut Context::from_waker(&chunk.wakers[index]))
            .is_pending()
        {
            return;
        }
        chunk.futures[index] = None;
        self.free.push(slot);
        self.len -= 1;
    }

    /// Appends a chunk and returns the index of one of its slots.
    fn grow(&mut self) -> usize {
        let base = self.chunks.len() * CHUNK;
        self.chunks.push(Chunk::new(self.parent.clone()));
        // Reverse order so that `free.pop()` keeps handing out the lowest slot available,
        // which keeps the occupied slots packed into the chunks swept first.
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
        futures::executor::block_on(slab.drain());
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
        assert_eq!(slab.len, 200);
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

        futures::executor::block_on(async {
            // The future parks on `rx`, so the slab must report it as still outstanding.
            poll_fn(|cx| {
                slab.poll(cx);
                Poll::Ready(())
            })
            .await;
            assert_eq!(slab.len, 1);
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

        // The slab held `CHUNK` futures at once and they have all finished, so taking on
        // another `CHUNK` must not need a second chunk.
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

        // The future never finishes, so `poll` has to give up rather than spin, and it has to
        // leave the caller scheduled to come back.
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
        assert_eq!(slab.len, CHUNK * 3 + 1);
        assert_eq!(slab.chunks.len(), 4);
        for tx in txs {
            tx.send(()).unwrap();
        }
        run(&mut slab);
        assert!(slab.is_empty());
    }
}

// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A fixed set of slots for futures polled by one task, without allocating per future.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Wake;
use std::task::Waker;

use futures::stream::FusedStream;
use futures::task::AtomicWaker;
use futures::Stream;

/// Slots per word of the ready bitmap.
const WORD: usize = u64::BITS as usize;

/// How many times a sweep looks again for futures woken while it was polling. Stops a future that
/// always wakes itself from keeping the parent task from its other work.
const MAX_SWEEPS: u32 = 8;

/// Wake state shared by all the slots.
struct SlabWake {
    /// Bit `n % 64` of word `n / 64` is set when slot `n` has been woken.
    ready: Box<[AtomicU64]>,
    parent: AtomicWaker,
}

impl SlabWake {
    fn set_ready(&self, slot: usize, order: Ordering) {
        self.ready[slot / WORD].fetch_or(1 << (slot % WORD), order);
    }
}

/// Waker for one slot. It's created with the slab and reused by every future in the slot.
struct SlotWaker {
    wake: Arc<SlabWake>,
    slot: usize,
}

impl Wake for SlotWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wake.set_ready(self.slot, Ordering::Release);
        self.wake.parent.wake();
    }
}

/// A set of up to a fixed number of futures, polled by the task that owns it like
/// `FuturesUnordered`.
///
/// `FuturesUnordered` allocates a task for every future added to it. `FutureSlab` allocates all
/// of its slots and their wakers when it's created and reuses them, so adding a future doesn't
/// allocate.
///
/// As a `Stream`, each item is the number of futures that completed in one sweep over the futures
/// that were woken or added since the last one. The stream never ends: it stays pending while
/// nothing completes, including when the set is empty.
pub struct FutureSlab<F> {
    /// Never resized, so futures that have been pinned don't move.
    futures: Box<[Option<F>]>,
    wakers: Box<[Waker]>,
    wake: Arc<SlabWake>,
    /// Unoccupied slots, used as a stack.
    free: Vec<usize>,
}

impl<F> FutureSlab<F> {
    /// Returns a set with room for `capacity` futures.
    pub fn with_capacity(capacity: usize) -> Self {
        let wake = Arc::new(SlabWake {
            ready: (0..capacity.div_ceil(WORD))
                .map(|_| AtomicU64::new(0))
                .collect(),
            parent: AtomicWaker::new(),
        });
        let wakers = (0..capacity)
            .map(|slot| {
                Waker::from(Arc::new(SlotWaker {
                    wake: wake.clone(),
                    slot,
                }))
            })
            .collect();
        FutureSlab {
            futures: (0..capacity).map(|_| None).collect(),
            wakers,
            wake,
            // Reversed so the slots are handed out in order.
            free: (0..capacity).rev().collect(),
        }
    }

    /// Returns the number of futures that haven't completed.
    pub fn len(&self) -> usize {
        self.futures.len() - self.free.len()
    }

    /// Returns true if every future has completed.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns true if there's no room for another future.
    pub fn is_full(&self) -> bool {
        self.free.is_empty()
    }
}

impl<F: Future<Output = ()>> FutureSlab<F> {
    /// Adds `f` to the set. It's first polled by the next sweep.
    ///
    /// # Panics
    ///
    /// Panics if the set is full.
    pub fn push(&mut self, f: F) {
        let slot = self.free.pop().expect("FutureSlab is full");
        self.futures[slot] = Some(f);
        self.wake.set_ready(slot, Ordering::Relaxed);
    }

    /// Polls the futures that were woken or added since the last sweep. `cx` is woken when any of
    /// the futures are woken.
    fn sweep(&mut self, cx: &mut Context) {
        self.wake.parent.register(cx.waker());

        // Futures can be woken from other threads while this runs, so keep going until nothing is
        // ready. A wake after the last check isn't lost, it wakes `cx` through `parent`.
        for _ in 0..MAX_SWEEPS {
            let mut progressed = false;
            for word in 0..self.wake.ready.len() {
                let mut ready = self.wake.ready[word].swap(0, Ordering::Acquire);
                while ready != 0 {
                    let slot = word * WORD + ready.trailing_zeros() as usize;
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

    fn poll_slot(&mut self, slot: usize) {
        let Some(future) = self.futures[slot].as_mut() else {
            return;
        };
        // SAFETY: `futures` is a boxed slice that is never resized, and a future only leaves its
        // slot by being dropped, so it doesn't move while it's alive.
        let future = unsafe { Pin::new_unchecked(future) };
        if future
            .poll(&mut Context::from_waker(&self.wakers[slot]))
            .is_pending()
        {
            return;
        }
        self.futures[slot] = None;
        self.free.push(slot);
    }
}

impl<F: Future<Output = ()>> Stream for FutureSlab<F> {
    type Item = usize;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<usize>> {
        let slab = self.get_mut();
        let before = slab.len();
        slab.sweep(cx);
        match before - slab.len() {
            0 => Poll::Pending,
            completed => Poll::Ready(Some(completed)),
        }
    }
}

impl<F: Future<Output = ()>> FusedStream for FutureSlab<F> {
    fn is_terminated(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::future::poll_fn;
    use std::rc::Rc;

    use futures::channel::oneshot;
    use futures::StreamExt;

    use super::*;

    fn run<F: Future<Output = ()>>(slab: &mut FutureSlab<F>) {
        cros_async::block_on(async {
            while !slab.is_empty() {
                slab.next().await;
            }
        });
    }

    #[test]
    fn runs_ready_futures() {
        let count = Rc::new(Cell::new(0));
        let mut slab = FutureSlab::with_capacity(200);
        for _ in 0..200 {
            let count = count.clone();
            slab.push(async move {
                count.set(count.get() + 1);
            });
        }
        assert_eq!(slab.len(), 200);
        assert!(slab.is_full());
        run(&mut slab);
        assert_eq!(count.get(), 200);
        assert!(slab.is_empty());
    }

    #[test]
    fn wakes_parent_when_a_future_completes() {
        let (tx, rx) = oneshot::channel::<()>();
        let done = Rc::new(Cell::new(false));
        let mut slab = FutureSlab::with_capacity(1);
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
                assert!(slab.poll_next_unpin(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert_eq!(slab.len(), 1);
            assert!(!done.get());

            tx.send(()).unwrap();
            assert_eq!(slab.next().await, Some(1));
        });
        assert!(done.get());
    }

    #[test]
    fn reuses_freed_slots() {
        let mut slab = FutureSlab::with_capacity(WORD);
        for _ in 0..WORD {
            slab.push(std::future::ready(()));
        }
        assert!(slab.is_full());
        run(&mut slab);

        // All the slots are free again, so there's room for as many again.
        for _ in 0..WORD {
            slab.push(std::future::ready(()));
        }
        assert!(slab.is_full());
        run(&mut slab);
        assert!(slab.is_empty());
    }

    #[test]
    #[should_panic(expected = "FutureSlab is full")]
    fn push_past_capacity_panics() {
        let mut slab = FutureSlab::with_capacity(1);
        slab.push(std::future::pending::<()>());
        slab.push(std::future::pending::<()>());
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

        let mut slab = FutureSlab::with_capacity(1);
        slab.push(SelfWake(0));
        let woken = Arc::new(CountingWaker(AtomicU64::new(0)));
        let waker = Waker::from(woken.clone());
        assert!(slab
            .poll_next_unpin(&mut Context::from_waker(&waker))
            .is_pending());

        // The sweep should give up after `MAX_SWEEPS` and leave the caller woken.
        let polls = slab.futures[0].as_ref().unwrap().0;
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
    fn spans_several_ready_words() {
        let mut slab = FutureSlab::with_capacity(WORD * 3 + 1);
        let (txs, rxs): (Vec<_>, Vec<_>) =
            (0..WORD * 3 + 1).map(|_| oneshot::channel::<()>()).unzip();
        for rx in rxs {
            slab.push(async move {
                let _ = rx.await;
            });
        }
        assert!(slab.is_full());
        for tx in txs {
            tx.send(()).unwrap();
        }
        run(&mut slab);
        assert!(slab.is_empty());
    }
}

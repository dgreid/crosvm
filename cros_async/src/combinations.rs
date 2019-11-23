// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Need non-snake case so the macro can re-use type names for variables.
#![allow(non_snake_case)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Context;
use std::task::Waker;

use futures::future::{maybe_done, MaybeDone};

use crate::executor::{FutureList, UnitFutures};
use crate::waker::create_waker;

// Macro-generate future combinators to allow for running different numbers of top-level futures in
// this FutureList.
macro_rules! generate {
    ($(
        $(#[$doc:meta])*
        ($Complete:ident, <$($Fut:ident),*>),
    )*) => ($(
        $(#[$doc])*
        #[must_use = "Combinations of futures don't do anything unless run in an executor."]
        paste::item! {
            pub(crate) struct $Complete<$($Fut: Future),*> {
                added_futures: UnitFutures,
                $($Fut: MaybeDone<$Fut>,)*
                $([<$Fut _ready>]: AtomicBool,)*
            }
        }

        impl<$($Fut: Future),*> $Complete<$($Fut),*> {
            paste::item! {
                pub(crate) fn new($($Fut: $Fut),*) -> $Complete<$($Fut),*> {
                    $Complete {
                        added_futures: UnitFutures::new(),
                        $($Fut: maybe_done($Fut),)*
                        $([<$Fut _ready>]: AtomicBool::new(true),)*
                    }
                }
            }
        }

        impl<$($Fut: Future),*> FutureList for $Complete<$($Fut),*> {
            type Output = ($($Fut::Output),*);

            fn futures_mut(&mut self) -> &mut UnitFutures {
                &mut self.added_futures
            }

            paste::item! {
                fn poll_results(&mut self) -> Option<Self::Output> {
                    let _ = self.added_futures.poll_results();

                    let mut complete = true;
                    $(
                        let $Fut = unsafe {
                            // Safe because no future will be moved before the structure is dropped
                            // and no future can run after the structure is dropped.
                            Pin::new_unchecked(&mut self.$Fut)
                        };
                        if self.[<$Fut _ready>].swap(false, Ordering::Relaxed) {
                            let waker = unsafe {
                                let raw_waker =
                                    create_waker(&self.[<$Fut _ready>] as *const _ as *const _);
                                Waker::from_raw(raw_waker)
                            };
                            let mut ctx = Context::from_waker(&waker);
                            complete &= $Fut.poll(&mut ctx).is_ready();
                        }
                    )*

                    if complete {
                        $(
                            let $Fut = unsafe {
                                // Safe because no future will be moved before the structure is
                                // dropped and no future can run after the structure is dropped.
                                Pin::new_unchecked(&mut self.$Fut)
                            };
                        )*
                        Some(($($Fut.take_output().unwrap()), *))
                    } else {
                        None
                    }
                }

                fn any_ready(&self) -> bool {
                    let mut ready = self.added_futures.any_ready();
                    $(
                        ready |= self.[<$Fut _ready>].load(Ordering::Relaxed);
                    )*
                    ready
                }
            }
        }
    )*)
}

generate! {
    /// Future for the [`complete2`] function.
    (Complete2, <Fut1, Fut2>),

    /// Future for the [`complete3`] function.
    (Complete3, <Fut1, Fut2, Fut3>),

    /// Future for the [`complete4`] function.
    (Complete4, <Fut1, Fut2, Fut3, Fut4>),

    /// Future for the [`complete5`] function.
    (Complete5, <Fut1, Fut2, Fut3, Fut4, Fut5>),
}

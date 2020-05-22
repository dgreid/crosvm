// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::io_source::IoSource;
use crate::uring_executor::Result;
use crate::uring_fut::UringFutState;
use crate::uring_mem::{BackingMemory, MemVec};

/// Future for the `read_to_mem` function.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct ReadMem<'a, W: IoSource + ?Sized> {
    reader: &'a W,
    state: UringFutState<(u64, Rc<dyn BackingMemory>, &'a [MemVec]), Rc<dyn BackingMemory>>,
}

impl<R: IoSource + ?Sized + Unpin> Unpin for ReadMem<'_, R> {}

impl<'a, R: IoSource + ?Sized + Unpin> ReadMem<'a, R> {
    pub(crate) fn new(
        reader: &'a R,
        file_offset: u64,
        mem: Rc<dyn BackingMemory>,
        mem_offsets: &'a [MemVec],
    ) -> Self {
        ReadMem {
            reader,
            state: UringFutState::new((file_offset, mem, mem_offsets)),
        }
    }
}

impl<R: IoSource + ?Sized + Unpin> Future for ReadMem<'_, R> {
    type Output = Result<u32>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let state = std::mem::replace(&mut self.state, UringFutState::Processing);
        let (new_state, ret) = match state.advance(
            |(file_offset, mem, mem_offsets)| {
                Ok((
                    Pin::new(&self.reader).read_to_mem(
                        file_offset,
                        Rc::clone(&mem),
                        mem_offsets,
                    )?,
                    mem,
                ))
            },
            |op| Pin::new(&self.reader).poll_complete(cx, op),
        ) {
            Ok(d) => d,
            Err(e) => return Poll::Ready(Err(e)),
        };

        self.state = new_state;

        match ret {
            Poll::Pending => Poll::Pending,
            Poll::Ready((r, _)) => Poll::Ready(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::rc::Rc;

    use futures::pin_mut;

    use crate::io_ext::IoSourceExt;
    use crate::uring_mem::{MemVec, VecIoWrapper};

    #[test]
    fn readmem() {
        async fn go() {
            let f = File::open("/dev/zero").unwrap();
            let source = crate::io_source::AsyncSource::new(f).unwrap();
            let v = vec![0x55u8; 64];
            let vw = Rc::new(VecIoWrapper::from(v));
            let ret = source
                .read_to_mem(
                    0,
                    Rc::<VecIoWrapper>::clone(&vw),
                    &[MemVec { offset: 0, len: 32 }],
                )
                .await
                .unwrap();
            assert_eq!(32, ret);
            let v: Vec<u8> = Rc::try_unwrap(vw).unwrap().into();
            assert!(v.iter().take(32).all(|&b| b == 0));
        }

        let fut = go();
        pin_mut!(fut);
        crate::run_one(fut).unwrap();
    }
}

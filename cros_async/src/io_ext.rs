// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Extension functions to asynchronously access files.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::io_source::IoSource;
use crate::uring_executor::{MemVec, PendingOperation, Result};
use crate::uring_mem::VecIoWrapper;

/// Extends IoSource with ergonomic methods to perform asynchronous IO.
pub trait IoSourceExt: IoSource {
    fn read_to_vec<'a>(&'a self, file_offset: u64, vec: Vec<u8>) -> ReadVec<'a, Self>
    where
        Self: Unpin,
    {
        ReadVec::new(self, file_offset, vec)
    }
}

impl<T: IoSource + ?Sized> IoSourceExt for T {}

#[derive(Debug)]
enum UringFutState<T, W> {
    Init(T),
    Wait((PendingOperation, W)),
    Done,
    Processing,
}

impl<T, W> UringFutState<T, W> {
    fn advance<S, P>(self, start: S, poll: P) -> Result<(Self, Poll<(Result<u32>, W)>)>
    where
        S: FnOnce(T) -> Result<(PendingOperation, W)>,
        P: FnOnce(&mut PendingOperation) -> Poll<Result<u32>>,
    {
        use UringFutState::*;
        // First advance from init if that's the current state.
        let (mut op, wait_data) = match self {
            Init(init_data) => start(init_data)?,
            Wait(op_wait) => op_wait,
            Done | Processing => unreachable!("Invalid future state"),
        };

        match poll(&mut op) {
            Poll::Pending => Ok((Wait((op, wait_data)), Poll::Pending)),
            Poll::Ready(res) => Ok((Done, Poll::Ready((res, wait_data)))),
        }
    }
}

/// Future for the `read_to_vectored` function.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct ReadVec<'a, R: IoSource + ?Sized> {
    reader: &'a R,
    state: UringFutState<(u64, Rc<VecIoWrapper>), Rc<VecIoWrapper>>,
}

impl<R: IoSource + ?Sized + Unpin> Unpin for ReadVec<'_, R> {}

impl<'a, R: IoSource + ?Sized + Unpin> ReadVec<'a, R> {
    pub(super) fn new(reader: &'a R, file_offset: u64, vec: Vec<u8>) -> Self {
        ReadVec {
            reader,
            state: UringFutState::Init((file_offset, Rc::new(VecIoWrapper::from(vec)))),
        }
    }
}

impl<R: IoSource + ?Sized + Unpin> Future for ReadVec<'_, R> {
    type Output = Result<Vec<u8>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let state = std::mem::replace(&mut self.state, UringFutState::Processing);
        let (new_state, ret) = match state.advance(
            |(file_offset, wrapped_vec)| {
                Ok((
                    Pin::new(&self.reader).read_to_mem(
                        file_offset,
                        Rc::<VecIoWrapper>::clone(&wrapped_vec),
                        &[MemVec {
                            offset: 0,
                            len: wrapped_vec.len(),
                        }],
                    )?,
                    wrapped_vec,
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
            Poll::Ready((r, wrapped_vec)) => match r {
                Ok(_) => Poll::Ready(Ok(Rc::try_unwrap(wrapped_vec)
                    .expect("too many refs on vec")
                    .into())),
                Err(e) => Poll::Ready(Err(e)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::pin_mut;
    use std::fs::File;

    use crate::io_ext::IoSourceExt;

    #[test]
    fn readvec() {
        async fn go() {
            let f = File::open("/dev/zero").unwrap();
            let source = crate::io_source::AsyncSource::new(f).unwrap();
            let v = vec![0x55u8; 32];
            let v_ptr = v.as_ptr();
            let ret_v = source.read_to_vec(0, v).await.unwrap();
            assert_eq!(v_ptr, ret_v.as_ptr());
            assert!(ret_v.iter().all(|&b| b == 0));
        }

        let fut = go();
        pin_mut!(fut);
        crate::run_one(fut).unwrap();
    }
}

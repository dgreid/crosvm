// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::future::Future;
use std::io;
use std::os::unix::io::AsRawFd;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use futures::future;

use data_model::VolatileMemory;

use crate::uring_executor;
use crate::WakerToken;

pub struct MemVec {
    pub offset: u64,
    pub len: usize,
}

enum IoOperation<'a, T: AsRawFd, M: VolatileMemory> {
    ReadVectored {
        fd: &'a T,
        file_offset: u64,
        mem: Rc<M>,
        iovecs: &'a [MemVec],
    },
}

impl<'a, T: AsRawFd, M: VolatileMemory> IoOperation<'a, T, M> {
    fn submit(self, waker: Waker) -> PendingOperation<'a, T> {
        match self {
            IoOperation::ReadVectored {
                fd,
                file_offset,
                mem,
                iovecs,
            } => {
                let waker_token =
                    uring_executor::submit_readv(fd.as_raw_fd(), file_offset, mem, iovecs, waker)
                        .unwrap();
                PendingOperation { waker_token, fd }
            }
        }
    }
}

struct PendingOperation<'a, T: AsRawFd> {
    waker_token: WakerToken,
    fd: &'a T,
}

enum State<'a, T: AsRawFd, M: VolatileMemory> {
    Init(IoOperation<'a, T, M>),
    Pending(PendingOperation<'a, T>),
    Done,
}

pub struct URingOp<'a, T: AsRawFd, M: VolatileMemory> {
    state: State<'a, T, M>,
}

impl<'a, T: AsRawFd, M: VolatileMemory> URingOp<'a, T, M> {
    fn poll_state(&mut self, cx: &mut Context) -> Poll<io::Result<u32>> {
        let mut res = None;
        self.state = match std::mem::replace(&mut self.state, State::Done) {
            State::Init(op) => {
                //submit op,
                let pending_op = op.submit(cx.waker().clone());
                State::Pending(pending_op)
            }
            State::Pending(pending_op) => {
                if let Some(result) = uring_executor::get_result(&pending_op.waker_token) {
                    res = Some(result);
                    State::Done
                } else {
                    State::Pending(pending_op)
                }
            }
            State::Done => unreachable!("re-polled after complete"),
        };
        if let Some(result) = res {
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}

struct AsyncShareadReadVectored<T: AsRawFd, M: VolatileMemory> {
    io_source: T,
    mem_target: Rc<M>,
}

impl<T: AsRawFd, M: VolatileMemory> AsyncShareadReadVectored<T, M> {
    pub fn new(io_source: T, mem_target: M) -> AsyncShareadReadVectored<T, M> {
        AsyncShareadReadVectored {
            io_source,
            mem_target: Rc::new(mem_target),
        }
    }
    // TODO Should this use the op and callback strategy from smol as well?
    // have a `with` function that can do the polling and manage the state.
}

pub trait AsyncVolatileRead<M: VolatileMemory, T: AsRawFd> {
    /// `file_offset` is the offset in the reading source `self`, most commonly a backing file.
    /// Pass Rc<M> so that the lifetime of the memory can be ensured to live longer than the
    /// asynchronous op. A ref is taken by the executor and not released until the op is completed,
    /// uring ops aren't cancelled synchronously, requiring the ref to be held beyond the scope of
    /// the returned future.
    /// Note that the `iovec`s are be relative to the start of `memory`.
    fn read_to_vectored<'a>(
        &'a mut self,
        file_offset: u64,
        memory: Rc<M>,
        iovecs: &'a [MemVec],
        cx: &mut Context,
    ) -> URingOp<'a, T, M>;
}

impl<M, T> AsyncVolatileRead<M, T> for T
where
    M: VolatileMemory,
    T: AsRawFd,
{
    fn read_to_vectored<'a>(
        &'a mut self,
        file_offset: u64,
        mem: Rc<M>,
        iovecs: &'a [MemVec],
        cx: &mut Context,
    ) -> URingOp<'a, T, M> {
        URingOp {
            state: State::Init(IoOperation::ReadVectored {
                fd: self,
                file_offset,
                mem,
                iovecs,
            }),
        }
    }
}

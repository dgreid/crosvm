// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ops::Deref;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::uring_executor::{
    self, BackingMemory, MemVec, PendingOperation, RegisteredSource, Result,
};

pub trait IoSource {
    fn read_to_mem(
        self: Pin<&Self>,
        file_offset: u64,
        mem: Rc<dyn BackingMemory>,
        mem_offsets: &[MemVec],
    ) -> Result<PendingOperation>;

    // read_to<T> where T:BackingMemory

    fn poll_complete(
        self: Pin<&Self>,
        cx: &mut Context,
        token: &mut PendingOperation,
    ) -> Poll<Result<u32>>;
}

macro_rules! deref_io_source {
    () => {
        fn read_to_mem(
            self: Pin<&Self>,
            file_offset: u64,
            mem: Rc<dyn BackingMemory>,
            mem_offsets: &[MemVec],
        ) -> Result<PendingOperation> {
            Pin::new(&**self).read_to_mem(file_offset, mem, mem_offsets)
        }

        fn poll_complete(
            self: Pin<&Self>,
            cx: &mut Context,
            token: &mut PendingOperation,
        ) -> Poll<Result<u32>> {
            Pin::new(&**self).poll_complete(cx, token)
        }
    };
}

impl<T: ?Sized + IoSource + Unpin> IoSource for Box<T> {
    deref_io_source!();
}

impl<T: ?Sized + IoSource + Unpin> IoSource for &T {
    deref_io_source!();
}

impl<T: ?Sized + IoSource + Unpin> IoSource for &mut T {
    deref_io_source!();
}

impl<P> IoSource for Pin<P>
where
    P: Deref + Unpin,
    P::Target: IoSource,
{
    fn read_to_mem(
        self: Pin<&Self>,
        file_offset: u64,
        mem: Rc<dyn BackingMemory>,
        mem_offsets: &[MemVec],
    ) -> Result<PendingOperation> {
        self.get_ref()
            .as_ref()
            .read_to_mem(file_offset, mem, mem_offsets)
    }

    fn poll_complete(
        self: Pin<&Self>,
        cx: &mut Context,
        token: &mut PendingOperation,
    ) -> Poll<Result<u32>> {
        self.get_ref().as_ref().poll_complete(cx, token)
    }
}

pub struct AsyncSource<F: AsRawFd> {
    registered_source: RegisteredSource,
    source: F,
}

impl<F: AsRawFd> AsyncSource<F> {
    pub fn new(io_source: F) -> Result<AsyncSource<F>> {
        let r = uring_executor::register_source(&io_source)?;
        Ok(AsyncSource {
            registered_source: r,
            source: io_source,
        })
    }

    pub fn to_source(self) -> F {
        self.source
    }
}

impl<F: AsRawFd> IoSource for AsyncSource<F> {
    fn read_to_mem(
        self: Pin<&Self>,
        file_offset: u64,
        mem: Rc<dyn BackingMemory>,
        mem_offsets: &[MemVec],
    ) -> Result<PendingOperation> {
        self.registered_source
            .start_read_to_mem(file_offset, mem, mem_offsets)
    }

    // read_to<T> where T:BackingMemory

    fn poll_complete(
        self: Pin<&Self>,
        cx: &mut Context,
        token: &mut PendingOperation,
    ) -> Poll<Result<u32>> {
        self.registered_source.poll_complete(cx, token)
    }
}

#[cfg(test)]
mod tests {
    use futures::pin_mut;
    use std::future::Future;

    use super::*;

    enum MemTestState<'a> {
        Init {
            file_offset: u64,
            mem: Rc<dyn BackingMemory>,
            mem_offsets: &'a [MemVec],
        },
        Wait {
            token: PendingOperation,
        },
        Idle,
    }

    pub struct TestRead<'a, F: AsRawFd + Unpin> {
        inner: &'a AsyncSource<F>,
        state: MemTestState<'a>,
    }

    impl<'a, F: AsRawFd + Unpin> TestRead<'a, F> {
        fn new(
            io: &'a AsyncSource<F>,
            file_offset: u64,
            mem: Rc<dyn BackingMemory>,
            mem_offsets: &'a [MemVec],
        ) -> Self {
            TestRead {
                inner: io,
                state: MemTestState::Init {
                    file_offset,
                    mem,
                    mem_offsets,
                },
            }
        }
    }

    impl<'a, F: AsRawFd + Unpin> Future for TestRead<'a, F> {
        type Output = Result<u32>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            use MemTestState::*;

            let mut state = match std::mem::replace(&mut self.state, Idle) {
                Init {
                    file_offset,
                    mem,
                    mem_offsets,
                } => match Pin::new(&self.inner).read_to_mem(file_offset, mem, mem_offsets) {
                    Ok(token) => Wait { token },
                    Err(e) => return Poll::Ready(Err(e)),
                },
                Idle => Idle,
                Wait { token } => Wait { token },
            };

            let ret = match &mut state {
                Wait { token } => Pin::new(&self.inner).poll_complete(cx, token),
                _ => panic!("Invalid state in future"),
            };
            self.state = state;
            ret
        }
    }

    #[test]
    fn read_to_mem() {
        use data_model::{GetVolatileRef, VolatileMemory};
        use sys_util::{GuestAddress, GuestMemory};

        let io_obj = Box::pin({
            // Use guest memory as a test file, it implements AsRawFd.
            let source = GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap();
            source.get_slice(0, 8192).unwrap().write_bytes(0x55);
            AsyncSource::new(source).unwrap()
        });

        // Start with memory filled with 0x44s.
        let buf = Rc::new(GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap());
        buf.get_slice(0, 8192).unwrap().write_bytes(0x44);

        let fut = TestRead::new(
            &io_obj,
            0,
            buf.clone(),
            &[MemVec {
                offset: 0,
                len: 8192,
            }],
        );
        pin_mut!(fut);
        crate::run_one(fut).unwrap();
        for i in 0..8192 {
            assert_eq!(buf.get_ref::<u8>(i).unwrap().load(), 0x55);
        }
    }
}

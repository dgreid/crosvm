// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::io_source::IoSource;
use crate::uring_executor::{BackingMemory, MemVec, PendingOperation, Result};
use crate::uring_mem::VecIoWrapper;

pub trait IoSourceExt: IoSource {
    fn read_to_vec<'a>(&'a self, file_offset: u64, vec: Vec<u8>) -> ReadVec<'a, Self>
    where
        Self: Unpin,
    {
        ReadVec::new(self, file_offset, vec)
    }
}

impl<T: IoSource + ?Sized> IoSourceExt for T {}

/// Future for the `read_to_vectored` function.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct ReadVec<'a, R: IoSource + ?Sized> {
    reader: &'a R,
    file_offset: u64,
    vec: Option<Rc<VecIoWrapper>>,
    token: Option<PendingOperation>,
}

impl<R: IoSource + ?Sized + Unpin> Unpin for ReadVec<'_, R> {}

impl<'a, R: IoSource + ?Sized + Unpin> ReadVec<'a, R> {
    pub(super) fn new(reader: &'a R, file_offset: u64, vec: Vec<u8>) -> Self {
        ReadVec {
            reader,
            file_offset,
            vec: Some(Rc::new(VecIoWrapper::from(vec))),
            token: None,
        }
    }
}

impl<R: IoSource + ?Sized + Unpin> Future for ReadVec<'_, R> {
    type Output = Result<Vec<u8>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut token = match std::mem::replace(&mut self.token, None) {
            None => {
                match Pin::new(&self.reader).read_to_mem(
                    self.file_offset,
                    Rc::<VecIoWrapper>::clone(self.vec.as_ref().unwrap()),
                    &[MemVec {
                        offset: 0,
                        len: self.vec.as_ref().unwrap().len(),
                    }],
                ) {
                    Ok(t) => Some(t),
                    Err(e) => return Poll::Ready(Err(e)),
                }
            }
            Some(t) => Some(t),
        };

        let ret = Pin::new(&self.reader).poll_complete(cx, token.as_mut().unwrap());
        self.token = token;
        match ret {
            Poll::Pending => Poll::Pending,
            Poll::Ready(res) => match res {
                Ok(_) => {
                    let vec_wrapper =
                        Rc::try_unwrap(self.vec.take().unwrap()).expect("bad rc on outer");
                    Poll::Ready(Ok(vec_wrapper.to_inner().expect("bad rc on the vec")))
                }
                Err(e) => Poll::Ready(Err(e)),
            },
        }
    }
}

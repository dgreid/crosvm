// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use futures::Stream;
use std::convert::TryFrom;
use std::marker::PhantomData;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use libc::{EWOULDBLOCK, O_NONBLOCK};

use msg_socket::{MsgError, MsgOnSocket, MsgReceiver as SysMsgReceiver, MsgSocket};
use sys_util::{self, add_fd_flags, Result};

use cros_async::add_read_waker;

/// Asynchronous version of `sys_util::MsgReceiver`. Provides an implementation of
/// `futures::Stream` so that messages can be consumed in an async context.
pub struct MsgReceiver<O: MsgOnSocket, T: SysMsgReceiver<O>>(T, PhantomData<O>);

impl<O: MsgOnSocket, T: SysMsgReceiver<O> + AsRawFd> MsgReceiver<O, T> {
    pub fn into_inner(self) -> T {
        self.0
    }

    pub fn from_receiver(sock: T) -> Result<MsgReceiver<O, T>> {
        let fd = sock.as_raw_fd();
        add_fd_flags(fd, O_NONBLOCK)?;
        Ok(MsgReceiver(sock, Default::default()))
    }
}

impl<O: MsgOnSocket, T: SysMsgReceiver<O> + AsRawFd> Stream for MsgReceiver<O, T> {
    type Item = O;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match self.0.recv() {
            Ok(msg) => Poll::Ready(Some(msg)),
            Err(MsgError::Recv(e)) => {
                if e.errno() == EWOULDBLOCK {
                    add_read_waker(&self.0, cx.waker().clone());
                    Poll::Pending
                } else {
                    // Indicate something went wrong and no more events will be provided.
                    Poll::Ready(None)
                }
            }
            Err(_) => {
                // Indicate something went wrong and no more events will be provided.
                Poll::Ready(None)
            }
        }
    }
}

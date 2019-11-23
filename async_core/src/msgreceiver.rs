// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use futures::Stream;
use std::convert::TryFrom;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use libc::{EWOULDBLOCK, O_NONBLOCK};

use msg_socket::{MsgError, MsgOnSocket, MsgReceiver as SysMsgReceiver, MsgSocket};
use sys_util::{self, add_fd_flags, Result};

use cros_async::add_read_waker;

/// Asynchronous version of `sys_util::MsgReceiver`. Provides an implementation of `futures::Stream` so that
/// messages can be consumed in an async context.
pub struct MsgReceiver<I: MsgOnSocket, O: MsgOnSocket>(MsgSocket<I, O>);

impl<I: MsgOnSocket, O: MsgOnSocket> MsgReceiver<I, O> {
    pub fn into_inner(self) -> MsgSocket<I, O> {
        self.0
    }
}

impl<I: MsgOnSocket, O: MsgOnSocket> TryFrom<MsgSocket<I, O>> for MsgReceiver<I, O> {
    type Error = sys_util::Error;

    fn try_from(sock: MsgSocket<I, O>) -> Result<MsgReceiver<I, O>> {
        let fd = sock.as_raw_fd();
        add_fd_flags(fd, O_NONBLOCK)?;
        Ok(MsgReceiver(sock))
    }
}

impl<I: MsgOnSocket, O: MsgOnSocket> Stream for MsgReceiver<I, O> {
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

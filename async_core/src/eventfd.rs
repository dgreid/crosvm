// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use futures::Stream;
use std::convert::TryFrom;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use libc::{EWOULDBLOCK, O_NONBLOCK};

use sys_util::{self, add_fd_flags, Result};

use cros_async::add_read_waker;

/// Asynchronous version of `sys_util::EventFd`. Provides an implementation of `futures::Stream` so that
/// events can be consumed in an async context.
///
/// # Example
///
/// ```
/// use std::convert::TryInto;
///
/// use async_core::EventFd;
/// use futures::StreamExt;
/// use sys_util::{self, Result};
/// async fn process_events() -> Result<()> {
///     let mut async_events: EventFd = sys_util::EventFd::new()?.try_into()?;
///     while let Some(e) = async_events.next().await {
///         // Handle event here.
///     }
///     Ok(())
/// }
/// ```
pub struct EventFd(sys_util::EventFd);

impl EventFd {
    pub fn new() -> Result<EventFd> {
        Self::try_from(sys_util::EventFd::new()?)
    }
}

impl TryFrom<sys_util::EventFd> for EventFd {
    type Error = sys_util::Error;

    fn try_from(eventfd: sys_util::EventFd) -> Result<EventFd> {
        let fd = eventfd.as_raw_fd();
        add_fd_flags(fd, O_NONBLOCK)?;
        Ok(EventFd(eventfd))
    }
}

impl Stream for EventFd {
    type Item = u64;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match self.0.read() {
            Ok(v) => Poll::Ready(Some(v)),
            Err(e) => {
                if e.errno() == EWOULDBLOCK {
                    add_read_waker(&self.0, cx.waker().clone());
                    return Poll::Pending;
                } else {
                    // Indicate something went wrong and no more events will be provided.
                    return Poll::Ready(None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cros_async::{select2, SelectResult};
    use futures::future::pending;
    use futures::stream::StreamExt;

    #[test]
    fn eventfd_write_read() {
        let evt = EventFd::new().unwrap();
        async fn read_one(mut evt: EventFd) -> u64 {
            if let Some(e) = evt.next().await {
                e
            } else {
                66
            }
        }
        async fn write_pend(evt: sys_util::EventFd) {
            evt.write(55).unwrap();
            let () = pending().await;
        }
        let write_evt = evt.0.try_clone().unwrap();

        if let (SelectResult::Finished(read_res), SelectResult::Pending(_pend_fut)) =
            select2(read_one(evt), write_pend(write_evt))
        {
            assert_eq!(read_res, 55);
        } else {
            panic!("wrong futures returned from select2");
        }
    }
}

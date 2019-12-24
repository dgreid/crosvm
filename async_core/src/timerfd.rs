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

/// Asynchronous version of `sys_util::TimerFd`. Provides an implementation of `futures::Stream` so
/// that timer events can be consumed in an async context.
///
/// # Example
///
/// ```
/// use std::convert::TryInto;
///
/// use async_core::TimerFd;
/// use futures::StreamExt;
/// use sys_util::{self, Result};
/// async fn process_events() -> Result<()> {
///     let mut async_timer: TimerFd = sys_util::TimerFd::new()?.try_into()?;
///     while let Some(e) = async_timer.next().await {
///         // Handle timer here.
///     }
///     Ok(())
/// }
/// ```
pub struct TimerFd(sys_util::TimerFd);

impl TryFrom<sys_util::TimerFd> for TimerFd {
    type Error = sys_util::Error;

    fn try_from(timerfd: sys_util::TimerFd) -> Result<TimerFd> {
        let fd = timerfd.as_raw_fd();
        add_fd_flags(fd, O_NONBLOCK)?;
        Ok(TimerFd(timerfd))
    }
}

impl Stream for TimerFd {
    type Item = u64;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match self.0.wait() {
            Ok(count) => Poll::Ready(Some(count)),
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
    use std::time::Duration;
    use sys_util;

    #[test]
    fn eventfd_write_read() {
        async fn timeout(duration: Duration) -> Option<u64> {
            let mut t = sys_util::TimerFd::new().unwrap();
            t.reset(duration, None).unwrap();
            let mut async_timer = TimerFd::try_from(t).unwrap();
            async_timer.next().await
        }

        if let (SelectResult::Finished(timer_count), SelectResult::Pending(_pend_fut)) =
            select2(timeout(Duration::from_millis(10)), pending::<()>())
        {
            assert_eq!(timer_count, Some(1));
        } else {
            panic!("wrong futures returned from select2");
        }
    }
}

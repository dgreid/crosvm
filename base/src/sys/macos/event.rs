// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS Event implementation using a pipe.
//!
//! On macOS, we use a self-pipe pattern instead of kqueue because:
//! 1. Kqueue file descriptors cannot be passed via SCM_RIGHTS (sendmsg)
//! 2. Pipes work reliably with SCM_RIGHTS for IPC
//! 3. The self-pipe pattern is a proven approach for cross-process events

use std::io;
use std::os::unix::io::RawFd;
use std::time::Duration;

use libc::c_int;
use libc::c_void;
use serde::Deserialize;
use serde::Serialize;

use crate::errno::errno_result;
use crate::errno::Result;
use crate::event::EventWaitResult;
use crate::sys::unix::RawDescriptor;
use crate::AsRawDescriptor;
use crate::FromRawDescriptor;
use crate::IntoRawDescriptor;
use crate::SafeDescriptor;

/// An Event backed by a pipe for cross-process signaling.
///
/// Unlike kqueue-based events, pipe-based events can be passed between processes
/// using SCM_RIGHTS (sendmsg).
#[derive(Debug, Serialize, Deserialize)]
pub struct PlatformEvent {
    /// The read end of the pipe - used for waiting
    #[serde(with = "crate::with_as_descriptor")]
    read_fd: SafeDescriptor,
    /// The write end of the pipe - used for signaling
    #[serde(with = "crate::with_as_descriptor")]
    write_fd: SafeDescriptor,
}

impl PlatformEvent {
    /// Create a new Event using a pipe.
    pub fn new() -> Result<PlatformEvent> {
        let mut fds: [RawFd; 2] = [0, 0];

        // SAFETY: fds is a valid, aligned array of two i32s on the stack.
        let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if ret < 0 {
            return errno_result();
        }

        // Set both ends to close-on-exec
        for &fd in &fds {
            // SAFETY: fd was returned by pipe() above and has not been closed.
            let ret = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            if ret < 0 {
                // Close both fds on error
                // SAFETY: fds[0] and fds[1] are valid fds from pipe() above.
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return errno_result();
            }
        }

        // Set both ends to non-blocking for proper timeout support
        for &fd in &fds {
            // SAFETY: fd was returned by pipe() above and has not been closed.
            let ret = unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags < 0 {
                    return errno_result();
                }
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK)
            };
            if ret < 0 {
                // SAFETY: fds[0] and fds[1] are valid fds from pipe() above.
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return errno_result();
            }
        }

        // SAFETY: fds[0] and fds[1] are valid fds from pipe() and we transfer ownership here.
        Ok(PlatformEvent {
            read_fd: unsafe { SafeDescriptor::from_raw_descriptor(fds[0]) },
            write_fd: unsafe { SafeDescriptor::from_raw_descriptor(fds[1]) },
        })
    }

    /// Signal the event by writing to the pipe.
    pub fn signal(&self) -> Result<()> {
        let buf: [u8; 1] = [1];

        // SAFETY: write_fd is a valid owned fd and buf is a valid 1-byte stack buffer.
        let ret = unsafe {
            libc::write(
                self.write_fd.as_raw_descriptor(),
                buf.as_ptr() as *const c_void,
                1,
            )
        };

        // EAGAIN/EWOULDBLOCK means the pipe is full (already signaled), which is fine
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::WouldBlock {
                return Err(crate::Error::new(err.raw_os_error().unwrap_or(0)));
            }
        }

        Ok(())
    }

    /// Wait for the event to be signaled.
    pub fn wait(&self) -> Result<()> {
        self.wait_timeout_impl(None)?;
        Ok(())
    }

    /// Wait for the event with a timeout.
    pub fn wait_timeout(&self, timeout: Duration) -> Result<EventWaitResult> {
        self.wait_timeout_impl(Some(timeout))
    }

    fn wait_timeout_impl(&self, timeout: Option<Duration>) -> Result<EventWaitResult> {
        let fd = self.read_fd.as_raw_descriptor();

        loop {
            // Use poll to wait for data with optional timeout
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            let timeout_ms = match timeout {
                Some(t) => t.as_millis().min(c_int::MAX as u128) as c_int,
                None => -1, // Infinite timeout
            };

            // SAFETY: pollfd is a valid stack-allocated struct and nfds is 1.
            let ret = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };

            if ret < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(crate::Error::new(err.raw_os_error().unwrap_or(0)));
            }

            if ret == 0 {
                return Ok(EventWaitResult::TimedOut);
            }

            // Data available, drain it
            let mut buf: [u8; 64] = [0; 64];

            // SAFETY: fd is the valid read end of our pipe and buf is a stack-allocated array.
            let read_ret = unsafe {
                libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len())
            };

            if read_ret < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    // Spurious wakeup, no data
                    if timeout.is_some() {
                        return Ok(EventWaitResult::TimedOut);
                    }
                    continue;
                }
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(crate::Error::new(err.raw_os_error().unwrap_or(0)));
            }

            return Ok(EventWaitResult::Signaled);
        }
    }

    /// Reset the event (drain any pending signals).
    pub fn reset(&self) -> Result<()> {
        let mut buf: [u8; 64] = [0; 64];
        let fd = self.read_fd.as_raw_descriptor();

        loop {
            // SAFETY: fd is the valid read end of our pipe and buf is a stack-allocated array.
            let ret = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };

            if ret < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    // No more data to drain
                    break;
                }
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(crate::Error::new(err.raw_os_error().unwrap_or(0)));
            }

            if ret == 0 {
                // EOF - pipe closed
                break;
            }
        }

        Ok(())
    }

    /// Clone the event.
    pub fn try_clone(&self) -> Result<PlatformEvent> {
        Ok(PlatformEvent {
            read_fd: self.read_fd.try_clone()?,
            write_fd: self.write_fd.try_clone()?,
        })
    }
}

impl PartialEq for PlatformEvent {
    fn eq(&self, other: &Self) -> bool {
        self.read_fd.as_raw_descriptor() == other.read_fd.as_raw_descriptor()
            && self.write_fd.as_raw_descriptor() == other.write_fd.as_raw_descriptor()
    }
}

impl Eq for PlatformEvent {}

impl AsRawDescriptor for PlatformEvent {
    /// Returns the read end of the pipe for use with poll/select.
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.read_fd.as_raw_descriptor()
    }
}

impl FromRawDescriptor for PlatformEvent {
    /// Constructs from a raw descriptor.
    ///
    /// WARNING: This only sets the read_fd. The write_fd will be invalid (-1).
    /// This is only useful for receiving an Event that was sent via sendmsg.
    /// After receiving, the Event can only be used for waiting, not signaling.
    unsafe fn from_raw_descriptor(descriptor: RawDescriptor) -> Self {
        PlatformEvent {
            read_fd: SafeDescriptor::from_raw_descriptor(descriptor),
            // write_fd is set to -1 since we only have the read end. close(-1) on drop
            // will return EBADF which is harmless, but we accept this rather than adding
            // Option<> complexity to the common path.
            write_fd: SafeDescriptor::from_raw_descriptor(-1),
        }
    }
}

impl IntoRawDescriptor for PlatformEvent {
    fn into_raw_descriptor(self) -> RawDescriptor {
        // Note: this leaks the write_fd
        self.read_fd.into_raw_descriptor()
    }
}

impl From<PlatformEvent> for SafeDescriptor {
    fn from(evt: PlatformEvent) -> Self {
        evt.read_fd
    }
}

impl From<SafeDescriptor> for PlatformEvent {
    fn from(read_fd: SafeDescriptor) -> Self {
        PlatformEvent {
            read_fd,
            // write_fd is set to -1 since we only have the read end. close(-1) on drop
            // will return EBADF which is harmless, but we accept this rather than adding
            // Option<> complexity to the common path.
            // SAFETY: -1 is not a valid fd, but SafeDescriptor will harmlessly fail on close.
            write_fd: unsafe { SafeDescriptor::from_raw_descriptor(-1) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn test_signal_and_wait() {
        let event = PlatformEvent::new().unwrap();
        event.signal().unwrap();
        event.wait().unwrap();
    }

    #[test]
    fn test_wait_timeout_signaled() {
        let event = PlatformEvent::new().unwrap();
        event.signal().unwrap();
        let result = event.wait_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(result, EventWaitResult::Signaled);
    }

    #[test]
    fn test_wait_timeout_not_signaled() {
        let event = PlatformEvent::new().unwrap();
        let start = Instant::now();
        let result = event.wait_timeout(Duration::from_millis(50)).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(result, EventWaitResult::TimedOut);
        assert!(elapsed >= Duration::from_millis(50));
        assert!(elapsed < Duration::from_millis(200));
    }

    #[test]
    fn test_reset() {
        let event = PlatformEvent::new().unwrap();
        event.signal().unwrap();
        event.signal().unwrap();
        event.signal().unwrap();
        event.reset().unwrap();
        let result = event.wait_timeout(Duration::from_millis(10)).unwrap();
        assert_eq!(result, EventWaitResult::TimedOut);
    }

    #[test]
    fn test_try_clone() {
        let event1 = PlatformEvent::new().unwrap();
        let event2 = event1.try_clone().unwrap();

        event1.signal().unwrap();
        let result = event2.wait_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(result, EventWaitResult::Signaled);
    }

    #[test]
    fn test_cross_thread() {
        let event = PlatformEvent::new().unwrap();
        let event_clone = event.try_clone().unwrap();

        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            event_clone.signal().unwrap();
        });

        let result = event.wait_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(result, EventWaitResult::Signaled);

        handle.join().unwrap();
    }
}

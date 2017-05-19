// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::os::unix::io::RawFd;

use libc;

use {Result, errno_result};

/// Trait for file descriptors that can be polled for input.
pub trait Pollable {
    /// Gets the file descriptor that can be polled for input.
    fn pollable_fd(&self) -> RawFd;
}

/// Waits for any of the given slice of Pollable objects to be readable without blocking and returns
/// the index of each that is readable.
pub fn poll(fds: &[&Pollable]) -> Result<Vec<usize>> {
    let mut pollfds = Vec::new();
    for fd in fds {
        pollfds.push(libc::pollfd {
            fd: fd.pollable_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
    // Safe because the pollfds pointer will only be read up to the length we give it, which is
    // exactly how large the vector is. We also check the return result.
    let ret = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, -1) };
    if ret < 0 {
        return errno_result()
    }

    let mut out = Vec::new();
    for (i, pollfd) in pollfds.iter().enumerate() {
        if (pollfd.revents & libc::POLLIN) != 0 {
            out.push(i);
        }
    }
    Ok(out)
}


#[cfg(test)]
mod tests {
    use super::*;
    use ::EventFd;

    #[test]
    fn evt_poll() {
        let evt = EventFd::new().unwrap();
        evt.write(1).unwrap();
        assert_eq!(poll(&[&evt]), Ok(vec![0]));
        assert_eq!(evt.read(), Ok((1)));
    }

    #[test]
    fn multi_evt_poll() {
        let evt1 = EventFd::new().unwrap();
        let evt2 = EventFd::new().unwrap();
        evt1.write(1).unwrap();
        assert_eq!(poll(&[&evt1, &evt2]), Ok(vec![0]));
        evt2.write(1).unwrap();
        assert_eq!(poll(&[&evt1, &evt2]), Ok(vec![0, 1]));
        evt1.read().unwrap();
        assert_eq!(poll(&[&evt1, &evt2]), Ok(vec![1]));
    }
}

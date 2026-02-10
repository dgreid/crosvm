// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::os::unix::io::FromRawFd;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::pin::Pin;

use anyhow::anyhow;
use base::safe_descriptor_from_cmdline_fd;
use base::AsRawDescriptor;
use base::RawDescriptor;
use cros_async::Executor;
use futures::Future;
use futures::FutureExt;
use vmm_vhost::BackendServer;
use vmm_vhost::Connection;

use crate::virtio::vhost_user_backend::connection::VhostUserConnectionTrait;
use crate::virtio::vhost_user_backend::handler::sys::macos::run_handler;

/// Connection from connected socket
pub struct VhostUserStream(UnixStream);

/// Check if a file descriptor is a socket using fstat.
fn fd_is_socket(fd: RawFd) -> bool {
    // SAFETY: stat is a zeroed struct on the stack with only primitive fields. fd is
    // provided by the caller; fstat will return -1 if it is invalid.
    unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut stat) == 0 {
            // S_IFSOCK indicates socket type
            (stat.st_mode & libc::S_IFMT) == libc::S_IFSOCK
        } else {
            false
        }
    }
}

impl VhostUserStream {
    /// Creates a new vhost-user stream from an existing connected socket file descriptor.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The provided file descriptor is not a socket.
    /// - An error occurs while creating the underlying stream.
    pub fn new_socket_from_fd(socket_fd: RawDescriptor) -> anyhow::Result<Self> {
        if !fd_is_socket(socket_fd) {
            return Err(anyhow!("fd {} is not a socket", socket_fd));
        }

        let safe_fd = safe_descriptor_from_cmdline_fd(&socket_fd)?;

        let stream = UnixStream::from(safe_fd);

        Ok(VhostUserStream(stream))
    }
}

impl VhostUserConnectionTrait for VhostUserStream {
    fn run_req_handler<'e>(
        self,
        handler: Box<dyn vmm_vhost::Backend>,
        ex: &'e Executor,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'e>> {
        async { stream_run_with_handler(self.0, handler, ex).await }.boxed_local()
    }
}

impl AsRawDescriptor for VhostUserStream {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.0.as_raw_descriptor()
    }
}

async fn stream_run_with_handler(
    stream: UnixStream,
    handler: Box<dyn vmm_vhost::Backend>,
    ex: &Executor,
) -> anyhow::Result<()> {
    let req_handler = BackendServer::new(Connection::try_from(stream)?, handler);
    run_handler(req_handler, ex).await
}

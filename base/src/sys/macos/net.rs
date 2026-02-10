// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS-specific network utilities.
//!
//! This module provides UnixSeqpacket which emulates SOCK_SEQPACKET semantics using
//! SOCK_STREAM with length-prefixed messages, since macOS doesn't support SOCK_SEQPACKET
//! for AF_UNIX sockets.

use std::cmp::Ordering;
use std::convert::TryFrom;
use std::ffi::OsString;
use std::fs::remove_file;
use std::io;
use std::mem;
use std::mem::size_of;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixDatagram;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::time::Duration;
use std::time::Instant;

use libc::c_int;
use libc::c_void;
use libc::close;
use libc::fcntl;
use libc::in6_addr;
use libc::in_addr;
use libc::sa_family_t;
use libc::setsockopt;
use libc::sockaddr_in;
use libc::sockaddr_in6;
use libc::socklen_t;
use libc::AF_INET;
use libc::AF_INET6;
use libc::FD_CLOEXEC;
use libc::F_SETFD;
use libc::SOCK_STREAM;
use libc::SOL_SOCKET;
use libc::SO_NOSIGPIPE;
use log::warn;
use serde::Deserialize;
use serde::Serialize;

use crate::AsRawDescriptor;
use crate::descriptor::FromRawDescriptor;
use crate::descriptor::IntoRawDescriptor;
use crate::Error;
use crate::RawDescriptor;
use crate::SafeDescriptor;
use crate::ScmSocket;

/// Length of the message length prefix in bytes
const LENGTH_PREFIX_SIZE: usize = 4;

fn socket(
    domain: c_int,
    sock_type: c_int,
    protocol: c_int,
) -> io::Result<SafeDescriptor> {
    // SAFETY:
    // Socket initialization doesn't touch memory.
    match unsafe { libc::socket(domain, sock_type, protocol) } {
        -1 => Err(io::Error::last_os_error()),
        // SAFETY:
        // Safe because we own the file descriptor.
        fd => Ok(unsafe { SafeDescriptor::from_raw_descriptor(fd) }),
    }
}

pub(in crate::sys) fn socketpair(
    domain: c_int,
    sock_type: c_int,
    protocol: c_int,
) -> io::Result<(SafeDescriptor, SafeDescriptor)> {
    let mut fds = [0, 0];
    // SAFETY: fds is a valid 2-element i32 array on the stack.
    match unsafe { libc::socketpair(domain, sock_type, protocol, fds.as_mut_ptr()) } {
        -1 => Err(io::Error::last_os_error()),
        _ => Ok(
            // SAFETY:
            // Safe because we own the file descriptors and 'move' them to SafeDescriptor.
            unsafe {
                (
                    SafeDescriptor::from_raw_descriptor(fds[0]),
                    SafeDescriptor::from_raw_descriptor(fds[1]),
                )
            },
        ),
    }
}

/// Assist in handling both IP version 4 and IP version 6.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum InetVersion {
    V4,
    V6,
}

impl From<InetVersion> for sa_family_t {
    fn from(v: InetVersion) -> sa_family_t {
        match v {
            InetVersion::V4 => AF_INET as sa_family_t,
            InetVersion::V6 => AF_INET6 as sa_family_t,
        }
    }
}

pub(in crate::sys) fn sockaddrv4_to_lib_c(s: &SocketAddrV4) -> sockaddr_in {
    sockaddr_in {
        sin_family: AF_INET as sa_family_t,
        sin_port: s.port().to_be(),
        sin_addr: in_addr {
            s_addr: u32::from_ne_bytes(s.ip().octets()),
        },
        sin_zero: [0; 8],
        sin_len: size_of::<sockaddr_in>() as u8,
    }
}

pub(in crate::sys) fn sockaddrv6_to_lib_c(s: &SocketAddrV6) -> sockaddr_in6 {
    sockaddr_in6 {
        sin6_family: AF_INET6 as sa_family_t,
        sin6_port: s.port().to_be(),
        sin6_flowinfo: 0,
        sin6_addr: in6_addr {
            s6_addr: s.ip().octets(),
        },
        sin6_scope_id: 0,
        sin6_len: size_of::<sockaddr_in6>() as u8,
    }
}

macro_rules! ScmSocketTryFrom {
    ($name:ident) => {
        impl TryFrom<$name> for ScmSocket<$name> {
            type Error = io::Error;

            fn try_from(socket: $name) -> io::Result<Self> {
                let set = 1;
                let set_ptr = &set as *const c_int as *const c_void;
                let size = size_of::<c_int>() as socklen_t;
                // SAFETY: set_ptr is a pointer to c_int and size is size_of(c_int), trust that the
                // kernel will write size bytes to the pointer.
                let res = unsafe {
                    setsockopt(
                        socket.as_raw_descriptor(),
                        SOL_SOCKET,
                        SO_NOSIGPIPE,
                        set_ptr,
                        size,
                    )
                };
                if res < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(ScmSocket { socket })
                }
            }
        }
    };
}

ScmSocketTryFrom!(UnixDatagram);
ScmSocketTryFrom!(UnixListener);
ScmSocketTryFrom!(UnixSeqpacket);
ScmSocketTryFrom!(UnixStream);

fn cloexec_or_close<Raw: AsRawDescriptor>(raw: Raw) -> io::Result<Raw> {
    // SAFETY: `raw` owns a file descriptor, there are no actions with memory.
    let res = unsafe { fcntl(raw.as_raw_descriptor(), F_SETFD, FD_CLOEXEC) };
    if res >= 0 {
        Ok(raw)
    } else {
        let err = io::Error::last_os_error();
        // SAFETY: `raw` owns this file descriptor.
        unsafe { close(raw.as_raw_descriptor()) };
        Err(err)
    }
}

// Return `sockaddr_un` for a given `path`
pub(in crate::sys) fn sockaddr_un<P: AsRef<Path>>(
    path: P,
) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut addr = libc::sockaddr_un {
        sun_family: libc::AF_UNIX as libc::sa_family_t,
        sun_path: std::array::from_fn(|_| 0),
        sun_len: 0,
    };

    // Check if the input path is valid. Since
    // * The pathname in sun_path should be null-terminated.
    // * The length of the pathname, including the terminating null byte, should not exceed the size
    //   of sun_path.
    //
    // and our input is a `Path`, we only need to check
    // * If the string size of `Path` should less than sizeof(sun_path)
    // and make sure `sun_path` ends with '\0' by initialized the sun_path with zeros.
    //
    // Empty path name is valid since abstract socket address has sun_paht[0] = '\0'
    let bytes = path.as_ref().as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Input path size should be less than the length of sun_path.",
        ));
    };

    // Copy data from `path` to `addr.sun_path`
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }

    // The addrlen argument that describes the enclosing sockaddr_un structure
    // should have a value of at least:
    //
    //     offsetof(struct sockaddr_un, sun_path) + strlen(addr.sun_path) + 1
    //
    // or, more simply, addrlen can be specified as sizeof(struct sockaddr_un).
    addr.sun_len = sun_path_offset() as u8 + bytes.len() as u8 + 1;
    Ok((addr, addr.sun_len as libc::socklen_t))
}

// Offset of sun_path in structure sockaddr_un.
pub(in crate::sys) fn sun_path_offset() -> usize {
    std::mem::offset_of!(libc::sockaddr_un, sun_path)
}

/// A TCP socket.
pub struct TcpSocket {
    pub(in crate::sys) inet_version: InetVersion,
    pub(in crate::sys) descriptor: SafeDescriptor,
}

impl TcpSocket {
    pub fn new(inet_version: InetVersion) -> io::Result<Self> {
        Ok(TcpSocket {
            inet_version,
            descriptor: cloexec_or_close(socket(
                Into::<sa_family_t>::into(inet_version) as libc::c_int,
                SOCK_STREAM,
                0,
            )?)?,
        })
    }
}

impl AsRawDescriptor for TcpSocket {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.descriptor.as_raw_descriptor()
    }
}

/// A Unix socket that emulates `SOCK_SEQPACKET` using `SOCK_STREAM` with length-prefixed messages.
///
/// This is necessary because macOS does not support `SOCK_SEQPACKET` for AF_UNIX sockets.
/// Each message is prefixed with a 4-byte little-endian length field.
#[derive(Debug, Serialize, Deserialize)]
pub struct UnixSeqpacket(SafeDescriptor);

impl UnixSeqpacket {
    /// Open a connection to socket named by `path`.
    ///
    /// # Arguments
    /// * `path` - Path to socket
    ///
    /// # Returns
    /// A `UnixSeqpacket` structure point to the socket
    ///
    /// # Errors
    /// Return `io::Error` when error occurs.
    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        // Use SOCK_STREAM since macOS doesn't support SOCK_SEQPACKET
        let descriptor = socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)?;
        let (addr, len) = sockaddr_un(path.as_ref())?;
        // SAFETY: addr is a valid sockaddr_un on the stack and len is computed by
        // sockaddr_un() to match addr's actual content length.
        unsafe {
            let ret = libc::connect(
                descriptor.as_raw_descriptor(),
                &addr as *const _ as *const _,
                len,
            );
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // Set SO_NOSIGPIPE to avoid SIGPIPE on broken pipe
        let set: c_int = 1;
        // SAFETY: Safe because we own the socket and the pointer is valid.
        unsafe {
            setsockopt(
                descriptor.as_raw_descriptor(),
                SOL_SOCKET,
                SO_NOSIGPIPE,
                &set as *const c_int as *const c_void,
                size_of::<c_int>() as socklen_t,
            );
        }

        Ok(UnixSeqpacket(descriptor))
    }

    /// Creates a pair of connected sockets.
    ///
    /// Both returned file descriptors have the `CLOEXEC` flag set.
    pub fn pair() -> io::Result<(UnixSeqpacket, UnixSeqpacket)> {
        // Use SOCK_STREAM since macOS doesn't support SOCK_SEQPACKET
        let (fd0, fd1) = socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0)?;
        let s0 = UnixSeqpacket::from(fd0);
        let s1 = UnixSeqpacket::from(fd1);

        // Set SO_NOSIGPIPE on both sockets
        let set: c_int = 1;
        for sock in [&s0, &s1] {
            // SAFETY: Safe because we own the socket and the pointer is valid.
            unsafe {
                setsockopt(
                    sock.as_raw_descriptor(),
                    SOL_SOCKET,
                    SO_NOSIGPIPE,
                    &set as *const c_int as *const c_void,
                    size_of::<c_int>() as socklen_t,
                );
            }
        }

        Ok((cloexec_or_close(s0)?, cloexec_or_close(s1)?))
    }

    /// Clone the underlying FD.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }

    /// Gets the number of bytes that can be read from this socket without blocking.
    ///
    /// Note: This returns the raw byte count in the buffer, which includes length prefixes.
    /// For the actual next packet size, use `next_packet_size()`.
    pub fn get_readable_bytes(&self) -> io::Result<usize> {
        let mut byte_count = 0i32;
        // SAFETY:
        // Safe because the kernel will writes an i32 to an i32 pointer only.
        let ret = unsafe { libc::ioctl(self.as_raw_descriptor(), libc::FIONREAD, &mut byte_count) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(byte_count as usize)
        }
    }

    /// Gets the number of bytes in the next packet.
    ///
    /// This peeks at the length prefix to determine the packet size.
    pub fn next_packet_size(&self) -> io::Result<usize> {
        let mut len_buf = [0u8; LENGTH_PREFIX_SIZE];

        // Peek at the length prefix
        // SAFETY: len_buf is a LENGTH_PREFIX_SIZE-byte array on the stack and the length
        // passed to recv matches.
        let ret = unsafe {
            libc::recv(
                self.as_raw_descriptor(),
                len_buf.as_mut_ptr() as *mut c_void,
                LENGTH_PREFIX_SIZE,
                libc::MSG_PEEK,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        if ret == 0 {
            // Connection closed
            return Ok(0);
        }

        if (ret as usize) < LENGTH_PREFIX_SIZE {
            // Partial length prefix - this shouldn't happen in normal operation
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete length prefix",
            ));
        }

        Ok(u32::from_le_bytes(len_buf) as usize)
    }

    /// Write data from a given buffer to the socket with length prefix.
    ///
    /// This method uses a 4-byte little-endian length prefix framing protocol to emulate
    /// SOCK_SEQPACKET message boundaries over SOCK_STREAM. Do NOT mix calls to this method
    /// with `raw_sendmsg` (used by `ScmSocket`/`send_with_fds`), which uses its own independent
    /// length prefix framing. Mixing the two framing protocols on the same socket will corrupt
    /// the message stream.
    ///
    /// # Arguments
    /// * `buf` - A reference to the data buffer.
    ///
    /// # Returns
    /// * `usize` - The size of the message (not including the length prefix).
    ///
    /// # Errors
    /// Returns error when write failed.
    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        // Write length prefix followed by data using writev for atomicity.
        // Using a single writev call ensures the length prefix and data are sent
        // together, preventing interleaving with concurrent sends on the same socket.
        let len = buf.len() as u32;
        let len_buf = len.to_le_bytes();

        let total_len = LENGTH_PREFIX_SIZE + buf.len();
        let mut written = 0usize;
        while written < total_len {
            // Adjust iovec to account for partial writes
            let (adjusted_iov, iov_count) = if written < LENGTH_PREFIX_SIZE {
                // Still need to write part of the length prefix
                let prefix_offset = written;
                let adjusted = [
                    libc::iovec {
                        iov_base: len_buf[prefix_offset..].as_ptr() as *mut c_void,
                        iov_len: LENGTH_PREFIX_SIZE - prefix_offset,
                    },
                    libc::iovec {
                        iov_base: buf.as_ptr() as *mut c_void,
                        iov_len: buf.len(),
                    },
                ];
                (adjusted, 2)
            } else {
                // Length prefix fully written, just the data remains
                let data_offset = written - LENGTH_PREFIX_SIZE;
                let adjusted = [
                    libc::iovec {
                        iov_base: buf[data_offset..].as_ptr() as *mut c_void,
                        iov_len: buf.len() - data_offset,
                    },
                    libc::iovec {
                        iov_base: std::ptr::null_mut(),
                        iov_len: 0,
                    },
                ];
                (adjusted, 1)
            };

            // SAFETY: adjusted_iov points to valid iovec entries and iov_count matches the
            // number of valid entries.
            let ret = unsafe {
                libc::writev(
                    self.as_raw_descriptor(),
                    adjusted_iov.as_ptr(),
                    iov_count,
                )
            };
            if ret < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if ret == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write to socket",
                ));
            }
            written += ret as usize;
        }

        Ok(buf.len())
    }

    /// Read a complete message from the socket.
    ///
    /// This method reads messages framed with a 4-byte little-endian length prefix, matching
    /// the protocol used by `send()`. Do NOT mix calls to this method with `raw_recvmsg`
    /// (used by `ScmSocket`/`recv_with_fds`), which uses its own independent length prefix
    /// framing. Mixing the two framing protocols on the same socket will corrupt the message
    /// stream.
    ///
    /// # Arguments
    /// * `buf` - A mut reference to the data buffer.
    ///
    /// # Returns
    /// * `usize` - The size of bytes read to the buffer.
    ///
    /// # Errors
    /// Returns error when read failed.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        // Read length prefix
        let mut len_buf = [0u8; LENGTH_PREFIX_SIZE];
        let mut read = 0usize;
        while read < LENGTH_PREFIX_SIZE {
            // SAFETY: len_buf[read..] is a valid subslice and the length passed to read
            // does not exceed the remaining capacity.
            let ret = unsafe {
                libc::read(
                    self.as_raw_descriptor(),
                    len_buf[read..].as_mut_ptr() as *mut c_void,
                    LENGTH_PREFIX_SIZE - read,
                )
            };
            if ret < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if ret == 0 {
                if read == 0 {
                    // Clean EOF at message boundary
                    return Ok(0);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete length prefix",
                ));
            }
            read += ret as usize;
        }

        let msg_len = u32::from_le_bytes(len_buf) as usize;

        if msg_len == 0 {
            return Ok(0);
        }

        // Truncate if buffer is too small (mimics SEQPACKET behavior)
        let to_read = std::cmp::min(msg_len, buf.len());
        let to_discard = msg_len - to_read;

        // Read the message data
        read = 0;
        while read < to_read {
            // SAFETY: buf[read..to_read] is a valid subslice and the length passed to read
            // does not exceed the remaining capacity.
            let ret = unsafe {
                libc::read(
                    self.as_raw_descriptor(),
                    buf[read..to_read].as_mut_ptr() as *mut c_void,
                    to_read - read,
                )
            };
            if ret < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if ret == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete message",
                ));
            }
            read += ret as usize;
        }

        // Discard any extra bytes if buffer was too small
        if to_discard > 0 {
            let mut discard_buf = vec![0u8; std::cmp::min(to_discard, 4096)];
            let mut discarded = 0usize;
            while discarded < to_discard {
                let chunk = std::cmp::min(to_discard - discarded, discard_buf.len());
                // SAFETY: discard_buf is a valid heap buffer and chunk does not exceed its length.
                let ret = unsafe {
                    libc::read(
                        self.as_raw_descriptor(),
                        discard_buf.as_mut_ptr() as *mut c_void,
                        chunk,
                    )
                };
                if ret < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err);
                }
                if ret == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "incomplete message during discard",
                    ));
                }
                discarded += ret as usize;
            }
        }

        Ok(to_read)
    }

    /// Read data from the socket fd to a given `Vec`, resizing it to the received packet's size.
    ///
    /// # Arguments
    /// * `buf` - A mut reference to a `Vec` to resize and read into.
    ///
    /// # Errors
    /// Returns error when `recv` or `next_packet_size` failed.
    pub fn recv_to_vec(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        let packet_size = self.next_packet_size()?;
        buf.resize(packet_size, 0);
        let read_bytes = self.recv(buf)?;
        buf.resize(read_bytes, 0);
        Ok(())
    }

    /// Read data from the socket fd to a new `Vec`.
    ///
    /// # Returns
    /// * `vec` - A new `Vec` with the entire received packet.
    ///
    /// # Errors
    /// Returns error when `recv` or `next_packet_size` failed.
    pub fn recv_as_vec(&self) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.recv_to_vec(&mut buf)?;
        Ok(buf)
    }

    #[allow(clippy::useless_conversion)]
    fn set_timeout(&self, timeout: Option<Duration>, kind: libc::c_int) -> io::Result<()> {
        let timeval = match timeout {
            Some(t) => {
                if t.as_secs() == 0 && t.subsec_micros() == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "zero timeout duration is invalid",
                    ));
                }
                // subsec_micros fits in i32 because it is defined to be less than one million.
                let nsec = t.subsec_micros() as i32;
                libc::timeval {
                    tv_sec: t.as_secs() as libc::time_t,
                    tv_usec: libc::suseconds_t::from(nsec),
                }
            }
            None => libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        };
        // SAFETY:
        // Safe because we own the fd, and the length of the pointer's data is the same as the
        // passed in length parameter.
        let ret = unsafe {
            libc::setsockopt(
                self.as_raw_descriptor(),
                libc::SOL_SOCKET,
                kind,
                &timeval as *const libc::timeval as *const libc::c_void,
                mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Sets or removes the timeout for read/recv operations on this socket.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_timeout(timeout, libc::SO_RCVTIMEO)
    }

    /// Sets or removes the timeout for write/send operations on this socket.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_timeout(timeout, libc::SO_SNDTIMEO)
    }

    /// Sets the blocking mode for this socket.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let mut nonblocking = nonblocking as libc::c_int;
        // SAFETY:
        // Safe, setting an FD to not block can't affect memory safety.
        let ret = unsafe { libc::ioctl(self.as_raw_descriptor(), libc::FIONBIO, &mut nonblocking) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl From<UnixSeqpacket> for SafeDescriptor {
    fn from(s: UnixSeqpacket) -> Self {
        s.0
    }
}

impl From<SafeDescriptor> for UnixSeqpacket {
    fn from(s: SafeDescriptor) -> Self {
        Self(s)
    }
}

impl FromRawDescriptor for UnixSeqpacket {
    unsafe fn from_raw_descriptor(descriptor: RawDescriptor) -> Self {
        Self(SafeDescriptor::from_raw_descriptor(descriptor))
    }
}

impl AsRawDescriptor for UnixSeqpacket {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.0.as_raw_descriptor()
    }
}

impl IntoRawDescriptor for UnixSeqpacket {
    fn into_raw_descriptor(self) -> RawDescriptor {
        self.0.into_raw_descriptor()
    }
}

impl io::Read for UnixSeqpacket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.recv(buf)
    }
}

impl io::Write for UnixSeqpacket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Like a `UnixListener` but for accepting `UnixSeqpacket` type sockets.
///
/// On macOS, this uses `SOCK_STREAM` internally since `SOCK_SEQPACKET` is not supported.
pub struct UnixSeqpacketListener {
    descriptor: SafeDescriptor,
    no_path: bool,
}

impl UnixSeqpacketListener {
    /// Creates a new `UnixSeqpacketListener` bound to the given path.
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        // Use SOCK_STREAM since macOS doesn't support SOCK_SEQPACKET
        let descriptor = socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)?;
        let (addr, len) = sockaddr_un(path.as_ref())?;

        // SAFETY: addr is a valid sockaddr_un on the stack and len is computed by
        // sockaddr_un() to match addr's actual content length.
        unsafe {
            let ret = libc::bind(
                descriptor.as_raw_descriptor(),
                &addr as *const _ as *const _,
                len,
            );
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
            let ret = libc::listen(descriptor.as_raw_descriptor(), 128);
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(UnixSeqpacketListener {
            descriptor,
            no_path: false,
        })
    }

    /// Blocks for and accepts a new incoming connection and returns the socket associated with that
    /// connection.
    ///
    /// The returned socket has the close-on-exec flag set.
    pub fn accept(&self) -> io::Result<UnixSeqpacket> {
        // SAFETY: we own this fd and the kernel will not write to null pointers.
        let fd = unsafe { libc::accept(self.as_raw_descriptor(), null_mut(), null_mut()) };
        match fd {
            -1 => Err(io::Error::last_os_error()),
            fd => {
                // SAFETY: the fd is valid as checked above.
                let safe_desc = unsafe { SafeDescriptor::from_raw_descriptor(fd) };
                let sock = UnixSeqpacket::from(cloexec_or_close(safe_desc)?);

                // Set SO_NOSIGPIPE
                let set: c_int = 1;
                // SAFETY: set is a valid c_int on the stack and size_of::<c_int>() matches.
                unsafe {
                    setsockopt(
                        sock.as_raw_descriptor(),
                        SOL_SOCKET,
                        SO_NOSIGPIPE,
                        &set as *const c_int as *const c_void,
                        size_of::<c_int>() as socklen_t,
                    );
                }

                Ok(sock)
            }
        }
    }

    pub fn accept_with_timeout(&self, timeout: Duration) -> io::Result<UnixSeqpacket> {
        let start = Instant::now();

        loop {
            let mut fds = libc::pollfd {
                fd: self.as_raw_descriptor(),
                events: libc::POLLIN,
                revents: 0,
            };
            let elapsed = Instant::now().saturating_duration_since(start);
            let remaining = timeout.checked_sub(elapsed).unwrap_or(Duration::ZERO);
            let cur_timeout_ms = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
            // SAFETY:
            // Safe because we give a valid pointer to a list (of 1) FD.
            match unsafe { libc::poll(&mut fds, 1, cur_timeout_ms) }.cmp(&0) {
                Ordering::Greater => return self.accept(),
                Ordering::Equal => return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT)),
                Ordering::Less => {
                    if Error::last() != Error::new(libc::EINTR) {
                        return Err(io::Error::last_os_error());
                    }
                }
            }
        }
    }

    /// Gets the path that this listener is bound to.
    pub fn path(&self) -> io::Result<PathBuf> {
        let mut addr = sockaddr_un(Path::new(""))?.0;
        if self.no_path {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket has no path",
            ));
        }
        let sun_path_offset = (&addr.sun_path as *const _ as usize
            - &addr.sun_family as *const _ as usize)
            as libc::socklen_t;
        let mut len = mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        // SAFETY:
        // Safe because the length given matches the length of the data of the given pointer.
        let ret = unsafe {
            libc::getsockname(
                self.as_raw_descriptor(),
                &mut addr as *mut libc::sockaddr_un as *mut libc::sockaddr,
                &mut len,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        if addr.sun_family != libc::AF_UNIX as libc::sa_family_t
            || addr.sun_path[0] == 0
            || len < 1 + sun_path_offset
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "getsockname on socket returned invalid value",
            ));
        }

        let path_os_str = OsString::from_vec(
            addr.sun_path[..(len - sun_path_offset - 1) as usize]
                .iter()
                .map(|&c| c as _)
                .collect(),
        );
        Ok(path_os_str.into())
    }

    /// Sets the blocking mode for this socket.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let mut nonblocking = nonblocking as libc::c_int;
        // SAFETY:
        // Safe because the nonblocking arg is valid as declared above.
        let ret = unsafe { libc::ioctl(self.as_raw_descriptor(), libc::FIONBIO, &mut nonblocking) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl AsRawDescriptor for UnixSeqpacketListener {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.descriptor.as_raw_descriptor()
    }
}

impl From<UnixSeqpacketListener> for OwnedFd {
    fn from(val: UnixSeqpacketListener) -> Self {
        val.descriptor.into()
    }
}

/// Used to attempt to clean up a `UnixSeqpacketListener` after it is dropped.
pub struct UnlinkUnixSeqpacketListener(pub UnixSeqpacketListener);

impl AsRawDescriptor for UnlinkUnixSeqpacketListener {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.0.as_raw_descriptor()
    }
}

impl AsRef<UnixSeqpacketListener> for UnlinkUnixSeqpacketListener {
    fn as_ref(&self) -> &UnixSeqpacketListener {
        &self.0
    }
}

impl Deref for UnlinkUnixSeqpacketListener {
    type Target = UnixSeqpacketListener;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for UnlinkUnixSeqpacketListener {
    fn drop(&mut self) {
        if let Ok(path) = self.0.path() {
            if let Err(e) = remove_file(path) {
                warn!("failed to remove control socket file: {:?}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sockaddr_un_zero_length_input() {
        let _res = sockaddr_un(Path::new("")).expect("sockaddr_un failed");
    }

    #[test]
    fn sockaddr_un_long_input_err() {
        let res = sockaddr_un(Path::new(&"a".repeat(108)));
        assert!(res.is_err());
    }

    #[test]
    fn sockaddr_un_long_input_pass() {
        let _res = sockaddr_un(Path::new(&"a".repeat(107))).expect("sockaddr_un failed");
    }

    #[test]
    fn sockaddr_un_len_check() {
        let (_addr, len) = sockaddr_un(Path::new(&"a".repeat(50))).expect("sockaddr_un failed");
        assert_eq!(len, (sun_path_offset() + 50 + 1) as u32);
    }

    #[test]
    fn seqpacket_pair_and_send_recv() {
        let (s1, s2) = UnixSeqpacket::pair().expect("pair failed");

        let msg = b"Hello, world!";
        s1.send(msg).expect("send failed");

        let mut buf = [0u8; 32];
        let n = s2.recv(&mut buf).expect("recv failed");

        assert_eq!(n, msg.len());
        assert_eq!(&buf[..n], msg);
    }

    #[test]
    fn seqpacket_next_packet_size() {
        let (s1, s2) = UnixSeqpacket::pair().expect("pair failed");

        let msg = b"Test message";
        s1.send(msg).expect("send failed");

        let size = s2.next_packet_size().expect("next_packet_size failed");
        assert_eq!(size, msg.len());

        // Consume the message
        let mut buf = [0u8; 32];
        s2.recv(&mut buf).expect("recv failed");
    }

    #[test]
    fn seqpacket_recv_as_vec() {
        let (s1, s2) = UnixSeqpacket::pair().expect("pair failed");

        let msg = b"Vector test";
        s1.send(msg).expect("send failed");

        let received = s2.recv_as_vec().expect("recv_as_vec failed");
        assert_eq!(received, msg);
    }
}

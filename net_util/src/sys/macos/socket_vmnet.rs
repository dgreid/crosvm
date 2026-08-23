// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Network backend for the `socket_vmnet` packet protocol.
//!
//! `socket_vmnet` exposes raw Ethernet frames over a Unix stream. Each frame is
//! prefixed by a four-byte big-endian length. Crosvm's virtio-net frontend
//! expects a 12-byte `virtio_net_hdr_v1` before the Ethernet frame, so this
//! backend adds that header on receive and removes it on transmit.

use std::cmp::min;
use std::ffi::OsStr;
use std::io;
use std::io::Read;
use std::io::Write;
use std::mem::size_of;
use std::net::Ipv4Addr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use base::AsRawDescriptor;
use base::Error as SysError;
use base::FileReadWriteVolatile;
use base::RawDescriptor;
use base::ReadNotifier;
use base::VolatileSlice;
use virtio_sys::virtio_net::virtio_net_hdr;
use virtio_sys::virtio_net::virtio_net_hdr_v1;

use super::TapT;
use crate::Error;
use crate::MacAddress;
use crate::Result;
use crate::TapTCommon;

const LENGTH_PREFIX_SIZE: usize = 4;
const VIRTIO_NET_OFFLOAD_HEADER_SIZE: usize = size_of::<virtio_net_hdr>();
const VIRTIO_NET_HEADER_SIZE: usize = size_of::<virtio_net_hdr_v1>();
const _: () = assert!(VIRTIO_NET_OFFLOAD_HEADER_SIZE == 10);
const _: () = assert!(VIRTIO_NET_HEADER_SIZE == 12);
const ETHERNET_HEADER_SIZE: usize = 14;
const MTU: u16 = 1500;
// socket_vmnet does not expose vmnet_max_packet_size_key to clients. Its
// default MTU of 1500 corresponds to a maximum raw Ethernet frame of 1514.
const MAX_ETHERNET_FRAME_SIZE: usize = MTU as usize + ETHERNET_HEADER_SIZE;
const MAX_CROSVM_FRAME_SIZE: usize = VIRTIO_NET_HEADER_SIZE + MAX_ETHERNET_FRAME_SIZE;

#[derive(Default)]
struct RxState {
    header: [u8; LENGTH_PREFIX_SIZE],
    header_read: usize,
    frame: Vec<u8>,
    frame_read: usize,
    expected_frame_size: Option<usize>,
    failed: Option<(io::ErrorKind, String)>,
}

impl RxState {
    fn reset_frame(&mut self) {
        self.header_read = 0;
        self.frame.clear();
        self.frame_read = 0;
        self.expected_frame_size = None;
    }

    fn fail(&mut self, kind: io::ErrorKind, message: impl Into<String>) -> io::Error {
        let message = message.into();
        self.failed = Some((kind, message.clone()));
        io::Error::new(kind, message)
    }

    fn previous_failure(&self) -> Option<io::Error> {
        self.failed
            .as_ref()
            .map(|(kind, message)| io::Error::new(*kind, message.clone()))
    }
}

/// A `socket_vmnet` connection usable as a virtio-net packet backend.
pub struct SocketVmnet {
    stream: UnixStream,
    rx: RxState,
}

impl SocketVmnet {
    /// Connects to a running `socket_vmnet` daemon.
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .map_err(SysError::from)
            .map_err(Error::CreateSocket)?;
        Self::from_stream(stream)
            .map_err(SysError::from)
            .map_err(Error::CreateSocket)
    }

    fn from_stream(stream: UnixStream) -> io::Result<Self> {
        // RX uses MSG_DONTWAIT so that it can retain incomplete protocol frames
        // without changing TX semantics. Keep TX blocking so write_all can
        // finish a partially written packet before another packet is started.
        stream.set_nonblocking(false)?;
        Ok(Self {
            stream,
            rx: RxState::default(),
        })
    }

    fn receive_frame(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(error) = self.rx.previous_failure() {
            return Err(error);
        }

        while self.rx.header_read < LENGTH_PREFIX_SIZE {
            let header_read = self.rx.header_read;
            match recv_nonblocking(self.stream.as_raw_fd(), &mut self.rx.header[header_read..]) {
                Ok(0) if header_read == 0 => return Ok(0),
                Ok(0) => {
                    return Err(self.rx.fail(
                        io::ErrorKind::UnexpectedEof,
                        "socket_vmnet closed during a frame length prefix",
                    ));
                }
                Ok(count) => self.rx.header_read += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }

        let frame_size = if let Some(frame_size) = self.rx.expected_frame_size {
            frame_size
        } else {
            let frame_size = u32::from_be_bytes(self.rx.header) as usize;
            if !(ETHERNET_HEADER_SIZE..=MAX_ETHERNET_FRAME_SIZE).contains(&frame_size) {
                return Err(self.rx.fail(
                    io::ErrorKind::InvalidData,
                    format!("invalid socket_vmnet frame size {frame_size}"),
                ));
            }
            self.rx.frame.resize(frame_size, 0);
            self.rx.expected_frame_size = Some(frame_size);
            frame_size
        };

        let crosvm_frame_size = VIRTIO_NET_HEADER_SIZE + frame_size;
        if buf.len() < crosvm_frame_size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!(
                    "receive buffer is too small for socket_vmnet frame: {} < {}",
                    buf.len(),
                    crosvm_frame_size
                ),
            ));
        }

        while self.rx.frame_read < frame_size {
            let frame_read = self.rx.frame_read;
            match recv_nonblocking(self.stream.as_raw_fd(), &mut self.rx.frame[frame_read..]) {
                Ok(0) => {
                    return Err(self.rx.fail(
                        io::ErrorKind::UnexpectedEof,
                        "socket_vmnet closed during an Ethernet frame",
                    ));
                }
                Ok(count) => self.rx.frame_read += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }

        buf[..VIRTIO_NET_HEADER_SIZE].fill(0);
        buf[VIRTIO_NET_HEADER_SIZE..crosvm_frame_size].copy_from_slice(&self.rx.frame);
        self.rx.reset_frame();
        Ok(crosvm_frame_size)
    }

    fn transmit_frame(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() < VIRTIO_NET_HEADER_SIZE + ETHERNET_HEADER_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "virtio-net frame is too short",
            ));
        }
        if buf.len() > MAX_CROSVM_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("virtio-net frame is too large: {}", buf.len()),
            ));
        }
        // The final two bytes of virtio_net_hdr_v1 are the receive-only
        // num_buffers field. Linux does not initialize them on transmit, so
        // validate only the offload fields shared with virtio_net_hdr.
        if buf[..VIRTIO_NET_OFFLOAD_HEADER_SIZE]
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "socket_vmnet does not support virtio-net offload headers",
            ));
        }

        write_protocol_frame(&mut self.stream, &buf[VIRTIO_NET_HEADER_SIZE..])?;
        Ok(buf.len())
    }
}

fn recv_nonblocking(descriptor: RawDescriptor, buf: &mut [u8]) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }

    // SAFETY: `buf` is valid for writes of `buf.len()` bytes and `descriptor`
    // belongs to the connected Unix socket held by the caller.
    let result = unsafe {
        libc::recv(
            descriptor,
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

fn write_protocol_frame(writer: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    let frame_size = u32::try_from(frame.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket_vmnet frame length does not fit in u32",
        )
    })?;
    let mut framed = Vec::with_capacity(LENGTH_PREFIX_SIZE + frame.len());
    framed.extend_from_slice(&frame_size.to_be_bytes());
    framed.extend_from_slice(frame);
    writer.write_all(&framed)
}

impl Read for SocketVmnet {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.receive_frame(buf)
    }
}

impl Write for SocketVmnet {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.transmit_frame(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl FileReadWriteVolatile for SocketVmnet {
    fn read_volatile(&mut self, slice: VolatileSlice) -> io::Result<usize> {
        if slice.size() == 0 {
            return Ok(0);
        }
        let mut frame = vec![0; min(slice.size(), MAX_CROSVM_FRAME_SIZE)];
        let count = self.read(&mut frame)?;
        slice.copy_from(&frame[..count]);
        Ok(count)
    }

    fn read_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> io::Result<usize> {
        let total_size = bufs.iter().try_fold(0usize, |total, buf| {
            total.checked_add(buf.size()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "receive buffer size overflow")
            })
        })?;
        if total_size == 0 {
            return Ok(0);
        }

        let mut frame = vec![0; min(total_size, MAX_CROSVM_FRAME_SIZE)];
        let count = self.read(&mut frame)?;
        let mut copied = 0;
        for buf in bufs {
            let copy_size = min(buf.size(), count - copied);
            buf.copy_from(&frame[copied..copied + copy_size]);
            copied += copy_size;
            if copied == count {
                break;
            }
        }
        Ok(count)
    }

    fn write_volatile(&mut self, slice: VolatileSlice) -> io::Result<usize> {
        if slice.size() > MAX_CROSVM_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "virtio-net frame is too large",
            ));
        }
        let mut frame = vec![0; slice.size()];
        slice.copy_to(&mut frame);
        self.write(&frame)
    }

    fn write_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> io::Result<usize> {
        let total_size = bufs.iter().try_fold(0usize, |total, buf| {
            total.checked_add(buf.size()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "transmit buffer size overflow")
            })
        })?;
        if total_size > MAX_CROSVM_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "virtio-net frame is too large",
            ));
        }

        let mut frame = vec![0; total_size];
        let mut copied = 0;
        for buf in bufs {
            let end = copied + buf.size();
            buf.copy_to(&mut frame[copied..end]);
            copied = end;
        }
        self.write(&frame)
    }
}

impl AsRawDescriptor for SocketVmnet {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.stream.as_raw_fd()
    }
}

impl ReadNotifier for SocketVmnet {
    fn get_read_notifier(&self) -> &dyn AsRawDescriptor {
        &self.stream
    }
}

impl TapTCommon for SocketVmnet {
    fn new_with_name(name: &[u8], vnet_hdr: bool, multi_vq: bool) -> Result<Self> {
        if !vnet_hdr || multi_vq {
            return Err(Error::CreateTap(SysError::new(libc::ENOTSUP)));
        }
        Self::connect(Path::new(OsStr::from_bytes(name)))
    }

    fn new(_vnet_hdr: bool, _multi_vq: bool) -> Result<Self> {
        Err(Error::CreateTap(SysError::new(libc::ENOTSUP)))
    }

    fn into_mq_taps(self, vq_pairs: u16) -> Result<Vec<Self>> {
        if vq_pairs != 1 {
            return Err(Error::CreateTap(SysError::new(libc::ENOTSUP)));
        }
        Ok(vec![self])
    }

    fn ip_addr(&self) -> Result<Ipv4Addr> {
        Err(Error::CreateTap(SysError::new(libc::ENOTSUP)))
    }

    fn set_ip_addr(&self, _ip_addr: Ipv4Addr) -> Result<()> {
        Err(Error::CreateTap(SysError::new(libc::ENOTSUP)))
    }

    fn netmask(&self) -> Result<Ipv4Addr> {
        Err(Error::CreateTap(SysError::new(libc::ENOTSUP)))
    }

    fn set_netmask(&self, _netmask: Ipv4Addr) -> Result<()> {
        Err(Error::CreateTap(SysError::new(libc::ENOTSUP)))
    }

    fn mtu(&self) -> Result<u16> {
        Ok(MTU)
    }

    fn set_mtu(&self, mtu: u16) -> Result<()> {
        if mtu == MTU {
            Ok(())
        } else {
            Err(Error::CreateTap(SysError::new(libc::ENOTSUP)))
        }
    }

    fn mac_address(&self) -> Result<MacAddress> {
        Err(Error::CreateTap(SysError::new(libc::ENOTSUP)))
    }

    fn set_mac_address(&self, _mac_addr: MacAddress) -> Result<()> {
        Err(Error::CreateTap(SysError::new(libc::ENOTSUP)))
    }

    fn set_offload(&self, flags: libc::c_uint) -> Result<()> {
        if flags == 0 {
            Ok(())
        } else {
            Err(Error::CreateTap(SysError::new(libc::ENOTSUP)))
        }
    }

    fn enable(&self) -> Result<()> {
        Ok(())
    }

    fn try_clone(&self) -> Result<Self> {
        Err(Error::CloneTap(SysError::new(libc::ENOTSUP)))
    }

    unsafe fn from_raw_descriptor(descriptor: RawDescriptor) -> Result<Self> {
        // SAFETY: The caller guarantees that `descriptor` is an owned, valid
        // connected Unix stream descriptor.
        Self::from_stream(unsafe { UnixStream::from_raw_fd(descriptor) })
            .map_err(SysError::from)
            .map_err(Error::CreateSocket)
    }
}

impl TapT for SocketVmnet {}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::os::unix::net::UnixListener;

    use tempfile::TempDir;

    use super::*;

    fn test_connection() -> (TempDir, SocketVmnet, UnixStream) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("socket_vmnet");
        let listener = UnixListener::bind(&path).unwrap();
        let client = SocketVmnet::connect(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        (directory, client, server)
    }

    fn ethernet_frame(seed: u8) -> Vec<u8> {
        (0..ETHERNET_HEADER_SIZE + 32)
            .map(|index| seed.wrapping_add(index as u8))
            .collect()
    }

    fn framed(frame: &[u8]) -> Vec<u8> {
        let mut result = Vec::with_capacity(LENGTH_PREFIX_SIZE + frame.len());
        result.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        result.extend_from_slice(frame);
        result
    }

    #[test]
    fn reads_back_to_back_frames() {
        let (_directory, mut client, mut server) = test_connection();
        let first = ethernet_frame(1);
        let second = ethernet_frame(31);
        let mut wire = framed(&first);
        wire.extend_from_slice(&framed(&second));
        server.write_all(&wire).unwrap();

        for expected in [&first, &second] {
            let mut received = [0xff; MAX_CROSVM_FRAME_SIZE];
            let count = client.read(&mut received).unwrap();
            assert_eq!(count, VIRTIO_NET_HEADER_SIZE + expected.len());
            assert_eq!(
                &received[..VIRTIO_NET_HEADER_SIZE],
                &[0; VIRTIO_NET_HEADER_SIZE]
            );
            assert_eq!(
                &received[VIRTIO_NET_HEADER_SIZE..count],
                expected.as_slice()
            );
        }
    }

    #[test]
    fn retains_partial_header_and_body() {
        let (_directory, mut client, mut server) = test_connection();
        let frame = ethernet_frame(7);
        let wire = framed(&frame);
        let mut received = [0; MAX_CROSVM_FRAME_SIZE];

        server.write_all(&wire[..2]).unwrap();
        assert_eq!(
            client.read(&mut received).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );

        server.write_all(&wire[2..10]).unwrap();
        assert_eq!(
            client.read(&mut received).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );

        server.write_all(&wire[10..]).unwrap();
        let count = client.read(&mut received).unwrap();
        assert_eq!(count, VIRTIO_NET_HEADER_SIZE + frame.len());
        assert_eq!(&received[VIRTIO_NET_HEADER_SIZE..count], frame.as_slice());
    }

    #[test]
    fn returns_would_block_when_no_frame_is_available() {
        let (_directory, mut client, _server) = test_connection();
        let mut received = [0; MAX_CROSVM_FRAME_SIZE];

        assert_eq!(
            client.read(&mut received).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
    }

    #[test]
    fn returns_clean_eof_between_frames() {
        let (_directory, mut client, server) = test_connection();
        drop(server);
        let mut received = [0; MAX_CROSVM_FRAME_SIZE];

        assert_eq!(client.read(&mut received).unwrap(), 0);
    }

    #[test]
    fn rejects_eof_during_length_prefix() {
        let (_directory, mut client, mut server) = test_connection();
        server.write_all(&[0, 0]).unwrap();
        drop(server);
        let mut received = [0; MAX_CROSVM_FRAME_SIZE];

        assert_eq!(
            client.read(&mut received).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
        assert_eq!(
            client.read(&mut received).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn rejects_eof_during_frame_body() {
        let (_directory, mut client, mut server) = test_connection();
        let frame = ethernet_frame(9);
        let wire = framed(&frame);
        server.write_all(&wire[..LENGTH_PREFIX_SIZE + 3]).unwrap();
        drop(server);
        let mut received = [0; MAX_CROSVM_FRAME_SIZE];

        assert_eq!(
            client.read(&mut received).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
        assert_eq!(
            client.read(&mut received).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn rejects_invalid_frame_lengths() {
        for invalid_size in [0, ETHERNET_HEADER_SIZE - 1, MAX_ETHERNET_FRAME_SIZE + 1] {
            let (_directory, mut client, mut server) = test_connection();
            server
                .write_all(&(invalid_size as u32).to_be_bytes())
                .unwrap();
            let mut received = [0; MAX_CROSVM_FRAME_SIZE];
            assert_eq!(
                client.read(&mut received).unwrap_err().kind(),
                ErrorKind::InvalidData
            );
            assert_eq!(
                client.read(&mut received).unwrap_err().kind(),
                ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn accepts_maximum_sized_frames() {
        let (_directory, mut client, mut server) = test_connection();
        let frame = vec![0x5a; MAX_ETHERNET_FRAME_SIZE];
        server.write_all(&framed(&frame)).unwrap();

        let mut received = [0; MAX_CROSVM_FRAME_SIZE];
        assert_eq!(client.read(&mut received).unwrap(), received.len());
        assert_eq!(&received[VIRTIO_NET_HEADER_SIZE..], frame.as_slice());

        assert_eq!(client.write(&received).unwrap(), received.len());
        let mut transmitted = vec![0; LENGTH_PREFIX_SIZE + frame.len()];
        server.read_exact(&mut transmitted).unwrap();
        assert_eq!(transmitted, framed(&frame));
    }

    #[test]
    fn small_receive_buffer_does_not_consume_frame() {
        let (_directory, mut client, mut server) = test_connection();
        let frame = ethernet_frame(11);
        server.write_all(&framed(&frame)).unwrap();

        let mut too_small = vec![0; VIRTIO_NET_HEADER_SIZE + frame.len() - 1];
        assert_eq!(
            client.read(&mut too_small).unwrap_err().kind(),
            ErrorKind::WriteZero
        );

        let mut received = [0; MAX_CROSVM_FRAME_SIZE];
        let count = client.read(&mut received).unwrap();
        assert_eq!(&received[VIRTIO_NET_HEADER_SIZE..count], frame.as_slice());
    }

    #[test]
    fn writes_framed_ethernet_without_virtio_header() {
        let (_directory, mut client, mut server) = test_connection();
        let frame = ethernet_frame(19);
        let mut crosvm_frame = vec![0; VIRTIO_NET_HEADER_SIZE];
        crosvm_frame.extend_from_slice(&frame);

        assert_eq!(client.write(&crosvm_frame).unwrap(), crosvm_frame.len());
        let mut received = vec![0; LENGTH_PREFIX_SIZE + frame.len()];
        server.read_exact(&mut received).unwrap();
        assert_eq!(received, framed(&frame));

        crosvm_frame[VIRTIO_NET_OFFLOAD_HEADER_SIZE..VIRTIO_NET_HEADER_SIZE].fill(0xff);
        assert_eq!(client.write(&crosvm_frame).unwrap(), crosvm_frame.len());
        server.read_exact(&mut received).unwrap();
        assert_eq!(received, framed(&frame));

        crosvm_frame[0] = 1;
        assert_eq!(
            client.write(&crosvm_frame).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_short_and_oversized_transmit_frames() {
        let (_directory, mut client, _server) = test_connection();
        let too_short = vec![0; VIRTIO_NET_HEADER_SIZE + ETHERNET_HEADER_SIZE - 1];
        assert_eq!(
            client.write(&too_short).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );

        let oversized = vec![0; MAX_CROSVM_FRAME_SIZE + 1];
        assert_eq!(
            client.write(&oversized).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn volatile_vectored_io_preserves_frame_boundaries() {
        let (_directory, mut client, mut server) = test_connection();
        let received_frame = ethernet_frame(23);
        server.write_all(&framed(&received_frame)).unwrap();

        let mut first = [0xff; 9];
        let mut second = [0xff; MAX_CROSVM_FRAME_SIZE - 9];
        let read_count = {
            let bufs = [
                VolatileSlice::new(&mut first),
                VolatileSlice::new(&mut second),
            ];
            client.read_vectored_volatile(&bufs).unwrap()
        };
        let mut combined = first.to_vec();
        combined.extend_from_slice(&second);
        assert_eq!(
            &combined[..VIRTIO_NET_HEADER_SIZE],
            &[0; VIRTIO_NET_HEADER_SIZE]
        );
        assert_eq!(
            &combined[VIRTIO_NET_HEADER_SIZE..read_count],
            received_frame.as_slice()
        );

        let transmitted_frame = ethernet_frame(47);
        let mut crosvm_frame = vec![0; VIRTIO_NET_HEADER_SIZE];
        crosvm_frame.extend_from_slice(&transmitted_frame);
        let split = VIRTIO_NET_HEADER_SIZE + 5;
        let mut tx_first = crosvm_frame[..split].to_vec();
        let mut tx_second = crosvm_frame[split..].to_vec();
        let write_count = {
            let bufs = [
                VolatileSlice::new(&mut tx_first),
                VolatileSlice::new(&mut tx_second),
            ];
            client.write_vectored_volatile(&bufs).unwrap()
        };
        assert_eq!(write_count, crosvm_frame.len());

        let mut wire = vec![0; LENGTH_PREFIX_SIZE + transmitted_frame.len()];
        server.read_exact(&mut wire).unwrap();
        assert_eq!(wire, framed(&transmitted_frame));
    }

    #[test]
    fn protocol_writer_retries_short_writes() {
        struct ShortWriter {
            bytes: Vec<u8>,
            interrupt_next: bool,
            max_write: usize,
        }

        impl Write for ShortWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.interrupt_next {
                    self.interrupt_next = false;
                    return Err(io::Error::from(ErrorKind::Interrupted));
                }
                let count = min(self.max_write, buf.len());
                self.bytes.extend_from_slice(&buf[..count]);
                Ok(count)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let frame = ethernet_frame(59);
        let mut writer = ShortWriter {
            bytes: Vec::new(),
            interrupt_next: true,
            max_write: 3,
        };
        write_protocol_frame(&mut writer, &frame).unwrap();
        let second_frame = ethernet_frame(71);
        write_protocol_frame(&mut writer, &second_frame).unwrap();

        let mut expected = framed(&frame);
        expected.extend_from_slice(&framed(&second_frame));
        assert_eq!(writer.bytes, expected);
    }

    #[test]
    fn supports_only_single_queue_without_offload() {
        let (_directory, client, _server) = test_connection();
        assert_eq!(client.mtu().unwrap(), MTU);
        assert!(client.set_mtu(MTU).is_ok());
        assert!(client.set_mtu(MTU + 1).is_err());
        assert!(client.set_offload(0).is_ok());
        assert!(client.set_offload(1).is_err());
        assert_eq!(client.into_mq_taps(1).unwrap().len(), 1);

        let (_directory, client, _server) = test_connection();
        assert!(client.into_mq_taps(2).is_err());
    }

    #[test]
    fn read_notifier_is_socket_descriptor() {
        let (_directory, client, _server) = test_connection();
        let descriptor = client.as_raw_descriptor();

        assert_ne!(descriptor, -1);
        assert_eq!(client.get_read_notifier().as_raw_descriptor(), descriptor);
    }
}

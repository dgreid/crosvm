// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! MacOS networking stubs.
//!
//! macOS doesn't have Linux-style TAP devices. For networking, crosvm on macOS
//! should use slirp (user-space networking) or vmnet.framework.

use base::FileReadWriteVolatile;

use crate::TapTCommon;

/// MacOS TAP trait - a placeholder since macOS doesn't have traditional TAP devices.
/// Use slirp or vmnet.framework for actual networking.
pub trait TapT: FileReadWriteVolatile + TapTCommon {}

pub mod fakes {
    use std::io;
    use std::io::Read;
    use std::io::Write;
    use std::net::Ipv4Addr;

    use base::AsRawDescriptor;
    use base::FileReadWriteVolatile;
    use base::RawDescriptor;
    use base::VolatileSlice;

    use crate::MacAddress;
    use crate::Result;
    use crate::TapTCommon;

    use super::TapT;

    /// Fake TAP device for testing on macOS
    pub struct FakeTap {
        // Placeholder - no actual TAP device on macOS
    }

    impl FakeTap {
        pub fn new(_vnet_hdr: bool, _multi_vq: bool) -> Result<Self> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }
    }

    impl Read for FakeTap {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::Other, "FakeTap not supported"))
        }
    }

    impl Write for FakeTap {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::Other, "FakeTap not supported"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AsRawDescriptor for FakeTap {
        fn as_raw_descriptor(&self) -> RawDescriptor {
            -1
        }
    }

    impl FileReadWriteVolatile for FakeTap {
        fn read_volatile(&mut self, _slice: VolatileSlice) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::Other, "FakeTap not supported"))
        }

        fn read_vectored_volatile(&mut self, _bufs: &[VolatileSlice]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::Other, "FakeTap not supported"))
        }

        fn write_volatile(&mut self, _slice: VolatileSlice) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::Other, "FakeTap not supported"))
        }

        fn write_vectored_volatile(&mut self, _bufs: &[VolatileSlice]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::Other, "FakeTap not supported"))
        }
    }

    impl TapTCommon for FakeTap {
        fn new_with_name(_name: &[u8], _vnet_hdr: bool, _multi_vq: bool) -> Result<Self> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn new(vnet_hdr: bool, multi_vq: bool) -> Result<Self> {
            FakeTap::new(vnet_hdr, multi_vq)
        }

        fn into_mq_taps(self, _vq_pairs: u16) -> Result<Vec<Self>> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn ip_addr(&self) -> Result<Ipv4Addr> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn set_ip_addr(&self, _ip_addr: Ipv4Addr) -> Result<()> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn netmask(&self) -> Result<Ipv4Addr> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn set_netmask(&self, _netmask: Ipv4Addr) -> Result<()> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn mtu(&self) -> Result<u16> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn set_mtu(&self, _mtu: u16) -> Result<()> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn mac_address(&self) -> Result<MacAddress> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn set_mac_address(&self, _mac_addr: MacAddress) -> Result<()> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn set_offload(&self, _flags: libc::c_uint) -> Result<()> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn enable(&self) -> Result<()> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        fn try_clone(&self) -> Result<Self> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }

        unsafe fn from_raw_descriptor(_descriptor: RawDescriptor) -> Result<Self> {
            Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
        }
    }

    impl TapT for FakeTap {}
}

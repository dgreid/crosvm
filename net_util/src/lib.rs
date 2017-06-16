// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

extern crate libc;
extern crate net_sys;
extern crate sys_util;

use std::fs::File;
use std::mem;
use std::net;
use std::os::raw::*;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use sys_util::{Error, Result};

fn errno_result<T>() -> Result<T> {
    Err(Error::last())
}

unsafe fn ioctl_with_ref<F: AsRawFd, T>(fd: &F, nr: c_ulong, arg: &T) -> c_int {
    libc::ioctl(fd.as_raw_fd(), nr, arg as *const T as *const c_void)
}

unsafe fn ioctl_with_mut_ref<F: AsRawFd, T>(fd: &F, nr: c_ulong, arg: &mut T) -> c_int {
    libc::ioctl(fd.as_raw_fd(), nr, arg as *mut T as *mut c_void)
}

/// Create a sockaddr_in from an IPv4 address, and expose it as
/// an opaque sockaddr suitable for usage by socket ioctls.
fn create_sockaddr(ip_addr: net::Ipv4Addr) -> net_sys::sockaddr {
    // IPv4 addresses big-endian (network order), but Ipv4Addr will give us
    // a view of those bytes directly so we can avoid any endian trickiness.
    let addr_in = net_sys::sockaddr_in {
        sin_family: net_sys::AF_INET as u16,
        sin_port: 0,
        sin_addr: unsafe { mem::transmute(ip_addr.octets()) },
        __pad: [0; 8usize],
    };

    unsafe { mem::transmute(addr_in) }
}

fn create_socket() -> Result<net::UdpSocket> {
    // This is safe since we check the return value.
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        return errno_result();
    }

    // This is safe; nothing else will use or hold onto the raw sock fd.
    Ok(unsafe { net::UdpSocket::from_raw_fd(sock) })
}

/// Handle for a network tap interface.
///
/// For now, this simply wraps the file descriptor for the tap device so methods
/// can run ioctls on the interface. The tap interface fd will be closed when
/// Tap goes out of scope, and the kernel will clean up the interface
/// automatically.
pub struct Tap {
    tap_file: File,
    if_name: [u8; 16usize],
}

impl Tap {
    /// Create a new tap interface.
    pub fn new() -> Result<Tap> {
        // Open calls are safe because we give a constant nul-terminated
        // string and verify the result.
        let fd = unsafe {
            libc::open(b"/dev/net/tun\0".as_ptr() as *const i8,
                       libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC)
        };
        if fd < 0 {
            return errno_result();
        }

        // We just checked that the fd is valid.
        let tuntap = unsafe { File::from_raw_fd(fd) };

        const TUNTAP_DEV_FORMAT: &'static [u8; 8usize] = b"vmtap%d\0";

        // This is pretty messy because of the unions used by ifreq. Since we
        // don't call as_mut on the same union field more than once, this block
        // is safe.
        let mut ifreq: net_sys::ifreq = Default::default();
        unsafe {
            let ifrn_name = ifreq.ifr_ifrn.ifrn_name.as_mut();
            let ifru_flags = ifreq.ifr_ifru.ifru_flags.as_mut();
            let mut name_slice = &mut ifrn_name[..TUNTAP_DEV_FORMAT.len()];
            name_slice.copy_from_slice(TUNTAP_DEV_FORMAT);
            *ifru_flags = (net_sys::IFF_TAP | net_sys::IFF_NO_PI) as c_short;
        }

        // ioctl is safe since we call it with a valid tap fd and check the return
        // value.
        let ret = unsafe { ioctl_with_mut_ref(&tuntap, net_sys::TUNSETIFF(), &mut ifreq) };
        if ret < 0 {
            return errno_result();
        }

        // Safe since only the name is accessed, and it's cloned out.
        Ok(Tap {
               tap_file: tuntap,
               if_name: unsafe { ifreq.ifr_ifrn.ifrn_name.as_ref().clone() },
           })
    }

    pub fn set_ip_addr(&self, ip_addr: net::Ipv4Addr) -> Result<()> {
        let sock = create_socket()?;
        let addr = create_sockaddr(ip_addr);

        let mut ifreq = self.get_ifreq();

        // We only access one field of the ifru union, hence this is safe.
        unsafe {
            let ifru_addr = ifreq.ifr_ifru.ifru_addr.as_mut();
            *ifru_addr = addr;
        }

        // ioctl is safe. Called with a valid sock fd, and we check the return.
        let ret = unsafe { ioctl_with_ref(&sock, net_sys::sockios::SIOCSIFADDR as u64, &ifreq) };
        if ret < 0 {
            return errno_result();
        }

        Ok(())
    }

    pub fn set_netmask(&self, netmask: net::Ipv4Addr) -> Result<()> {
        let sock = create_socket()?;
        let addr = create_sockaddr(netmask);

        let mut ifreq = self.get_ifreq();

        // We only access one field of the ifru union, hence this is safe.
        unsafe {
            let ifru_addr = ifreq.ifr_ifru.ifru_addr.as_mut();
            *ifru_addr = addr;
        }

        // ioctl is safe. Called with a valid sock fd, and we check the return.
        let ret = unsafe { ioctl_with_ref(&sock, net_sys::sockios::SIOCSIFNETMASK as u64, &ifreq) };
        if ret < 0 {
            return errno_result();
        }

        Ok(())
    }

    pub fn enable(&self) -> Result<()> {
        let sock = create_socket()?;

        let mut ifreq = self.get_ifreq();

        // We only access one field of the ifru union, hence this is safe.
        unsafe {
            let ifru_flags = ifreq.ifr_ifru.ifru_flags.as_mut();
            *ifru_flags = (net_sys::net_device_flags_IFF_UP |
                           net_sys::net_device_flags_IFF_RUNNING) as i16;
        }

        // ioctl is safe. Called with a valid sock fd, and we check the return.
        let ret = unsafe { ioctl_with_ref(&sock, net_sys::sockios::SIOCSIFFLAGS as u64, &ifreq) };
        if ret < 0 {
            return errno_result();
        }

        Ok(())
    }

    fn get_ifreq(&self) -> net_sys::ifreq {
        let mut ifreq: net_sys::ifreq = Default::default();

        // This sets the name of the interface, which is the only entry
        // in a single-field union.
        unsafe {
            let ifrn_name = ifreq.ifr_ifrn.ifrn_name.as_mut();
            ifrn_name.clone_from_slice(&self.if_name);
        }

        ifreq
    }
}

impl AsRawFd for Tap {
    fn as_raw_fd(&self) -> RawFd {
        self.tap_file.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_create() {
        Tap::new().unwrap();
    }

    #[test]
    fn tap_configure() {
        let tap = Tap::new().unwrap();
        let ip_addr: net::Ipv4Addr = "100.115.92.5".parse().unwrap();
        let netmask: net::Ipv4Addr = "255.255.255.252".parse().unwrap();

        tap.set_ip_addr(ip_addr).unwrap();
        tap.set_netmask(netmask).unwrap();
    }

    #[test]
    fn tap_enable() {
        let tap = Tap::new().unwrap();

        tap.enable().unwrap();
    }
}

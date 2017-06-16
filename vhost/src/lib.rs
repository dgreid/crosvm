// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

extern crate libc;
extern crate net_util;
extern crate sys_util;
extern crate virtio_sys;

use std::fs::File;
use std::mem;
use std::os::raw::*;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use sys_util::{EventFd, GuestMemory, Error, Result};

fn errno_result<T>() -> Result<T> {
    Err(Error::last())
}

unsafe fn ioctl<F: AsRawFd>(fd: &F, nr: c_ulong) -> c_int {
    libc::ioctl(fd.as_raw_fd(), nr, 0)
}

unsafe fn ioctl_with_ref<F: AsRawFd, T>(fd: &F, nr: c_ulong, arg: &T) -> c_int {
    libc::ioctl(fd.as_raw_fd(), nr, arg as *const T as *const c_void)
}

unsafe fn ioctl_with_mut_ref<F: AsRawFd, T>(fd: &F, nr: c_ulong, arg: &mut T) -> c_int {
    libc::ioctl(fd.as_raw_fd(), nr, arg as *mut T as *mut c_void)
}

unsafe fn ioctl_with_ptr<F: AsRawFd, T>(fd: &F, nr: c_ulong, arg: *const T) -> c_int {
    libc::ioctl(fd.as_raw_fd(), nr, arg as *const c_void)
}

/// Handle to run VHOST_NET ioctls.
///
/// This provides a simple wrapper around a VHOST_NET file descriptor and
/// methods that safely run ioctls on that file descriptor.
///
/// # Examples
///
/// ```
/// # use virtio::VhostNet;
/// let vhost_net = VhostNet::new().unwrap();
/// let features = vhost_net.get_features().unwrap();
/// println!("available features: {:x}", features);
/// ```
pub struct VhostNet {
    vhost_net: File,
}

// The following calls to ioctl are all safe since we:
// 1) Only call VHOST ioctls on a VHOST_NET fd.
// 2) Check the return value and return sys_util::Error on failure.
impl VhostNet {
    /// Opens /dev/vhost-net and holds a file descriptor open for it.
    pub fn new() -> Result<VhostNet> {
        // Open calls are safe because we give a constant nul-terminated
        // string and verify the result.
        let fd = unsafe {
            libc::open(b"/dev/vhost-net\0".as_ptr() as *const i8,
                       libc::O_RDONLY | libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC)
        };
        if fd < 0 {
            return errno_result();
        }
        // There are no other users of this fd, so this is safe.
        Ok(VhostNet { vhost_net: unsafe { File::from_raw_fd(fd) } })
    }

    /// Set the current process as the owner of this file descriptor.
    /// This must be run before any other vhost ioctls.
    pub fn set_owner(&self) -> Result<()> {
        let ret = unsafe { ioctl(&self.vhost_net, virtio_sys::VHOST_SET_OWNER()) };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    /// Get a bitmask of supported virtio/vhost features.
    pub fn get_features(&self) -> Result<u64> {
        let mut avail_features: u64 = 0;
        let ret = unsafe {
            ioctl_with_mut_ref(&self.vhost_net,
                               virtio_sys::VHOST_GET_FEATURES(),
                               &mut avail_features)
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(avail_features)
    }

    /// Inform VHOST_NET which features to enable. This should be a subset of
    /// supported features from VHOST_GET_FEATURES.
    pub fn set_features(&self, features: u64) -> Result<()> {
        let ret =
            unsafe { ioctl_with_ref(&self.vhost_net, virtio_sys::VHOST_SET_FEATURES(), &features) };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    /// Set the guest memory mappings for vhost to use.
    pub fn set_mem_table(&self, memory: &GuestMemory) -> Result<()> {
        let num_regions = memory.num_regions();
        let vec_size_bytes = mem::size_of::<virtio_sys::vhost_memory>() +
                             (num_regions * mem::size_of::<virtio_sys::vhost_memory_region>());
        let bytes: Vec<u8> = vec![0; vec_size_bytes];
        // Convert bytes pointer to a vhost_memory mut ref. The vector has been
        // sized correctly to ensure it can hold vhost_memory and N regions.
        let vhost_memory: &mut virtio_sys::vhost_memory =
            unsafe { &mut *(bytes.as_ptr() as *mut virtio_sys::vhost_memory) };
        vhost_memory.nregions = num_regions as u32;
        // regions is a zero-length array, so taking a mut slice requires that
        // we correctly specify the size to match the amount of backing memory.
        let vhost_regions = unsafe { vhost_memory.regions.as_mut_slice(num_regions as usize) };

        memory
            .with_regions_mut::<_, ()>(|index, guest_addr, size, host_addr| {
                vhost_regions[index] = virtio_sys::vhost_memory_region {
                    guest_phys_addr: guest_addr.offset() as u64,
                    memory_size: size as u64,
                    userspace_addr: host_addr as u64,
                    flags_padding: 0u64,
                };
                Ok(())
            })
            .unwrap();


        let ret = unsafe {
            ioctl_with_ptr(&self.vhost_net,
                           virtio_sys::VHOST_SET_MEM_TABLE(),
                           bytes.as_ptr())
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    /// Set the number of descriptors in the vring.
    pub fn set_vring_num(&self, queue_index: usize, num: u16) -> Result<()> {
        let vring_state = virtio_sys::vhost_vring_state {
            index: queue_index as u32,
            num: num as u32,
        };

        let ret = unsafe {
            ioctl_with_ref(&self.vhost_net,
                           virtio_sys::VHOST_SET_VRING_NUM(),
                           &vring_state)
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    /// Set the addresses for a given vring.
    pub fn set_vring_addr(&self,
                          queue_index: usize,
                          flags: u32,
                          desc_addr: *const u8,
                          used_addr: *const u8,
                          avail_addr: *const u8,
                          log_addr: *const u8)
                          -> Result<()> {
        let vring_addr = virtio_sys::vhost_vring_addr {
            index: queue_index as u32,
            flags: flags,
            desc_user_addr: desc_addr as u64,
            used_user_addr: used_addr as u64,
            avail_user_addr: avail_addr as u64,
            log_guest_addr: log_addr as u64,
        };

        let ret = unsafe {
            ioctl_with_ref(&self.vhost_net,
                           virtio_sys::VHOST_SET_VRING_ADDR(),
                           &vring_addr)
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    /// Set the first index to look for available descriptors.
    pub fn set_vring_base(&self, queue_index: usize, num: u16) -> Result<()> {
        let vring_state = virtio_sys::vhost_vring_state {
            index: queue_index as u32,
            num: num as u32,
        };

        let ret = unsafe {
            ioctl_with_ref(&self.vhost_net,
                           virtio_sys::VHOST_SET_VRING_BASE(),
                           &vring_state)
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    /// Set the eventfd to trigger when buffers have been used by the host.
    pub fn set_vring_call(&self, queue_index: usize, fd: &EventFd) -> Result<()> {
        let vring_file = virtio_sys::vhost_vring_file {
            index: queue_index as u32,
            fd: fd.as_raw_fd(),
        };

        let ret = unsafe {
            ioctl_with_ref(&self.vhost_net,
                           virtio_sys::VHOST_SET_VRING_CALL(),
                           &vring_file)
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    /// Set the eventfd to trigger when buffers are available for the host
    /// to process.
    pub fn set_vring_kick(&self, queue_index: usize, fd: &EventFd) -> Result<()> {
        let vring_file = virtio_sys::vhost_vring_file {
            index: queue_index as u32,
            fd: fd.as_raw_fd(),
        };

        let ret = unsafe {
            ioctl_with_ref(&self.vhost_net,
                           virtio_sys::VHOST_SET_VRING_KICK(),
                           &vring_file)
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    /// Set the file descriptor that will serve as the VHOST_NET backend. In
    /// most cases, this should be a file descriptor for an open tap device.
    pub fn net_set_backend(&self, queue_index: usize, fd: &net_util::Tap) -> Result<()> {
        let vring_file = virtio_sys::vhost_vring_file {
            index: queue_index as u32,
            fd: fd.as_raw_fd(),
        };

        let ret = unsafe {
            ioctl_with_ref(&self.vhost_net,
                           virtio_sys::VHOST_NET_SET_BACKEND(),
                           &vring_file)
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }
}

impl AsRawFd for VhostNet {
    fn as_raw_fd(&self) -> RawFd {
        self.vhost_net.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_vhostnet() {
        VhostNet::new().unwrap();
    }

    #[test]
    fn set_owner() {
        let vhost_net = VhostNet::new().unwrap();
        vhost_net.set_owner().unwrap();
    }

    #[test]
    fn get_features() {
        let vhost_net = VhostNet::new().unwrap();
        vhost_net.get_features().unwrap();
    }

    #[test]
    fn set_features() {
        let vhost_net = VhostNet::new().unwrap();
        vhost_net.set_features(0).unwrap();
    }
}

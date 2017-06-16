// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

// Each ioctl number gets a function instead of a constant because size_of can
// not be used in const expressions.
macro_rules! ioctl_ioc_nr {
    ($name:ident, $dir:expr, $ty:expr, $nr:expr, $size:expr) => (
        pub fn $name() -> ::std::os::raw::c_ulong {
            (($dir << _IOC_DIRSHIFT) |
            ($ty << _IOC_TYPESHIFT) |
            ($nr<< _IOC_NRSHIFT) |
            ($size << _IOC_SIZESHIFT)) as ::std::os::raw::c_ulong
        }
    )
}

macro_rules! ioctl_io_nr {
    ($name:ident, $nr:expr) => (
        ioctl_ioc_nr!($name, _IOC_NONE, VHOST_VIRTIO, $nr, 0);
    )
}

macro_rules! ioctl_ior_nr {
    ($name:ident, $nr:expr, $size:ty) => (
        ioctl_ioc_nr!($name, _IOC_READ, VHOST_VIRTIO, $nr, ::std::mem::size_of::<$size>() as u32);
    )
}

macro_rules! ioctl_iow_nr {
    ($name:ident, $nr:expr, $size:ty) => (
        ioctl_ioc_nr!($name, _IOC_WRITE, VHOST_VIRTIO, $nr, ::std::mem::size_of::<$size>() as u32);
    )
}

macro_rules! ioctl_iowr_nr {
    ($name:ident, $nr:expr, $size:ty) => (
        ioctl_ioc_nr!($name, _IOC_READ|_IOC_WRITE, VHOST_VIRTIO, $nr, ::std::mem::size_of::<$size>() as u32);
    )
}

// generated with bindgen /usr/include/linux/vhost.h --no-unstable-rust --constified-enum '*' --with-derive-default
pub mod vhost;
// generated with bindgen /usr/include/linux/virtio_net.h --no-unstable-rust --constified-enum '*' --with-derive-default
pub mod virtio_net;
// generated with bindgen /usr/include/linux/virtio_ring.h --no-unstable-rust --constified-enum '*' --with-derive-default
pub mod virtio_ring;
pub use vhost::*;
pub use virtio_net::*;
pub use virtio_ring::*;

ioctl_ior_nr!(VHOST_GET_FEATURES, 0x00, ::std::os::raw::c_ulonglong);
ioctl_iow_nr!(VHOST_SET_FEATURES, 0x00, ::std::os::raw::c_ulonglong);
ioctl_io_nr!(VHOST_SET_OWNER, 0x01);
ioctl_io_nr!(VHOST_RESET_OWNER, 0x02);
ioctl_iow_nr!(VHOST_SET_MEM_TABLE, 0x03, vhost_memory);
ioctl_iow_nr!(VHOST_SET_LOG_BASE, 0x04, ::std::os::raw::c_ulonglong);
ioctl_iow_nr!(VHOST_SET_LOG_FD, 0x07, ::std::os::raw::c_int);
ioctl_iow_nr!(VHOST_SET_VRING_NUM, 0x10, vhost_vring_state);
ioctl_iow_nr!(VHOST_SET_VRING_ADDR, 0x11, vhost_vring_addr);
ioctl_iow_nr!(VHOST_SET_VRING_BASE, 0x12, vhost_vring_state);
ioctl_iowr_nr!(VHOST_GET_VRING_BASE, 0x12, vhost_vring_state);
ioctl_iow_nr!(VHOST_SET_VRING_KICK, 0x20, vhost_vring_file);
ioctl_iow_nr!(VHOST_SET_VRING_CALL, 0x21, vhost_vring_file);
ioctl_iow_nr!(VHOST_SET_VRING_ERR, 0x22, vhost_vring_file);
ioctl_iow_nr!(VHOST_NET_SET_BACKEND, 0x30, vhost_vring_file);
ioctl_iow_nr!(VHOST_SCSI_SET_ENDPOINT, 0x40, vhost_scsi_target);
ioctl_iow_nr!(VHOST_SCSI_CLEAR_ENDPOINT, 0x41, vhost_scsi_target);
ioctl_iow_nr!(VHOST_SCSI_GET_ABI_VERSION, 0x42, ::std::os::raw::c_int);
ioctl_iow_nr!(VHOST_SCSI_SET_EVENTS_MISSED, 0x43, ::std::os::raw::c_uint);
ioctl_iow_nr!(VHOST_SCSI_GET_EVENTS_MISSED, 0x44, ::std::os::raw::c_uint);

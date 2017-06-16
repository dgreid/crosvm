// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

pub const _IOC_NRBITS: ::std::os::raw::c_uint = 8;
pub const _IOC_TYPEBITS: ::std::os::raw::c_uint = 8;
pub const _IOC_SIZEBITS: ::std::os::raw::c_uint = 14;
pub const _IOC_DIRBITS: ::std::os::raw::c_uint = 2;
pub const _IOC_NRMASK: ::std::os::raw::c_uint = 255;
pub const _IOC_TYPEMASK: ::std::os::raw::c_uint = 255;
pub const _IOC_SIZEMASK: ::std::os::raw::c_uint = 16383;
pub const _IOC_DIRMASK: ::std::os::raw::c_uint = 3;
pub const _IOC_NRSHIFT: ::std::os::raw::c_uint = 0;
pub const _IOC_TYPESHIFT: ::std::os::raw::c_uint = 8;
pub const _IOC_SIZESHIFT: ::std::os::raw::c_uint = 16;
pub const _IOC_DIRSHIFT: ::std::os::raw::c_uint = 30;
pub const _IOC_NONE: ::std::os::raw::c_uint = 0;
pub const _IOC_WRITE: ::std::os::raw::c_uint = 1;
pub const _IOC_READ: ::std::os::raw::c_uint = 2;
pub const IOC_IN: ::std::os::raw::c_uint = 1073741824;
pub const IOC_OUT: ::std::os::raw::c_uint = 2147483648;
pub const IOC_INOUT: ::std::os::raw::c_uint = 3221225472;
pub const IOCSIZE_MASK: ::std::os::raw::c_uint = 1073676288;
pub const IOCSIZE_SHIFT: ::std::os::raw::c_uint = 16;

pub const TUNTAP: ::std::os::raw::c_uint = 84;

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
        ioctl_ioc_nr!($name, _IOC_NONE, TUNTAP, $nr, 0);
    )
}

macro_rules! ioctl_ior_nr {
    ($name:ident, $nr:expr, $size:ty) => (
        ioctl_ioc_nr!($name, _IOC_READ, TUNTAP, $nr, ::std::mem::size_of::<$size>() as u32);
    )
}

macro_rules! ioctl_iow_nr {
    ($name:ident, $nr:expr, $size:ty) => (
        ioctl_ioc_nr!($name, _IOC_WRITE, TUNTAP, $nr, ::std::mem::size_of::<$size>() as u32);
    )
}

macro_rules! ioctl_iowr_nr {
    ($name:ident, $nr:expr, $size:ty) => (
        ioctl_ioc_nr!($name, _IOC_READ|_IOC_WRITE, TUNTAP, $nr, ::std::mem::size_of::<$size>() as u32);
    )
}

// generated with bindgen /usr/include/linux/if.h --no-unstable-rust
// --constified-enum '*' --with-derive-default -- -D __UAPI_DEF_IF_IFNAMSIZ -D
// __UAPI_DEF_IF_NET_DEVICE_FLAGS -D __UAPI_DEF_IF_IFREQ -D __UAPI_DEF_IF_IFMAP
// Name is "iff" to avoid conflicting with "if" keyword.
// Generated against Linux 4.11 to include fix "uapi: fix linux/if.h userspace
// compilation errors".
// Manual fixup of ifrn_name to be of type c_uchar instead of c_char.
pub mod iff;
// generated with bindgen /usr/include/linux/if_tun.h --no-unstable-rust
// --constified-enum '*' --with-derive-default
pub mod if_tun;
// generated with bindgen /usr/include/linux/in.h --no-unstable-rust
// --constified-enum '*' --with-derive-default
// Name is "inn" to avoid conflicting with "in" keyword.
pub mod inn;
// generated with bindgen /usr/include/linux/sockios.h --no-unstable-rust
// --constified-enum '*' --with-derive-default
pub mod sockios;
pub use iff::*;
pub use if_tun::*;
pub use inn::*;
pub use sockios::*;

ioctl_iow_nr!(TUNSETNOCSUM, 200, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETDEBUG, 201, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETIFF, 202, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETPERSIST, 203, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETOWNER, 204, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETLINK, 205, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETGROUP, 206, ::std::os::raw::c_int);
ioctl_ior_nr!(TUNGETFEATURES, 207, ::std::os::raw::c_uint);
ioctl_iow_nr!(TUNSETOFFLOAD, 208, ::std::os::raw::c_uint);
ioctl_iow_nr!(TUNSETTXFILTER, 209, ::std::os::raw::c_uint);
ioctl_ior_nr!(TUNGETIFF, 210, ::std::os::raw::c_uint);
ioctl_ior_nr!(TUNGETSNDBUF, 211, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETSNDBUF, 212, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNATTACHFILTER, 213, sock_fprog);
ioctl_iow_nr!(TUNDETACHFILTER, 214, sock_fprog);
ioctl_ior_nr!(TUNGETVNETHDRSZ, 215, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETVNETHDRSZ, 216, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETQUEUE, 217, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETIFINDEX, 218, ::std::os::raw::c_uint);
ioctl_ior_nr!(TUNGETFILTER, 219, sock_fprog);
ioctl_iow_nr!(TUNSETVNETLE, 220, ::std::os::raw::c_int);
ioctl_ior_nr!(TUNGETVNETLE, 221, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETVNETBE, 222, ::std::os::raw::c_int);
ioctl_ior_nr!(TUNGETVNETBE, 223, ::std::os::raw::c_int);

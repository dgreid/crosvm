// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Represents an address in the guest's memory space.

use std::cmp::{Eq, Ord, Ordering, PartialEq, PartialOrd};
use std::ops::{Add, Sub, BitAnd, BitOr};

/// Represents an Address in the guest's memory.
#[derive(Clone, Copy, Debug)]
pub struct GuestAddress(usize);

impl GuestAddress {
    /// Creates a guest address from a raw offset.
    pub fn new(raw_addr: usize) -> GuestAddress {
        GuestAddress(raw_addr)
    }

    /// Returns the offset from this address to the given base address.
    ///
    /// # Examples
    ///
    /// ```
    /// # use sys_util::GuestAddress;
    ///   let base = GuestAddress::new(0x100);
    ///   let addr = GuestAddress::new(0x150);
    ///   assert_eq!(addr.offset_from(base), 0x50usize);
    /// ```
    pub fn offset_from(&self, base: GuestAddress) -> usize {
        self.0 - base.0
    }

    /// Returns the address as a usize offset from 0x0.
    /// Use this when a raw number is needed to pass to the kernel.
    pub fn offset(&self) -> usize {
        self.0
    }

    /// Returns the result of the add or None if there is overflow.
    pub fn checked_add(&self, other: usize) -> Option<GuestAddress> {
        self.0.checked_add(other).map(GuestAddress)
    }

    /// Returns the bitwise and of the address with the given mask.
    pub fn mask(&self, mask: u64) -> GuestAddress {
        GuestAddress(self.0 & mask as usize)
    }
}

impl Add for GuestAddress {
    type Output = GuestAddress;

    fn add(self, other: GuestAddress) -> GuestAddress {
        GuestAddress(self.0 + other.0)
    }
}

impl Add<usize> for GuestAddress {
    type Output = GuestAddress;

    fn add(self, other: usize) -> GuestAddress {
        GuestAddress(self.0 + other)
    }
}

impl BitAnd<u64> for GuestAddress {
    type Output = GuestAddress;

    fn bitand(self, other: u64) -> GuestAddress {
        GuestAddress(self.0 & other as usize)
    }
}

impl BitOr<u64> for GuestAddress {
    type Output = GuestAddress;

    fn bitor(self, other: u64) -> GuestAddress {
        GuestAddress(self.0 | other as usize)
    }
}

impl Sub for GuestAddress {
    type Output = GuestAddress;

    fn sub(self, other: GuestAddress) -> GuestAddress {
        GuestAddress(self.0 - other.0)
    }
}

impl Sub<usize> for GuestAddress {
    type Output = GuestAddress;

    fn sub(self, other: usize) -> GuestAddress {
        GuestAddress(self.0 - other)
    }
}

impl PartialEq for GuestAddress {
    fn eq(&self, other: &GuestAddress) -> bool {
        self.0 == other.0
    }
}
impl Eq for GuestAddress {}

impl Ord for GuestAddress {
    fn cmp(&self, other: &GuestAddress) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for GuestAddress {
    fn partial_cmp(&self, other: &GuestAddress) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equals() {
        let a = GuestAddress::new(0x300);
        let b = GuestAddress::new(0x300);
        let c = GuestAddress::new(0x301);
        assert_eq!(a, b);
        assert_eq!(b, a);
        assert_ne!(a, c);
        assert_ne!(c, a);
    }

    #[test]
    fn cmp() {
        let a = GuestAddress::new(0x300);
        let b = GuestAddress::new(0x301);
        assert!(a < b);
        assert!(b > a);
        assert!(!(a < a));
    }

    #[test]
    fn add_sub() {
        let a = GuestAddress::new(0x50);
        let b = GuestAddress::new(0x60);
        assert_eq!(GuestAddress::new(0xb0), a + b);
        assert_eq!(GuestAddress::new(0x10), b - a);
        assert_eq!(a, a + b - b);
    }

    #[test]
    fn checked_add_overflow() {
        let a = GuestAddress::new(0xffffffffffffff55);
        assert_eq!(Some(GuestAddress::new(0xffffffffffffff57)),
                   a.checked_add(2));
        assert!(a.checked_add(0xf0).is_none());
    }
}

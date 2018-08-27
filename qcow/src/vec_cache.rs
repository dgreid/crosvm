// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub trait Cacheable {
    /// Used to check if the item needs to be written out or if it can be discarded.
    fn dirty(&self) -> bool;
}

#[derive(Debug)]
pub struct VecCache<T: 'static + Copy + Default> {
    cluster_addrs: Vec<T>,
    dirty: bool,
}

impl<T: 'static + Copy + Default> VecCache<T> {
    pub fn new(count: usize) -> VecCache<T> {
        VecCache {
            cluster_addrs: vec![Default::default(); count],
            dirty: true,
        }
    }

    pub fn from_vec(addrs: Vec<T>) -> VecCache<T> {
        VecCache {
            cluster_addrs: addrs,
            dirty: false,
        }
    }

    pub fn get(&self, index: usize) -> T {
        *self.cluster_addrs.get(index).unwrap_or(&Default::default())
    }

    pub fn set(&mut self, index: usize, val: T) {
        if index < self.cluster_addrs.len() {
            self.cluster_addrs[index] = val;
            self.dirty = true;
        }
    }

    pub fn addrs(&self) -> &Vec<T> {
        &self.cluster_addrs
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }
}

impl<T: 'static + Copy + Default> Cacheable for VecCache<T> {
    fn dirty(&self) -> bool {
        self.dirty
    }
}

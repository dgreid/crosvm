// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashMap;

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

#[derive(Debug)]
pub struct L2Cache<T: Cacheable> {
    tables: HashMap<usize, T>,
    table_size: usize,
}

impl<T: Cacheable> L2Cache<T> {
    pub fn new(table_size: usize, capacity: usize) -> L2Cache<T> {
        L2Cache {
            tables: HashMap::with_capacity(capacity),
            table_size,
        }
    }

    pub fn contains(&self, l1_index: usize) -> bool {
        self.tables.contains_key(&l1_index)
    }

    pub fn get_table(&self, l1_index: usize) -> Option<&T> {
        self.tables.get(&l1_index)
    }

    pub fn get_table_mut(&mut self, l1_index: usize) -> Option<&mut T> {
        self.tables.get_mut(&l1_index)
    }

    pub fn insert(&mut self, l1_index: usize, table: T) -> Option<(usize, T)> {
        let evicted = if self.tables.len() == self.tables.capacity() {
            // TODO(dgreid) smarter eviction
            let k = self.tables.keys().nth(0).unwrap().clone();
            self.tables.remove_entry(&k)
        } else {
            None
        };

        self.tables.insert(l1_index, table);

        evicted
    }

    pub fn dirty_iter_mut(&mut self) -> impl Iterator<Item = (&usize, &mut T)> {
        self.tables
            .iter_mut()
            .filter_map(|(k, v)| if v.dirty() { Some((k, v)) } else { None })
    }
}

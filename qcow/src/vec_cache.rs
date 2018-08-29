// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Trait that allows for checking if an implementor is dirty. Useful for types that are cached so
/// it can be checked if they need to be committed to disk.
pub trait Cacheable {
    /// Used to check if the item needs to be written out or if it can be discarded.
    fn dirty(&self) -> bool;
}

#[derive(Debug)]
/// Represents a vector that implements the `Cacheable` trait so it can be held in a cache.
pub struct VecCache<T: 'static + Copy + Default> {
    vec: Vec<T>,
    dirty: bool,
}

impl<T: 'static + Copy + Default> VecCache<T> {
    /// Creates a `VecCache` that can hold `count` elements.
    pub fn new(count: usize) -> VecCache<T> {
        VecCache {
            vec: vec![Default::default(); count],
            dirty: true,
        }
    }

    /// Creates a `VecCache` from the passed in `vec`.
    pub fn from_vec(vec: Vec<T>) -> VecCache<T> {
        VecCache {
            vec: vec,
            dirty: false,
        }
    }

    /// Gets the value in the vector if it exists, or returns the default value.
    pub fn get(&self, index: usize) -> T {
        *self.vec.get(index).unwrap_or(&Default::default())
    }

    /// Sets the value at `index` to `val`.
    pub fn set(&mut self, index: usize, val: T) {
        if index < self.vec.len() {
            self.vec[index] = val;
            self.dirty = true;
        }
    }

    /// Gets a reference to the underlying vector.
    pub fn get_vec(&self) -> &Vec<T> {
        &self.vec
    }

    /// Mark this cache element as clean.
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }
}

impl<T: 'static + Copy + Default> Cacheable for VecCache<T> {
    fn dirty(&self) -> bool {
        self.dirty
    }
}

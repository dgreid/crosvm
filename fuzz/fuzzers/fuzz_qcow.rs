// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_main]
extern crate byteorder;
#[macro_use] extern crate libfuzzer_sys;
extern crate libc;
extern crate qcow;
extern crate sys_util;

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use qcow::QcowFile;
use sys_util::SharedMemory;

use std::fs::File;
use std::io::{Cursor, Seek, SeekFrom, Write};

// Take the first 64 bits of data as an address and the next 64 bits as data to
// store there. The rest of the data is used as a qcow image.
fuzz_target!(|data: &[u8]| { // fuzzed code goes here
    if data.len() < 16 {// Need an address and data, each are 8 bytes.
        return;
    }
    let mut disk_image = Cursor::new(data);
    let addr = disk_image.read_u64::<BigEndian>().unwrap();
    let value = disk_image.read_u64::<BigEndian>().unwrap();
    let shm = SharedMemory::new(None).unwrap();
    let mut disk_file: File = shm.into();
    disk_file.write_all(&data[16..]).unwrap();
    disk_file.seek(SeekFrom::Start(0)).unwrap();
    if let Ok(mut qcow) = QcowFile::from(disk_file) {
        if qcow.seek(SeekFrom::Start(addr)).is_ok() {
            let _ = qcow.write_u64::<BigEndian>(value);
        }
    }
});

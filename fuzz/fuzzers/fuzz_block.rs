// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_main]
extern crate byteorder;
#[macro_use] extern crate libfuzzer_sys;
extern crate libc;
extern crate devices;
extern crate sys_util;

use byteorder::{LittleEndian, ReadBytesExt};

use sys_util::{EventFd, GuestAddress, GuestMemory, SharedMemory};

use std::fs::File;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use devices::virtio::{Block, Queue, VirtioDevice};

const MEM_SIZE: usize = 256 * 1024 * 1024;
const DESC_SIZE: u64 = 16; // Bytes in one virtio descriptor.
const QUEUE_SIZE: u16 = 16; // Max entries in the queue.
const CMD_SIZE: usize = 16; // Bytes in the command.

// Take the first 64 bits of data as an address and the next 64 bits as data to
// store there. The rest of the data is used as a qcow image.
fuzz_target!(|data: &[u8]| { // fuzzed code goes here
    let size_u64 = size_of::<u64>();
    let mem = GuestMemory::new(&[(GuestAddress(0), MEM_SIZE)]).unwrap();

    // The fuzz data is interpreted as:
    // starting index 8 bytes
    // command location 8 bytes
    // command 16 bytes
    // descriptors circular buffer
    if data.len() < 4 * size_u64 {// Need an index to start.
        return;
    }

    let mut data_image = Cursor::new(data);

    let first_index = data_image.read_u64::<LittleEndian>().unwrap();
    if first_index > MEM_SIZE as u64 / DESC_SIZE {
        return;
    }
    let first_offset = first_index * DESC_SIZE;
    if first_offset as usize + size_u64 > data.len() {
        return;
    }

    let command_addr = data_image.read_u64::<LittleEndian>().unwrap();
    if command_addr as usize > MEM_SIZE - CMD_SIZE {
        return;
    }
    if mem.write_slice_at_addr(&data[2 * size_u64..(2 * size_u64) + CMD_SIZE],
                               GuestAddress(command_addr as usize)).is_err() {
        return;
    }

    data_image.seek(SeekFrom::Start(first_offset)).unwrap();
    let desc_table = data_image.read_u64::<LittleEndian>().unwrap();

    if mem.write_slice_at_addr(&data[32..], GuestAddress(desc_table as usize)).is_err() {
        return;
    }

    let mut q = Queue::new(QUEUE_SIZE);
    q.ready = true;
    q.size = QUEUE_SIZE / 2;
    q.max_size = QUEUE_SIZE;

    let queue_evts: Vec<EventFd> = vec![EventFd::new().unwrap()];
    let queue_fd = queue_evts[0].as_raw_fd();
    let queue_evt = unsafe { EventFd::from_raw_fd(libc::dup(queue_fd)) };

    let shm = SharedMemory::new(None).unwrap();
    let disk_file: File = shm.into();
    let mut block = Block::new(disk_file).unwrap();

    block.activate(mem,
                   EventFd::new().unwrap(),
                   Arc::new(AtomicUsize::new(0)),
                   vec![q],
                   queue_evts);

    queue_evt.write(77).unwrap();
});

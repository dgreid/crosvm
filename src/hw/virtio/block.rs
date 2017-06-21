// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::convert::From;
use std::fs::File;
use std::io::{Result, Error, Seek, SeekFrom, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::result;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::spawn;

use byteorder::{ByteOrder, LittleEndian};

use sys_util::{EventFd, GuestAddress, GuestMemory, GuestMemoryError};

use super::{VirtioDevice, Queue, DescriptorChain, INTERRUPT_STATUS_USED_RING, TYPE_BLOCK};

const QUEUE_SIZE: u16 = 256;
const QUEUE_SIZES: &'static [u16] = &[QUEUE_SIZE];
const SECTOR_SHIFT: u8 = 9;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

#[derive(Debug)]
enum DiskRequestError {
    Io(Error),
    GuestMemory(GuestMemoryError),
    InvalidRequest,
}

impl From<Error> for DiskRequestError {
    fn from(e: Error) -> DiskRequestError {
        DiskRequestError::Io(e)
    }
}

impl From<GuestMemoryError> for DiskRequestError {
    fn from(e: GuestMemoryError) -> DiskRequestError {
        DiskRequestError::GuestMemory(e)
    }
}

struct DiskRequest {
    rtype: u32,
    sector: u64,
    data_addr: GuestAddress,
    data_len: usize,
    status_addr: GuestAddress,
}

impl DiskRequest {
    fn process<T: Seek + Read + Write>(&self,
                                       disk: &mut T,
                                       mem: &GuestMemory)
                                       -> result::Result<(), DiskRequestError> {
        disk.seek(SeekFrom::Start(self.sector << SECTOR_SHIFT))?;
        match self.rtype {
            VIRTIO_BLK_T_IN => mem.read_to_memory(self.data_addr, disk, self.data_len)?,
            VIRTIO_BLK_T_OUT => mem.write_from_memory(self.data_addr, disk, self.data_len)?,
            VIRTIO_BLK_T_FLUSH => disk.flush()?,
            _ => return Err(DiskRequestError::InvalidRequest),
        };
        Ok(())
    }
}

#[derive(Default)]
struct Request {
    disk_request: Option<DiskRequest>,
    desc_index: u16,
}

struct Worker {
    queues: Vec<Queue>,
    notify_evt: EventFd,
    mem: GuestMemory,
    disk_image: File,
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: EventFd,
}

fn request_type(mem: &GuestMemory, desc_addr: GuestAddress) -> Option<u32> {
    // Reading a u32 is OK because you can't end up with an invalid u32.
    mem.read_obj_from_addr(desc_addr).ok()
}

fn sector(mem: &GuestMemory, desc_addr: GuestAddress) -> Option<u64> {
    const SECTOR_OFFSET: usize = 8;
    let addr = match mem.checked_offset(desc_addr, SECTOR_OFFSET) {
        Some(v) => v,
        None => return None,
    };

    mem.read_obj_from_addr(addr).ok()
}

fn parse_req(req: &mut Request, avail_desc: &DescriptorChain, mem: &GuestMemory) {
    macro_rules! try_opt {
        ($x:expr) => (match $x {
            Some(v) => v,
            _ => return,
        })
    }

    // The head contains the request type which MUST be readable.
    if avail_desc.is_write_only() {
        return;
    }

    let req_type = try_opt!(request_type(&mem, avail_desc.addr));
    let sector = try_opt!(sector(&mem, avail_desc.addr));
    let data_desc = try_opt!(avail_desc.next_descriptor());
    let status_desc = try_opt!(data_desc.next_descriptor());

    // The status MUST always be writable
    if !status_desc.is_write_only() {
        return;
    }

    req.disk_request = Some(DiskRequest {
                                rtype: req_type,
                                sector: sector,
                                data_addr: data_desc.addr,
                                data_len: data_desc.len as usize,
                                status_addr: status_desc.addr,
                            });
}

impl Worker {
    fn read_queue(&mut self, queue_index: usize) -> Vec<Request> {
        let mut out = Vec::new();
        let queue = &mut self.queues[queue_index];

        for avail_desc in queue.iter(&self.mem) {
            let mut req = Request {
                desc_index: avail_desc.index,
                ..Default::default()
            };

            parse_req(&mut req, &avail_desc, &self.mem);

            out.push(req);
        }

        out
    }

    fn process_request(&mut self, request: Request) -> u32 {
        let mut len = 1;
        if let Some(disk_request) = request.disk_request {
            let res = disk_request.process(&mut self.disk_image, &self.mem);
            let status = match res {
                Ok(_) => {
                    if disk_request.rtype == VIRTIO_BLK_T_IN {
                        len = disk_request.data_len as u32;
                    }
                    VIRTIO_BLK_S_OK
                }
                Err(DiskRequestError::InvalidRequest) => VIRTIO_BLK_S_UNSUPP,
                Err(e) => {
                    println!("error: virtio block device io error: {:?}", e);
                    VIRTIO_BLK_S_IOERR
                }
            };
            if self.mem
                   .write_obj_at_addr(status, disk_request.status_addr)
                   .is_err() {
                println!("error: virtio block device status error");
            }
        }

        len
    }

    fn signal_used_queue(&self) {
        self.interrupt_status
            .fetch_or(INTERRUPT_STATUS_USED_RING as usize, Ordering::SeqCst);
        self.interrupt_evt.write(1).unwrap();
    }

    fn run(&mut self) {
        loop {
            if let Err(_) = self.notify_evt.read() {
                println!("error: waiting on eventfd");
                return;
            }

            // If nobody else has a ref on our interrupt_status, quit
            if Arc::strong_count(&self.interrupt_status) == 1 {
                return;
            }

            let mut needs_interrupt = false;
            for queue_index in 0..self.queues.len() {
                let requests = self.read_queue(queue_index);

                for request in requests {
                    let desc_index = request.desc_index;
                    let len = self.process_request(request);
                    self.queues[queue_index].add_used(&self.mem, desc_index, len);
                    needs_interrupt = true;
                }
            }
            if needs_interrupt {
                self.signal_used_queue();
            }
        }
    }
}

/// Virtio device for exposing block level read/write operations on a host file.
pub struct Block {
    disk_size: u64,
    disk_image: Option<File>,
}

impl Block {
    /// Create a new virtio block device that operates on the given file.
    ///
    /// The given file must be seekable and sizable.
    pub fn new(mut disk_image: File) -> Result<Block> {
        let disk_size = disk_image.seek(SeekFrom::End(0))? as u64;

        Ok(Block {
               disk_size: disk_size,
               disk_image: Some(disk_image),
           })
    }
}

impl VirtioDevice for Block {
    fn keep_fds(&self) -> Vec<RawFd> {
        let mut keep_fds = Vec::new();

        if let Some(ref disk_image) = self.disk_image {
            keep_fds.push(disk_image.as_raw_fd());
        }

        keep_fds
    }

    fn device_type(&self) -> u32 {
        TYPE_BLOCK
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let v = match offset {
            0 => ((self.disk_size + 0x511) >> SECTOR_SHIFT) as u32,
            _ => {
                println!("unknown virtio block config read: 0x{:x}", offset);
                0
            }
        };

        LittleEndian::write_u32(data, v);
    }

    fn activate(&mut self,
                mem: GuestMemory,
                interrupt_evt: EventFd,
                status: Arc<AtomicUsize>,
                queues: Vec<Queue>,
                mut queue_evts: Vec<EventFd>) {
        if queues.len() != 1 || queue_evts.len() != 1 {
            return;
        }

        if let Some(disk_image) = self.disk_image.take() {
            spawn(move || {
                let mut worker = Worker {
                    queues: queues,
                    notify_evt: queue_evts.remove(0),
                    mem: mem,
                    disk_image: disk_image,
                    interrupt_status: status,
                    interrupt_evt: interrupt_evt,
                };
                worker.run();
            });
        }
    }
}

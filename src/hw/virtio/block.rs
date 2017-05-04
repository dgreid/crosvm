// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::{Result, Error, ErrorKind};
use std::fs::File;
use std::io::{Seek, SeekFrom, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::spawn;

use byteorder::{ByteOrder, LittleEndian};

use sys_util::{EventFd, GuestAddress, GuestMemory};

use super::{VirtioDevice, Queue, INTERRUPT_STATUS_USED_RING};

const QUEUE_SIZE: u16 = 256;
const QUEUE_SIZES: &'static [u16] = &[QUEUE_SIZE];
const SECTOR_SHIFT: u8 = 9;

const VIRTIO_BLK_T_IN: u32    = 0;
const VIRTIO_BLK_T_OUT: u32   = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;

const VIRTIO_BLK_S_OK: u8     = 0;
const VIRTIO_BLK_S_IOERR: u8  = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

struct DiskRequest {
    rtype: u32,
    sector: u64,
    data_addr: GuestAddress,
    data_len: usize,
}

impl DiskRequest {
    fn process<T: Seek + Read + Write>(&self, disk: &mut T, mem: &GuestMemory) -> Result<()> {
        try!(disk.seek(SeekFrom::Start(self.sector << SECTOR_SHIFT)));
        match self.rtype {
            VIRTIO_BLK_T_IN => {
                if mem.read_to_memory(self.data_addr, disk, self.data_len).is_err() {
                    Err(Error::new(ErrorKind::Other,
                        "virtio block device received out of bounds buffer"))
                } else {
                    Ok(())
                }
            },
            VIRTIO_BLK_T_OUT => {
                if mem.write_from_memory(self.data_addr, disk, self.data_len).is_err() {
                    Err(Error::new(ErrorKind::Other,
                        "virtio block device received out of bounds buffer"))
                } else {
                    Ok(())
                }
            },
            VIRTIO_BLK_T_FLUSH => disk.flush(),
            _ => {
                println!("error: virtio block device received unknown request type: {}", self.rtype);
                Ok(())
            },
       }
    }
}

#[derive(Default)]
struct Request {
    disk_request: Option<DiskRequest>,
    status_addr: Option<GuestAddress>,
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

impl Worker {
    fn read_queue(&mut self, queue_index: usize) -> Vec<Request> {
        let mut out = Vec::new();
        let queue = &mut self.queues[queue_index];

        for avail_desc in queue.iter(&self.mem) {
            let mut req = Request {
                desc_index: avail_desc.index,
                ..Default::default()
            };

            let req_type = avail_desc.request_type().unwrap(); // TODO - don't unwrap
            let sector = avail_desc.sector().unwrap(); // TODO - don't unwrap

            if let Some(data_desc) = avail_desc.next_descriptor() {
                if let Some(status_desc) = data_desc.next_descriptor() {
                    if data_desc.is_write_only() && data_desc.is_write_only() {
                        req.status_addr = Some(status_desc.addr);
                        req.disk_request = Some(DiskRequest {
                            rtype: req_type,
                            sector: sector,
                            data_addr: data_desc.addr,
                            data_len: data_desc.len as usize,
                        });
                    }
                }
            }

            out.push(req);
        }

        out
    }


    fn process_request(&mut self, request: Request) -> u32 {
        let mut len = 1;
        let mut status = VIRTIO_BLK_S_UNSUPP;
        if let Some(disk_request) = request.disk_request {
            let res = disk_request.process(&mut self.disk_image, &self.mem);
            status = match res {
                Ok(_) => {
                    if disk_request.rtype == VIRTIO_BLK_T_IN {
                        len = disk_request.data_len as u32;
                    }
                    VIRTIO_BLK_S_OK
                },
                Err(e) => {
                    println!("error: virtio block device io error: {}", e);
                    VIRTIO_BLK_S_IOERR
                }
            }
        }

        if let Some(status_addr) = request.status_addr {
            if self.mem.write_obj_at_addr(status, status_addr).is_err() {
                println!("error: virtio block device status error");
            }
        }
        len
    }

    fn signal_used_queue(&self) {
        self.interrupt_status.fetch_or(INTERRUPT_STATUS_USED_RING as usize, Ordering::SeqCst);
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
    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let v = match offset {
            0 => ((self.disk_size + 0x511) >> SECTOR_SHIFT) as u32,
            _ =>  {
                println!("unknown virtio block config read: 0x{:x}", offset);
                0
            },
        };

        LittleEndian::write_u32(data, v);
    }

    fn activate(&mut self, mem: GuestMemory, interrupt_evt: EventFd, status: Arc<AtomicUsize>, queues: Vec<Queue>, mut queue_evts: Vec<EventFd>) {
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

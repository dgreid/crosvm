// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::mem;
use std::net::Ipv4Addr;
use std::ptr;
use std::slice;
use std::os::raw::*;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::spawn;

use net_util::Tap;
use sys_util::{Error as SysError, EventFd, GuestMemory, Poller};
use vhost::VhostNet;
use virtio_sys::vhost;
use virtio_sys::virtio_net;
use virtio_sys::virtio_net::virtio_net_config;

use super::{VirtioDevice, Queue, INTERRUPT_STATUS_USED_RING, TYPE_NET};

const QUEUE_SIZE: u16 = 256;
const QUEUE_SIZES: &'static [u16] = &[QUEUE_SIZE, QUEUE_SIZE];
const MAX_QUEUE_PAIRS: u16 = 1;

#[derive(Debug)]
pub enum NetError {
    /// Creating kill eventfd failed.
    CreateKillEventFd(SysError),
    /// Cloning kill eventfd failed.
    CloneKillEventFd(SysError),
    /// Open tap device failed.
    TapOpen(SysError),
    /// Setting tap IP failed.
    TapSetIp(SysError),
    /// Setting tap netmask failed.
    TapSetNetmask(SysError),
    /// Enabling tap interface failed.
    TapEnable(SysError),
    /// Open vhost-net device failed.
    VhostOpen(SysError),
    /// Set owner failed.
    VhostSetOwner(SysError),
    /// Get features failed.
    VhostGetFeatures(SysError),
    /// Set features failed.
    VhostSetFeatures(SysError),
    /// Set mem table failed.
    VhostSetMemTable(SysError),
    /// Set vring num failed.
    VhostSetVringNum(SysError),
    /// Set vring addr failed.
    VhostSetVringAddr(SysError),
    /// Set vring base failed.
    VhostSetVringBase(SysError),
    /// Set vring call failed.
    VhostSetVringCall(SysError),
    /// Set vring kick failed.
    VhostSetVringKick(SysError),
    /// Net set backend failed.
    VhostNetSetBackend(SysError),
    /// Failed to create vhost eventfd.
    VhostIrqCreate(SysError),
    /// Failed to read vhost eventfd.
    VhostIrqRead(SysError),
}

struct Worker {
    queues: Vec<Queue>,
    mem: GuestMemory,
    tap: Tap,
    vhost_net: VhostNet,
    vhost_interrupt: EventFd,
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: EventFd,
    acked_features: u64,
}

impl Worker {
    fn signal_used_queue(&self) {
        self.interrupt_status
            .fetch_or(INTERRUPT_STATUS_USED_RING as usize, Ordering::SeqCst);
        self.interrupt_evt.write(1).unwrap();
    }

    fn run(&mut self, queue_evts: Vec<EventFd>, kill_evt: EventFd) -> Result<(), NetError> {
        // Preliminary setup for vhost net.
        self.vhost_net.set_owner().map_err(NetError::VhostSetOwner)?;

        let avail_features = self.vhost_net
            .get_features()
            .map_err(NetError::VhostGetFeatures)?;

        let features: c_ulonglong = self.acked_features & avail_features |
                                    (1 << vhost::VHOST_NET_F_VIRTIO_NET_HDR);
        self.vhost_net
            .set_features(features)
            .map_err(NetError::VhostSetFeatures)?;

        self.vhost_net
            .set_mem_table(&self.mem)
            .map_err(NetError::VhostSetMemTable)?;

        for (queue_index, _) in self.queues.iter().enumerate() {
            let queue = &self.queues[queue_index];

            self.vhost_net
                .set_vring_num(queue_index, queue.max_size)
                .map_err(NetError::VhostSetVringNum)?;

            // These unwraps are all okay since the closures being run only
            // return Ok.
            let desc_addr = self.mem
                .do_in_region(queue.desc_table, |mapping, offset| {
                    Ok(mapping.as_ptr().wrapping_offset(offset as isize))
                })
                .unwrap();

            let used_addr = self.mem
                .do_in_region(queue.used_ring, |mapping, offset| {
                    Ok(mapping.as_ptr().wrapping_offset(offset as isize))
                })
                .unwrap();

            let avail_addr = self.mem
                .do_in_region(queue.avail_ring, |mapping, offset| {
                    Ok(mapping.as_ptr().wrapping_offset(offset as isize))
                })
                .unwrap();

            self.vhost_net
                .set_vring_addr(queue_index,
                                0,
                                desc_addr,
                                used_addr,
                                avail_addr,
                                ptr::null())
                .map_err(NetError::VhostSetVringAddr)?;
            self.vhost_net
                .set_vring_base(queue_index, 0)
                .map_err(NetError::VhostSetVringBase)?;
            self.vhost_net
                .set_vring_call(queue_index, &self.vhost_interrupt)
                .map_err(NetError::VhostSetVringCall)?;
            self.vhost_net
                .set_vring_kick(queue_index, &queue_evts[queue_index])
                .map_err(NetError::VhostSetVringKick)?;
            self.vhost_net
                .net_set_backend(queue_index, &self.tap)
                .map_err(NetError::VhostNetSetBackend)?;
        }

        const VHOST_IRQ: u32 = 1;
        const KILL: u32 = 2;

        let mut poller = Poller::new(2);

        'poll: loop {
            let tokens =
                match poller.poll(&[(VHOST_IRQ, &self.vhost_interrupt), (KILL, &kill_evt)]) {
                    Ok(v) => v,
                    Err(e) => {
                        println!("net: error polling for events: {:?}", e);
                        break;
                    }
                };

            let mut needs_interrupt = false;
            for &token in tokens {
                match token {
                    VHOST_IRQ => {
                        needs_interrupt = true;
                        self.vhost_interrupt.read().map_err(NetError::VhostIrqRead)?;
                    }
                    KILL => break 'poll,
                    _ => unreachable!(),
                }
            }
            if needs_interrupt {
                self.signal_used_queue();
            }
        }
        Ok(())
    }
}

pub struct Net {
    workers_kill_evt: Option<EventFd>,
    kill_evt: EventFd,
    tap: Option<Tap>,
    vhost_net: Option<VhostNet>,
    vhost_interrupt: Option<EventFd>,
    avail_features: u64,
    acked_features: u64,
    config_le: virtio_net_config,
}

impl Net {
    /// Create a new virtio network device with the given IP address and
    /// netmask.
    pub fn new(ip_addr: Ipv4Addr, netmask: Ipv4Addr) -> Result<Net, NetError> {
        let kill_evt = EventFd::new().map_err(NetError::CreateKillEventFd)?;

        let tap = Tap::new().map_err(NetError::TapOpen)?;
        tap.set_ip_addr(ip_addr).map_err(NetError::TapSetIp)?;
        tap.set_netmask(netmask).map_err(NetError::TapSetNetmask)?;
        tap.enable().map_err(NetError::TapEnable)?;
        let vhost_net = VhostNet::new().map_err(NetError::VhostOpen)?;

        let avail_features =
            1 << virtio_net::VIRTIO_NET_F_GUEST_CSUM | 1 << virtio_net::VIRTIO_NET_F_MRG_RXBUF |
            1 << vhost::VIRTIO_RING_F_INDIRECT_DESC |
            1 << vhost::VIRTIO_RING_F_EVENT_IDX |
            1 << vhost::VIRTIO_F_NOTIFY_ON_EMPTY | 1 << vhost::VIRTIO_F_VERSION_1;

        let config = virtio_net_config {
            // TODO(smbarber): Use real MAC. Note that this doesn't do anything
            // yet anyway, since we haven't declared the MAC feature.
            mac: [0u8, 1u8, 2u8, 3u8, 4u8, 5u8],
            status: 0u16.to_le(),
            max_virtqueue_pairs: MAX_QUEUE_PAIRS.to_le(),
            mtu: 0u16.to_le(),
        };

        Ok(Net {
               workers_kill_evt: Some(kill_evt.try_clone().map_err(NetError::CloneKillEventFd)?),
               kill_evt: kill_evt,
               tap: Some(tap),
               vhost_net: Some(vhost_net),
               vhost_interrupt: Some(EventFd::new().map_err(NetError::VhostIrqCreate)?),
               avail_features: avail_features,
               acked_features: 0u64,
               config_le: config,
           })
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        // Only kill the child if it claimed its eventfd.
        if self.workers_kill_evt.is_none() {
            // Ignore the result because there is nothing we can do about it.
            let _ = self.kill_evt.write(1);
        }
    }
}

impl VirtioDevice for Net {
    fn keep_fds(&self) -> Vec<RawFd> {
        let mut keep_fds = Vec::new();

        if let Some(ref tap) = self.tap {
            keep_fds.push(tap.as_raw_fd());
        }

        if let Some(ref vhost_net) = self.vhost_net {
            keep_fds.push(vhost_net.as_raw_fd());
        }

        if let Some(ref vhost_interrupt) = self.vhost_interrupt {
            keep_fds.push(vhost_interrupt.as_raw_fd());
        }

        if let Some(ref workers_kill_evt) = self.workers_kill_evt {
            keep_fds.push(workers_kill_evt.as_raw_fd());
        }

        keep_fds
    }

    fn device_type(&self) -> u32 {
        TYPE_NET
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn features(&self, page: u32) -> u32 {
        match page {
            0 => self.avail_features as u32,
            1 => (self.avail_features >> 32) as u32,
            _ => {
                println!("error: virtio net got request for features page: {}", page);
                0u32
            }
        }
    }

    fn ack_features(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => value as u64,
            1 => (value as u64) << 32,
            _ => {
                println!("error: virtio net device cannot ack unknown feature page: {}",
                         page);
                0u64
            }
        };

        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let unrequested_features = v & !self.avail_features;
        if unrequested_features != 0 {
            println!("error: virtio net got unknown feature ack: {:x}", v);

            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.acked_features |= v;
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let len = data.len();

        if (offset + len as u64) > mem::size_of::<virtio_net_config>() as u64 ||
           (offset as isize) < 0 {
            println!("unknown virtio net config read: offset 0x{:x} len {}",
                     offset,
                     len);
            return;
        }

        let conf_ptr = &self.config_le as *const virtio_net_config as *const u8;
        // We verified above that the offset + len does not exceed the bounds
        // of the struct. Additionally, the config_le struct is already little
        // endian for the guest.
        data.copy_from_slice(unsafe {
                                 slice::from_raw_parts(conf_ptr.offset(offset as isize), len)
                             });
    }

    fn activate(&mut self,
                mem: GuestMemory,
                interrupt_evt: EventFd,
                status: Arc<AtomicUsize>,
                queues: Vec<Queue>,
                queue_evts: Vec<EventFd>) {
        if queues.len() != 2 || queue_evts.len() != 2 {
            println!("expected 2 queues, got {}", queues.len());
            return;
        }

        if let Some(vhost_net) = self.vhost_net.take() {
            if let Some(tap) = self.tap.take() {
                if let Some(vhost_interrupt) = self.vhost_interrupt.take() {
                    if let Some(kill_evt) = self.workers_kill_evt.take() {
                        let acked_features = self.acked_features;
                        spawn(move || {
                            let mut worker = Worker {
                                queues: queues,
                                mem: mem,
                                tap: tap,
                                vhost_net: vhost_net,
                                vhost_interrupt: vhost_interrupt,
                                interrupt_status: status,
                                interrupt_evt: interrupt_evt,
                                acked_features: acked_features,
                            };
                            let result = worker.run(queue_evts, kill_evt);
                            if result.is_err() {
                                println!("net worker thread exited: {:?}", result);
                            }
                        });
                    }
                }
            }
        }
    }
}

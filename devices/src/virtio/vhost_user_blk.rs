// Copyright (C) 2019 Alibaba Cloud Computing. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
extern crate vhost;

use std::fs::OpenOptions;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::sync::Arc;

use vmm_vhost::vhost_user::message::*;
use vmm_vhost::vhost_user::SlaveListener;
use vmm_vhost::vhost_user::*;

use base::{
    Event, FromRawDescriptor, RawDescriptor, SafeDescriptor, SharedMemory, SharedMemoryUnix,
};
use vm_memory::{GuestAddress, GuestMemory, MemoryRegion};

use devices::virtio::SignalableInterrupt;
use devices::virtio::{base_features, BlockAsync, Queue, VirtioDevice};
use devices::ProtectionType;

pub const MAX_QUEUE_NUM: usize = 2;
pub const MAX_VRING_NUM: usize = 256;
pub const VIRTIO_TRANSPORT_FEATURES: u64 = 0x1_4000_0000;

struct CallEvent(Event);

impl SignalableInterrupt for CallEvent {
    /// Writes to the irqfd to VMM to deliver virtual interrupt to the guest.
    fn signal(&self, _vector: u16, interrupt_status_mask: u32) {
        self.0.write(interrupt_status_mask as u64).unwrap();
    }

    /// Notify the driver that buffers have been placed in the used queue.
    fn signal_used_queue(&self, vector: u16) {
        self.signal(vector, 0 /*INTERRUPT_STATUS_USED_RING*/)
    }

    /// Notify the driver that the device configuration has changed.
    fn signal_config_changed(&self) {} // TODO(dgreid)

    /// Get the event to signal resampling is needed if it exists.
    fn get_resample_evt(&self) -> Option<&Event> {
        None
    }

    /// Reads the status and writes to the interrupt event. Doesn't read the resample event, it
    /// assumes the resample has been requested.
    fn do_interrupt_resample(&self) {}
}

struct QueueInfo {
    desc_table: GuestAddress,
    avail_ring: GuestAddress,
    used_ring: GuestAddress,
}

/// Keeps a mpaaing from the vmm's virtual addresses to guest addresses.
/// used to translate messages from the vmm to guest offsets.
#[derive(Default)]
struct MappingInfo {
    vmm_addr: u64,
    guest_phys: u64,
    size: u64,
}

fn vmm_va_to_gpa(maps: &Vec<MappingInfo>, vmm_va: u64) -> Result<u64> {
    for map in maps {
        if vmm_va >= map.vmm_addr && vmm_va < map.vmm_addr + map.size {
            return Ok(vmm_va - map.vmm_addr + map.guest_phys);
        }
    }
    Err(Error::InvalidMessage)
}

struct MemInfo {
    guest_mem: GuestMemory,
    vmm_maps: Vec<MappingInfo>,
}

pub struct BlockSlaveReqHandler {
    pub owned: bool,
    pub features_acked: bool,
    pub acked_features: u64,
    pub acked_protocol_features: u64,
    pub queue_num: usize,
    pub vring_num: [u32; MAX_QUEUE_NUM],
    pub vring_base: [u32; MAX_QUEUE_NUM],
    queue_info: [Option<QueueInfo>; MAX_QUEUE_NUM],
    pub call_fd: [Option<Event>; MAX_QUEUE_NUM],
    pub kick_fd: [Option<Event>; MAX_QUEUE_NUM],
    pub err_fd: [Option<Event>; MAX_QUEUE_NUM],
    pub vring_started: [bool; MAX_QUEUE_NUM],
    pub vring_enabled: [bool; MAX_QUEUE_NUM],
    vu_req: Option<SlaveFsCacheReq>,
    mem: Option<MemInfo>,
    block: BlockAsync,
}

impl BlockSlaveReqHandler {
    pub fn new<P: AsRef<Path>>(filename: P) -> Self {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(false)
            .open(filename)
            .unwrap();
        let block = BlockAsync::new(
            base_features(ProtectionType::Unprotected),
            Box::new(f),
            false, /*read-only*/
            false, /*sparse*/
            512,
            None,
            None,
        )
        .unwrap();
        BlockSlaveReqHandler {
            owned: false,
            features_acked: false,
            acked_features: 0,
            acked_protocol_features: 0,
            queue_num: MAX_QUEUE_NUM,
            vring_num: [0; MAX_QUEUE_NUM],
            vring_base: [0; MAX_QUEUE_NUM],
            queue_info: Default::default(),
            call_fd: Default::default(),
            kick_fd: Default::default(),
            err_fd: Default::default(),
            vring_started: [false; MAX_QUEUE_NUM],
            vring_enabled: [false; MAX_QUEUE_NUM],
            vu_req: None,
            mem: None,
            block,
        }
    }

    fn potentially_start_dev(&mut self) {
        let num_queues = self.queue_info.iter().filter(|o| o.is_some()).count();
        if num_queues < 1 {
            println!("no queue info {} {}", num_queues, self.queue_num);
            return;
        }
        if self.kick_fd.iter().filter(|o| o.is_some()).count() < num_queues {
            println!(
                "no kicks {} {}",
                self.kick_fd.iter().filter(|o| o.is_some()).count(),
                self.queue_num
            );
            return;
        }
        if self.call_fd.iter().filter(|o| o.is_some()).count() < num_queues {
            println!(
                "no call {} {}",
                self.call_fd.iter().filter(|o| o.is_some()).count(),
                self.queue_num
            );
            return;
        }
        println!("-------- start dev");
        self.start_block_dev();
    }

    fn start_block_dev(&mut self) {
        let call_events = self
            .call_fd
            .iter_mut()
            .filter(|o| o.is_some())
            .map(|io| Box::<CallEvent>::new(CallEvent(io.take().unwrap())))
            .map(|b| Box::<dyn SignalableInterrupt + Send>::from(b))
            .collect();
        let queues = self
            .queue_info
            .iter()
            .filter(|o| o.is_some())
            .zip(self.vring_num.iter())
            .map(|(o, n)| {
                let qi = o.as_ref().unwrap();
                let mut queue = Queue::new(*n as u16);
                queue.desc_table = qi.desc_table;
                queue.avail_ring = qi.avail_ring;
                queue.used_ring = qi.used_ring;
                queue.ready = true;
                queue
            })
            .collect();
        let eventfds = self
            .kick_fd
            .iter_mut()
            .filter(|o| o.is_some())
            .map(|event| event.take().unwrap())
            .collect();
        self.block.reset();
        self.block.activate_vhost(
            self.mem.as_ref().unwrap().guest_mem.clone(),
            call_events,
            queues,
            eventfds,
        );
    }
}

impl VhostUserSlaveReqHandlerMut for BlockSlaveReqHandler {
    fn set_owner(&mut self) -> Result<()> {
        println!("set_owner");
        if self.owned {
            return Err(Error::InvalidOperation);
        }
        self.owned = true;
        Ok(())
    }

    fn reset_owner(&mut self) -> Result<()> {
        println!("reset_owner");
        self.owned = false;
        self.features_acked = false;
        self.acked_features = 0;
        self.acked_protocol_features = 0;
        Ok(())
    }

    fn get_features(&mut self) -> Result<u64> {
        let features = self.block.features() | VIRTIO_TRANSPORT_FEATURES;
        println!("get_features {:x}", features);
        Ok(features)
    }

    fn set_features(&mut self, features: u64) -> Result<()> {
        println!("set_features");
        if !self.owned {
            println!("set_features unowned");
            return Err(Error::InvalidOperation);
        } else if (features & !(self.block.features() | VIRTIO_TRANSPORT_FEATURES)) != 0 {
            println!("set_features no features");
            return Err(Error::InvalidParam);
        }

        self.acked_features = features;
        self.features_acked = true;

        // If VHOST_USER_F_PROTOCOL_FEATURES has not been negotiated,
        // the ring is initialized in an enabled state.
        // If VHOST_USER_F_PROTOCOL_FEATURES has been negotiated,
        // the ring is initialized in a disabled state. Client must not
        // pass data to/from the backend until ring is enabled by
        // VHOST_USER_SET_VRING_ENABLE with parameter 1, or after it has
        // been disabled by VHOST_USER_SET_VRING_ENABLE with parameter 0.
        let vring_enabled =
            self.acked_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() == 0;
        for enabled in &mut self.vring_enabled {
            *enabled = vring_enabled;
        }

        Ok(())
    }

    fn get_protocol_features(&mut self) -> Result<VhostUserProtocolFeatures> {
        println!("get_protocol_features");
        let mut features = VhostUserProtocolFeatures::all();
        features.remove(VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS);
        features.remove(VhostUserProtocolFeatures::INFLIGHT_SHMFD);
        features.remove(VhostUserProtocolFeatures::SLAVE_REQ);
        Ok(features)
    }

    fn set_protocol_features(&mut self, features: u64) -> Result<()> {
        println!("set_protocol_features");
        // Note: slave that reported VHOST_USER_F_PROTOCOL_FEATURES must
        // support this message even before VHOST_USER_SET_FEATURES was
        // called.
        // What happens if the master calls set_features() with
        // VHOST_USER_F_PROTOCOL_FEATURES cleared after calling this
        // interface?
        self.acked_protocol_features = features;
        Ok(())
    }

    fn set_mem_table(&mut self, contexts: &[VhostUserMemoryRegion], fds: &[RawFd]) -> Result<()> {
        println!("set_mem_table:");
        for c in contexts.iter() {
            let size = c.memory_size;
            let offset = c.mmap_offset;
            println!("    {:x} {:x}", size, offset);
        }
        if fds.len() != contexts.len() {
            println!("set_mem_table: mismatched fd/context count.");
            return Err(Error::InvalidParam);
        }

        let regions = contexts
            .iter()
            .zip(fds.iter())
            .map(|(region, &fd)| {
                let sd = unsafe { SafeDescriptor::from_raw_descriptor(fd as RawDescriptor) };
                MemoryRegion::new(
                    region.memory_size,
                    GuestAddress(region.guest_phys_addr),
                    region.mmap_offset,
                    Arc::new(SharedMemory::from_safe_descriptor(sd).unwrap()),
                )
                .unwrap()
            })
            .collect();
        let guest_mem = GuestMemory::from_regions(regions).unwrap();

        let vmm_maps = contexts
            .iter()
            .map(|region| MappingInfo {
                vmm_addr: region.user_addr,
                guest_phys: region.guest_phys_addr,
                size: region.memory_size,
            })
            .collect();
        self.mem = Some(MemInfo {
            guest_mem,
            vmm_maps,
        });
        Ok(())
    }

    fn get_queue_num(&mut self) -> Result<u64> {
        println!("get_queue_num");
        Ok(MAX_QUEUE_NUM as u64)
    }

    fn set_vring_num(&mut self, index: u32, num: u32) -> Result<()> {
        println!("set_vring_num");
        if index as usize >= self.queue_num || num == 0 || num as usize > MAX_VRING_NUM {
            return Err(Error::InvalidParam);
        }
        self.vring_num[index as usize] = num;

        Ok(())
    }

    fn set_vring_addr(
        &mut self,
        index: u32,
        flags: VhostUserVringAddrFlags,
        descriptor: u64,
        used: u64,
        available: u64,
        log: u64,
    ) -> Result<()> {
        println!(
            "set_vring_addr index:{} flags:{:x} desc:{:x} used:{:x} avail:{:x} log:{:x}",
            index, flags, descriptor, used, available, log
        );
        if index as usize >= self.queue_num {
            return Err(Error::InvalidParam);
        }
        if let Some(mem) = &self.mem {
            let queue = QueueInfo {
                desc_table: GuestAddress(vmm_va_to_gpa(&mem.vmm_maps, descriptor).unwrap()),
                avail_ring: GuestAddress(vmm_va_to_gpa(&mem.vmm_maps, available).unwrap()),
                used_ring: GuestAddress(vmm_va_to_gpa(&mem.vmm_maps, used).unwrap()),
            };
            self.queue_info[index as usize] = Some(queue);
            return Ok(());
        }
        return Err(Error::InvalidParam);
    }

    fn set_vring_base(&mut self, index: u32, base: u32) -> Result<()> {
        println!("set_vring_base");
        if index as usize >= self.queue_num || base as usize >= MAX_VRING_NUM {
            return Err(Error::InvalidParam);
        }
        self.vring_base[index as usize] = base;
        Ok(())
    }

    fn get_vring_base(&mut self, index: u32) -> Result<VhostUserVringState> {
        println!("get_vring_base");
        if index as usize >= self.queue_num {
            return Err(Error::InvalidParam);
        }
        // Quotation from vhost-user spec:
        // Client must start ring upon receiving a kick (that is, detecting
        // that file descriptor is readable) on the descriptor specified by
        // VHOST_USER_SET_VRING_KICK, and stop ring upon receiving
        // VHOST_USER_GET_VRING_BASE.
        self.vring_started[index as usize] = false;
        for c in self.call_fd.iter_mut() {
            *c = None;
        }
        for c in self.kick_fd.iter_mut() {
            *c = None;
        }
        for c in self.queue_info.iter_mut() {
            *c = None;
        }
        Ok(VhostUserVringState::new(
            index,
            self.vring_base[index as usize],
        ))
    }

    fn set_vring_kick(&mut self, index: u8, fd: Option<RawFd>) -> Result<()> {
        println!("set_vring_kick");
        if index as usize >= self.queue_num || index as usize > self.queue_num {
            return Err(Error::InvalidParam);
        }
        unsafe {
            // Safe because the FD is now owned.
            self.kick_fd[index as usize] = fd.map(|fd| Event::from_raw_descriptor(fd));
        }

        // Quotation from vhost-user spec:
        // Client must start ring upon receiving a kick (that is, detecting
        // that file descriptor is readable) on the descriptor specified by
        // VHOST_USER_SET_VRING_KICK, and stop ring upon receiving
        // VHOST_USER_GET_VRING_BASE.
        //
        // So we should add fd to event monitor(select, poll, epoll) here.
        self.vring_started[index as usize] = true;
        // TODO:dgreid - call activate here? or wait until both kick,call,and err have been set?
        self.potentially_start_dev();
        Ok(())
    }

    fn set_vring_call(&mut self, index: u8, fd: Option<RawFd>) -> Result<()> {
        println!("set_vring_call {}", index);
        if index as usize >= self.queue_num || index as usize > self.queue_num {
            return Err(Error::InvalidParam);
        }
        unsafe {
            // Safe because the FD is now owned.
            self.call_fd[index as usize] = fd.map(|fd| Event::from_raw_descriptor(fd));
        }
        self.potentially_start_dev();
        Ok(())
    }

    fn set_vring_err(&mut self, index: u8, fd: Option<RawFd>) -> Result<()> {
        println!("set_vring_err");
        if index as usize >= self.queue_num || index as usize > self.queue_num {
            return Err(Error::InvalidParam);
        }
        unsafe {
            // Safe because the FD is now owned.
            self.err_fd[index as usize] = fd.map(|fd| Event::from_raw_descriptor(fd));
        }
        Ok(())
    }

    fn set_vring_enable(&mut self, index: u32, enable: bool) -> Result<()> {
        println!("vring_enable");
        // This request should be handled only when VHOST_USER_F_PROTOCOL_FEATURES
        // has been negotiated.
        if self.acked_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() == 0 {
            return Err(Error::InvalidOperation);
        } else if index as usize >= self.queue_num || index as usize > self.queue_num {
            return Err(Error::InvalidParam);
        }

        // Slave must not pass data to/from the backend until ring is
        // enabled by VHOST_USER_SET_VRING_ENABLE with parameter 1,
        // or after it has been disabled by VHOST_USER_SET_VRING_ENABLE
        // with parameter 0.
        self.vring_enabled[index as usize] = enable;
        self.potentially_start_dev();
        Ok(())
    }

    fn get_config(
        &mut self,
        offset: u32,
        size: u32,
        _flags: VhostUserConfigFlags,
    ) -> Result<Vec<u8>> {
        println!("get_config, {} {}", offset, size);
        if offset >= VHOST_USER_CONFIG_SIZE || size + offset > VHOST_USER_CONFIG_SIZE {
            return Err(Error::InvalidParam);
        }
        let mut data = vec![0; size as usize];
        self.block.read_config(u64::from(offset), &mut data);
        Ok(data)
    }

    fn set_config(&mut self, offset: u32, buf: &[u8], _flags: VhostUserConfigFlags) -> Result<()> {
        let size = buf.len() as u32;
        println!("set_config {}", size);
        if offset < VHOST_USER_CONFIG_OFFSET
            || offset >= VHOST_USER_CONFIG_SIZE
            || size > VHOST_USER_CONFIG_SIZE - VHOST_USER_CONFIG_OFFSET
            || size + offset > VHOST_USER_CONFIG_SIZE
        {
            return Err(Error::InvalidParam);
        }
        Ok(())
    }

    fn set_slave_req_fd(&mut self, vu_req: SlaveFsCacheReq) {
        self.vu_req = Some(vu_req);
    }

    fn get_max_mem_slots(&mut self) -> Result<u64> {
        //TODO
        Ok(0)
    }

    fn add_mem_region(&mut self, region: &VhostUserSingleMemoryRegion, fd: RawFd) -> Result<()> {
        //TODO
        Ok(())
    }

    fn remove_mem_region(&mut self, region: &VhostUserSingleMemoryRegion) -> Result<()> {
        //TODO
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let backend = Arc::new(std::sync::Mutex::new(BlockSlaveReqHandler::new(&args[1])));
    let listener = Listener::new("/tmp/vhost_user_blk.socket", true).unwrap();
    let mut slave_listener = SlaveListener::new(listener, backend).unwrap();
    let mut listener = slave_listener.accept().unwrap().unwrap();
    loop {
        listener.handle_request().unwrap();
    }
}

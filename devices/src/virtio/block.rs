// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cell::RefCell;
use std::cmp::{max, min};
use std::convert::TryFrom;
use std::fmt::{self, Display};
use std::io::{self, Write};
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::u32;

use futures::lock::Mutex as AsyncMutex;
use futures::pin_mut;
use futures::stream::{FuturesUnordered, StreamExt};

use async_core::EventFd as AsyncEventFd;
use async_core::TimerFd as AsyncTimerFd;
use cros_async::select5;
use data_model::{DataInit, Le16, Le32, Le64};
use disk::DiskFile;
use msg_socket::{MsgError, MsgSender};
use sync::Mutex;
use sys_util::Error as SysError;
use sys_util::Result as SysResult;
use sys_util::{error, info, iov_max, warn, EventFd, GuestMemory, TimerFd};
use virtio_sys::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use vm_control::{DiskControlCommand, DiskControlResponseSocket, DiskControlResult};

use super::{
    copy_config, DescriptorChain, DescriptorError, Interrupt, Queue, Reader, VirtioDevice, Writer,
    TYPE_BLOCK, VIRTIO_F_VERSION_1,
};

const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: u16 = 1;
const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE; NUM_QUEUES as usize];
const SECTOR_SHIFT: u8 = 9;
const SECTOR_SIZE: u64 = 0x01 << SECTOR_SHIFT;
const MAX_DISCARD_SECTORS: u32 = u32::MAX;
const MAX_WRITE_ZEROES_SECTORS: u32 = u32::MAX;
// Arbitrary limits for number of discard/write zeroes segments.
const MAX_DISCARD_SEG: u32 = 32;
const MAX_WRITE_ZEROES_SEG: u32 = 32;
// Hard-coded to 64 KiB (in 512-byte sectors) for now,
// but this should probably be based on cluster size for qcow.
const DISCARD_SECTOR_ALIGNMENT: u32 = 128;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_DISCARD: u32 = 11;
const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const VIRTIO_BLK_F_SEG_MAX: u32 = 2;
const VIRTIO_BLK_F_RO: u32 = 5;
const VIRTIO_BLK_F_BLK_SIZE: u32 = 6;
const VIRTIO_BLK_F_FLUSH: u32 = 9;
const VIRTIO_BLK_F_MQ: u32 = 12;
const VIRTIO_BLK_F_DISCARD: u32 = 13;
const VIRTIO_BLK_F_WRITE_ZEROES: u32 = 14;

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtio_blk_geometry {
    cylinders: Le16,
    heads: u8,
    sectors: u8,
}

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for virtio_blk_geometry {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtio_blk_topology {
    physical_block_exp: u8,
    alignment_offset: u8,
    min_io_size: Le16,
    opt_io_size: Le32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for virtio_blk_topology {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtio_blk_config {
    capacity: Le64,
    size_max: Le32,
    seg_max: Le32,
    geometry: virtio_blk_geometry,
    blk_size: Le32,
    topology: virtio_blk_topology,
    writeback: u8,
    unused0: u8,
    num_queues: Le16,
    max_discard_sectors: Le32,
    max_discard_seg: Le32,
    discard_sector_alignment: Le32,
    max_write_zeroes_sectors: Le32,
    max_write_zeroes_seg: Le32,
    write_zeroes_may_unmap: u8,
    unused1: [u8; 3],
}

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for virtio_blk_req_header {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtio_blk_req_header {
    req_type: Le32,
    reserved: Le32,
    sector: Le64,
}

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for virtio_blk_config {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtio_blk_discard_write_zeroes {
    sector: Le64,
    num_sectors: Le32,
    flags: Le32,
}

const VIRTIO_BLK_DISCARD_WRITE_ZEROES_FLAG_UNMAP: u32 = 1 << 0;

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for virtio_blk_discard_write_zeroes {}

#[derive(Debug)]
enum ExecuteError {
    // Error creating a receiver for command messages.
    CreatingMessageReceiver(MsgError),
    Descriptor(DescriptorError),
    Read(io::Error),
    ReceivingCommand(MsgError),
    SendingResponse(MsgError),
    WriteStatus(io::Error),
    /// Error arming the flush timer.
    Flush(io::Error),
    ReadIo {
        length: usize,
        sector: u64,
        desc_error: io::Error,
    },
    TimerFd(SysError),
    WriteIo {
        length: usize,
        sector: u64,
        desc_error: io::Error,
    },
    DiscardWriteZeroes {
        ioerr: Option<io::Error>,
        sector: u64,
        num_sectors: u32,
        flags: u32,
    },
    ReadOnly {
        request_type: u32,
    },
    OutOfRange,
    MissingStatus,
    Unsupported(u32),
}

impl Display for ExecuteError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::ExecuteError::*;

        match self {
            CreatingMessageReceiver(e) => write!(f, "couldn't create a message receiver: {}", e),
            Descriptor(e) => write!(f, "virtio descriptor error: {}", e),
            Read(e) => write!(f, "failed to read message: {}", e),
            ReceivingCommand(e) => write!(f, "failed to read command message: {}", e),
            SendingResponse(e) => write!(f, "failed to send command response: {}", e),
            WriteStatus(e) => write!(f, "failed to write request status: {}", e),
            Flush(e) => write!(f, "failed to flush: {}", e),
            ReadIo {
                length,
                sector,
                desc_error,
            } => write!(
                f,
                "io error reading {} bytes from sector {}: {}",
                length, sector, desc_error,
            ),
            TimerFd(e) => write!(f, "{}", e),
            WriteIo {
                length,
                sector,
                desc_error,
            } => write!(
                f,
                "io error writing {} bytes to sector {}: {}",
                length, sector, desc_error,
            ),
            DiscardWriteZeroes {
                ioerr: Some(ioerr),
                sector,
                num_sectors,
                flags,
            } => write!(
                f,
                "failed to perform discard or write zeroes; sector={} num_sectors={} flags={}; {}",
                sector, num_sectors, flags, ioerr,
            ),
            DiscardWriteZeroes {
                ioerr: None,
                sector,
                num_sectors,
                flags,
            } => write!(
                f,
                "failed to perform discard or write zeroes; sector={} num_sectors={} flags={}",
                sector, num_sectors, flags,
            ),
            ReadOnly { request_type } => write!(f, "read only; request_type={}", request_type),
            OutOfRange => write!(f, "out of range"),
            MissingStatus => write!(f, "not enough space in descriptor chain to write status"),
            Unsupported(n) => write!(f, "unsupported ({})", n),
        }
    }
}

impl ExecuteError {
    fn status(&self) -> u8 {
        match self {
            ExecuteError::CreatingMessageReceiver(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Descriptor(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Read(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::ReceivingCommand(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::SendingResponse(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::WriteStatus(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Flush(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::ReadIo { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::TimerFd(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::WriteIo { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::DiscardWriteZeroes { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::ReadOnly { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::OutOfRange { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::MissingStatus => VIRTIO_BLK_S_IOERR,
            ExecuteError::Unsupported(_) => VIRTIO_BLK_S_UNSUPP,
        }
    }
}

#[derive(Debug)]
enum TimerError {
    TimerFdCreate(sys_util::Error),
    AsyncTimerCreate(async_core::TimerFdError),
}

impl Display for TimerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::TimerError::*;

        match self {
            TimerFdCreate(e) => write!(f, "couldn't create a timer FD: {}", e),
            AsyncTimerCreate(e) => write!(f, "couldn't create an async timer: {}", e),
        }
    }
}

#[derive(Debug)]
struct DiskState {
    disk_image: Box<dyn DiskFile>,
    disk_size: Arc<Mutex<u64>>,
    read_only: bool,
    sparse: bool,
}

fn process_one_request(
    avail_desc: DescriptorChain,
    read_only: bool,
    sparse: bool,
    disk: &mut dyn DiskFile,
    disk_size: u64,
    flush_timer: &TimerFd,
    flush_timer_armed: &AtomicBool,
    mem: &GuestMemory,
) -> result::Result<usize, ExecuteError> {
    let mut reader = Reader::new(mem, avail_desc.clone()).map_err(ExecuteError::Descriptor)?;
    let mut writer = Writer::new(mem, avail_desc).map_err(ExecuteError::Descriptor)?;

    // The last byte of the buffer is virtio_blk_req::status.
    // Split it into a separate Writer so that status_writer is the final byte and
    // the original writer is left with just the actual block I/O data.
    let available_bytes = writer.available_bytes();
    let status_offset = available_bytes
        .checked_sub(1)
        .ok_or(ExecuteError::MissingStatus)?;
    let mut status_writer = writer
        .split_at(status_offset)
        .map_err(ExecuteError::Descriptor)?;

    let status = match Block::execute_request(
        &mut reader,
        &mut writer,
        read_only,
        sparse,
        disk,
        disk_size,
        flush_timer,
        flush_timer_armed,
    ) {
        Ok(()) => VIRTIO_BLK_S_OK,
        Err(e) => {
            error!("failed executing disk request: {}", e);
            e.status()
        }
    };

    status_writer
        .write_all(&[status])
        .map_err(ExecuteError::WriteStatus)?;
    Ok(available_bytes)
}

async fn handle_queue(
    mem: &GuestMemory,
    disk_state: Rc<RefCell<DiskState>>,
    queue_mutex: Arc<AsyncMutex<Queue>>,
    evt_mutex: Arc<AsyncMutex<AsyncEventFd>>,
    flush_timer: &AsyncTimerFd,
    flush_timer_armed: &AtomicBool,
    interrupt: Rc<RefCell<Interrupt>>,
) {
    loop {
        let avail_desc = {
            let mut queue = queue_mutex.lock().await;
            let mut evt = evt_mutex.lock().await;
            let avail_desc = queue.next_async(mem, &mut evt).await;
            match avail_desc {
                Err(e) => {
                    error!("Failed to read the next descriptor from the queue: {}", e);
                    return;
                }
                Ok(a) => a,
            }
        };

        let mut disk_state = disk_state.borrow_mut();

        // tricky lock work to prevent borrowing disk_state for the entire loop.
        let disk_size_lock = disk_state.disk_size.clone();
        let disk_size = disk_size_lock.lock();

        //queue.set_notify(&mem, false); // TODO - might need to set notify to counter of contexts that disabled it.
        let desc_index = avail_desc.index;

        let len = match process_one_request(
            avail_desc,
            disk_state.read_only,
            disk_state.sparse,
            &mut *disk_state.disk_image,
            *disk_size,
            flush_timer.as_ref(),
            flush_timer_armed,
            &mem,
        ) {
            Ok(len) => len,
            Err(e) => {
                error!("block: failed to handle request: {}", e);
                0
            }
        };

        {
            let mut queue = queue_mutex.lock().await;
            queue.add_used(&mem, desc_index, len as u32);
            interrupt.borrow_mut().signal_used_queue(queue.vector);
            //    queue.set_notify(&mem, true);
        }
    }
}

async fn flush_timer(
    disk_state: Rc<RefCell<DiskState>>,
    timer: &AsyncTimerFd,
    timer_armed: &AtomicBool,
) {
    loop {
        let t = timer.next_expiration().await;
        if let Err(e) = t {
            error!("Error with flush timer: {}", e);
            return;
        }
        timer_armed.store(false, Ordering::Relaxed);
        let mut disk_state = disk_state.borrow_mut();
        if let Err(e) = disk_state.disk_image.fsync() {
            error!("Failed to flush the disk: {}", e);
        }
    }
}

async fn handle_irq_resample(interrupt: Rc<RefCell<Interrupt>>) {
    let resample_evt = interrupt
        .borrow_mut()
        .get_resample_evt()
        .try_clone()
        .unwrap();
    let mut resample_evt = AsyncEventFd::try_from(resample_evt).unwrap();
    loop {
        if let Ok(_) = resample_evt.read_next().await {
            interrupt.borrow_mut().interrupt_resample();
        } else {
            break;
        }
    }
}

async fn wait_kill(kill_evt: EventFd) {
    let mut kill_evt = AsyncEventFd::try_from(kill_evt).unwrap();
    // Once this event is readable, exit. Exiting this future will cause the main loop to
    // break and the device process to exit.
    let _ = kill_evt.read_next().await;
}

async fn handle_command_socket(
    command_socket: &DiskControlResponseSocket,
    interrupt: Rc<RefCell<Interrupt>>,
    disk_state: Rc<RefCell<DiskState>>,
) -> Result<(), ExecuteError> {
    let mut async_messages = command_socket
        .async_receiver()
        .map_err(ExecuteError::CreatingMessageReceiver)?;
    loop {
        match async_messages.next().await {
            Ok(command) => {
                let resp = match command {
                    DiskControlCommand::Resize { new_size } => {
                        resize(&mut disk_state.borrow_mut(), new_size)
                    }
                };

                command_socket
                    .send(&resp)
                    .map_err(ExecuteError::SendingResponse)?;
                interrupt.borrow_mut().signal_config_changed();
            }
            Err(e) => return Err(ExecuteError::ReceivingCommand(e)),
        }
    }
}

fn resize(disk_state: &mut DiskState, new_size: u64) -> DiskControlResult {
    if disk_state.read_only {
        error!("Attempted to resize read-only block device");
        return DiskControlResult::Err(SysError::new(libc::EROFS));
    }

    info!("Resizing block device to {} bytes", new_size);

    if let Err(e) = disk_state.disk_image.set_len(new_size) {
        error!("Resizing disk failed! {}", e);
        return DiskControlResult::Err(SysError::new(libc::EIO));
    }

    // Allocate new space if the disk image is not sparse.
    if let Err(e) = disk_state.disk_image.allocate(0, new_size) {
        error!("Allocating disk space after resize failed! {}", e);
        return DiskControlResult::Err(SysError::new(libc::EIO));
    }

    disk_state.sparse = false;

    if let Ok(new_disk_size) = disk_state.disk_image.get_len() {
        let mut disk_size = disk_state.disk_size.lock();
        *disk_size = new_disk_size;
    }
    DiskControlResult::Ok
}

fn run_worker(
    interrupt: Interrupt,
    queues: Vec<Queue>,
    mem: GuestMemory,
    disk_state: &Rc<RefCell<DiskState>>,
    control_socket: &DiskControlResponseSocket,
    queue_evts: Vec<EventFd>,
    kill_evt: EventFd,
) -> Result<(), String> {
    // Wrap the interupt in a `RefCell` so it can be shared between async functions.
    let interrupt = Rc::new(RefCell::new(interrupt));

    let async_timer = TimerFd::new()
        .map_err(TimerError::TimerFdCreate)
        .and_then(|sync_timer| {
            AsyncTimerFd::try_from(sync_timer).map_err(TimerError::AsyncTimerCreate)
        })
        .map_err(|e| format!("Failed to create the flush timer: {}", e))?;
    let flush_timer_armed = AtomicBool::new(false);
    let flush_timer = flush_timer(disk_state.clone(), &async_timer, &flush_timer_armed);
    pin_mut!(flush_timer);

    // Handle all the queues in one sub-select call.
    let queue_handlers = queues
        .into_iter()
        .map(|q| Arc::new(AsyncMutex::new(q)))
        .zip(
            queue_evts
                .into_iter()
                .map(|e| AsyncEventFd::try_from(e).unwrap())
                .map(|e| Arc::new(AsyncMutex::new(e))),
        )
        .flat_map(|(queue, event)| {
            // alias some refs so the lifetimes work.
            let mem = &mem;
            let timer = &async_timer;
            let timer_armed = &flush_timer_armed;
            let disk_state = &disk_state;
            let interrupt = &interrupt;
            std::iter::repeat_with(move || {
                handle_queue(
                    mem,
                    (*disk_state).clone(),
                    queue.clone(),
                    event.clone(),
                    timer,
                    timer_armed,
                    interrupt.clone(),
                )
            })
            .take(2)
        })
        .map(Box::pin)
        .collect::<FuturesUnordered<_>>()
        .into_future();

    // Handles control requests.
    let control = handle_command_socket(control_socket, interrupt.clone(), disk_state.clone());
    pin_mut!(control);

    // Process any requests to resample the irq value.
    let resample = handle_irq_resample(interrupt.clone());
    pin_mut!(resample);
    // Exit if the kill event is triggered.
    let kill = wait_kill(kill_evt);
    pin_mut!(kill);

    // And return once any future exits.
    let _ = select5(queue_handlers, control, flush_timer, resample, kill);

    Ok(())
}

/// Virtio device for exposing block level read/write operations on a host file.
pub struct Block {
    kill_evt: Option<EventFd>,
    worker_thread: Option<thread::JoinHandle<(Box<dyn DiskFile>, DiskControlResponseSocket)>>,
    disk_image: Option<Box<dyn DiskFile>>,
    disk_size: Arc<Mutex<u64>>,
    avail_features: u64,
    read_only: bool,
    sparse: bool,
    seg_max: u32,
    block_size: u32,
    control_socket: Option<DiskControlResponseSocket>,
}

fn build_config_space(disk_size: u64, seg_max: u32, block_size: u32) -> virtio_blk_config {
    virtio_blk_config {
        // If the image is not a multiple of the sector size, the tail bits are not exposed.
        capacity: Le64::from(disk_size >> SECTOR_SHIFT),
        seg_max: Le32::from(seg_max),
        blk_size: Le32::from(block_size),
        num_queues: Le16::from(NUM_QUEUES),
        max_discard_sectors: Le32::from(MAX_DISCARD_SECTORS),
        discard_sector_alignment: Le32::from(DISCARD_SECTOR_ALIGNMENT),
        max_write_zeroes_sectors: Le32::from(MAX_WRITE_ZEROES_SECTORS),
        write_zeroes_may_unmap: 1,
        max_discard_seg: Le32::from(MAX_DISCARD_SEG),
        max_write_zeroes_seg: Le32::from(MAX_WRITE_ZEROES_SEG),
        ..Default::default()
    }
}

impl Block {
    /// Create a new virtio block device that operates on the given DiskFile.
    pub fn new(
        disk_image: Box<dyn DiskFile>,
        read_only: bool,
        sparse: bool,
        block_size: u32,
        control_socket: Option<DiskControlResponseSocket>,
    ) -> SysResult<Block> {
        if block_size % SECTOR_SIZE as u32 != 0 {
            error!(
                "Block size {} is not a multiple of {}.",
                block_size, SECTOR_SIZE,
            );
            return Err(SysError::new(libc::EINVAL));
        }
        let disk_size = disk_image.get_len()?;
        if disk_size % block_size as u64 != 0 {
            warn!(
                "Disk size {} is not a multiple of block size {}; \
                 the remainder will not be visible to the guest.",
                disk_size, block_size,
            );
        }

        let mut avail_features: u64 = 1 << VIRTIO_BLK_F_FLUSH;
        avail_features |= 1 << VIRTIO_RING_F_EVENT_IDX;
        if read_only {
            avail_features |= 1 << VIRTIO_BLK_F_RO;
        } else {
            if sparse {
                avail_features |= 1 << VIRTIO_BLK_F_DISCARD;
            }
            avail_features |= 1 << VIRTIO_BLK_F_WRITE_ZEROES;
        }
        avail_features |= 1 << VIRTIO_F_VERSION_1;
        avail_features |= 1 << VIRTIO_BLK_F_SEG_MAX;
        avail_features |= 1 << VIRTIO_BLK_F_BLK_SIZE;
        avail_features |= 1 << VIRTIO_BLK_F_MQ;

        let seg_max = min(max(iov_max(), 1), u32::max_value() as usize) as u32;

        // Since we do not currently support indirect descriptors, the maximum
        // number of segments must be smaller than the queue size.
        // In addition, the request header and status each consume a descriptor.
        let seg_max = min(seg_max, u32::from(QUEUE_SIZE) - 2);

        Ok(Block {
            kill_evt: None,
            worker_thread: None,
            disk_image: Some(disk_image),
            disk_size: Arc::new(Mutex::new(disk_size)),
            avail_features,
            read_only,
            sparse,
            seg_max,
            block_size,
            control_socket,
        })
    }

    // Execute a single block device request.
    // `writer` includes the data region only; the status byte is not included.
    // It is up to the caller to convert the result of this function into a status byte
    // and write it to the expected location in guest memory.
    fn execute_request(
        reader: &mut Reader,
        writer: &mut Writer,
        read_only: bool,
        sparse: bool,
        disk: &mut dyn DiskFile,
        disk_size: u64,
        flush_timer: &TimerFd,
        flush_timer_armed: &AtomicBool,
    ) -> result::Result<(), ExecuteError> {
        let req_header: virtio_blk_req_header = reader.read_obj().map_err(ExecuteError::Read)?;

        let req_type = req_header.req_type.to_native();
        let sector = req_header.sector.to_native();
        // Delay after a write when the file is auto-flushed.
        let flush_delay = Duration::from_secs(60);

        if read_only && req_type != VIRTIO_BLK_T_IN {
            return Err(ExecuteError::ReadOnly {
                request_type: req_type,
            });
        }

        /// Check that a request accesses only data within the disk's current size.
        /// All parameters are in units of bytes.
        fn check_range(
            io_start: u64,
            io_length: u64,
            disk_size: u64,
        ) -> result::Result<(), ExecuteError> {
            let io_end = io_start
                .checked_add(io_length)
                .ok_or(ExecuteError::OutOfRange)?;
            if io_end > disk_size {
                Err(ExecuteError::OutOfRange)
            } else {
                Ok(())
            }
        }

        match req_type {
            VIRTIO_BLK_T_IN => {
                let data_len = writer.available_bytes();
                let offset = sector
                    .checked_shl(u32::from(SECTOR_SHIFT))
                    .ok_or(ExecuteError::OutOfRange)?;
                check_range(offset, data_len as u64, disk_size)?;
                writer
                    .write_all_from_at(disk, data_len, offset)
                    .map_err(|desc_error| ExecuteError::ReadIo {
                        length: data_len,
                        sector,
                        desc_error,
                    })?;
            }
            VIRTIO_BLK_T_OUT => {
                let data_len = reader.available_bytes();
                let offset = sector
                    .checked_shl(u32::from(SECTOR_SHIFT))
                    .ok_or(ExecuteError::OutOfRange)?;
                check_range(offset, data_len as u64, disk_size)?;
                reader
                    .read_exact_to_at(disk, data_len, offset)
                    .map_err(|desc_error| ExecuteError::WriteIo {
                        length: data_len,
                        sector,
                        desc_error,
                    })?;
                if !flush_timer_armed.load(Ordering::Relaxed) {
                    flush_timer
                        .reset(flush_delay, None)
                        .map_err(ExecuteError::TimerFd)?;
                    flush_timer_armed.store(true, Ordering::Relaxed);
                }
            }
            VIRTIO_BLK_T_DISCARD | VIRTIO_BLK_T_WRITE_ZEROES => {
                if req_type == VIRTIO_BLK_T_DISCARD && !sparse {
                    // Discard is a hint; if this is a non-sparse disk, just ignore it.
                    return Ok(());
                }

                while reader.available_bytes() >= size_of::<virtio_blk_discard_write_zeroes>() {
                    let seg: virtio_blk_discard_write_zeroes =
                        reader.read_obj().map_err(ExecuteError::Read)?;

                    let sector = seg.sector.to_native();
                    let num_sectors = seg.num_sectors.to_native();
                    let flags = seg.flags.to_native();

                    let valid_flags = if req_type == VIRTIO_BLK_T_WRITE_ZEROES {
                        VIRTIO_BLK_DISCARD_WRITE_ZEROES_FLAG_UNMAP
                    } else {
                        0
                    };

                    if (flags & !valid_flags) != 0 {
                        return Err(ExecuteError::DiscardWriteZeroes {
                            ioerr: None,
                            sector,
                            num_sectors,
                            flags,
                        });
                    }

                    let offset = sector
                        .checked_shl(u32::from(SECTOR_SHIFT))
                        .ok_or(ExecuteError::OutOfRange)?;
                    let length = u64::from(num_sectors)
                        .checked_shl(u32::from(SECTOR_SHIFT))
                        .ok_or(ExecuteError::OutOfRange)?;
                    check_range(offset, length, disk_size)?;

                    if req_type == VIRTIO_BLK_T_DISCARD {
                        // Since Discard is just a hint and some filesystems may not implement
                        // FALLOC_FL_PUNCH_HOLE, ignore punch_hole errors.
                        let _ = disk.punch_hole(offset, length);
                    } else {
                        disk.write_zeroes_all_at(offset, length as usize)
                            .map_err(|e| ExecuteError::DiscardWriteZeroes {
                                ioerr: Some(e),
                                sector,
                                num_sectors,
                                flags,
                            })?;
                    }
                }
            }
            VIRTIO_BLK_T_FLUSH => {
                disk.fsync().map_err(ExecuteError::Flush)?;
                flush_timer.clear().map_err(ExecuteError::TimerFd)?;
                flush_timer_armed.store(false, Ordering::Relaxed);
            }
            t => return Err(ExecuteError::Unsupported(t)),
        };
        Ok(())
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }

        if let Some(worker_thread) = self.worker_thread.take() {
            let _ = worker_thread.join();
        }
    }
}

impl VirtioDevice for Block {
    fn keep_fds(&self) -> Vec<RawFd> {
        let mut keep_fds = Vec::new();

        if let Some(disk_image) = &self.disk_image {
            keep_fds.extend(disk_image.as_raw_fds());
        }

        if let Some(control_socket) = &self.control_socket {
            keep_fds.push(control_socket.as_raw_fd());
        }

        keep_fds
    }

    fn features(&self) -> u64 {
        self.avail_features
    }

    fn device_type(&self) -> u32 {
        TYPE_BLOCK
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config_space = {
            let disk_size = self.disk_size.lock();
            build_config_space(*disk_size, self.seg_max, self.block_size)
        };
        copy_config(data, 0, config_space.as_slice(), offset);
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Interrupt,
        queues: Vec<Queue>,
        queue_evts: Vec<EventFd>,
    ) {
        let (self_kill_evt, kill_evt) = match EventFd::new().and_then(|e| Ok((e.try_clone()?, e))) {
            Ok(v) => v,
            Err(e) => {
                error!("failed creating kill EventFd pair: {}", e);
                return;
            }
        };
        self.kill_evt = Some(self_kill_evt);

        let read_only = self.read_only;
        let sparse = self.sparse;
        let disk_size = self.disk_size.clone();
        if let Some(disk_image) = self.disk_image.take() {
            if let Some(control_socket) = self.control_socket.take() {
                let worker_result =
                    thread::Builder::new()
                        .name("virtio_blk".to_string())
                        .spawn(move || {
                            let disk_state = Rc::new(RefCell::new(DiskState {
                                disk_image,
                                disk_size,
                                read_only,
                                sparse,
                            }));
                            if let Err(err_string) = run_worker(
                                interrupt,
                                queues,
                                mem,
                                &disk_state,
                                &control_socket,
                                queue_evts,
                                kill_evt,
                            ) {
                                error!("{}", err_string);
                            }

                            let disk_state = Rc::try_unwrap(disk_state).unwrap().into_inner();
                            (disk_state.disk_image, control_socket)
                        });

                match worker_result {
                    Err(e) => {
                        error!("failed to spawn virtio_blk worker: {}", e);
                        return;
                    }
                    Ok(join_handle) => {
                        self.worker_thread = Some(join_handle);
                    }
                }
            }
        }
    }

    fn reset(&mut self) -> bool {
        if let Some(kill_evt) = self.kill_evt.take() {
            if kill_evt.write(1).is_err() {
                error!("{}: failed to notify the kill event", self.debug_label());
                return false;
            }
        }

        if let Some(worker_thread) = self.worker_thread.take() {
            match worker_thread.join() {
                Err(_) => {
                    error!("{}: failed to get back resources", self.debug_label());
                    return false;
                }
                Ok((disk_image, control_socket)) => {
                    self.disk_image = Some(disk_image);
                    self.control_socket = Some(control_socket);
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::mem::size_of_val;
    use sys_util::GuestAddress;
    use tempfile::TempDir;

    use crate::virtio::descriptor_utils::{create_descriptor_chain, DescriptorType};

    use super::*;

    #[test]
    fn read_size() {
        let tempdir = TempDir::new().unwrap();
        let mut path = tempdir.path().to_owned();
        path.push("disk_image");
        let f = File::create(&path).unwrap();
        f.set_len(0x1000).unwrap();

        let b = Block::new(Box::new(f), true, false, 512, None).unwrap();
        let mut num_sectors = [0u8; 4];
        b.read_config(0, &mut num_sectors);
        // size is 0x1000, so num_sectors is 8 (4096/512).
        assert_eq!([0x08, 0x00, 0x00, 0x00], num_sectors);
        let mut msw_sectors = [0u8; 4];
        b.read_config(4, &mut msw_sectors);
        // size is 0x1000, so msw_sectors is 0.
        assert_eq!([0x00, 0x00, 0x00, 0x00], msw_sectors);
    }

    #[test]
    fn read_block_size() {
        let tempdir = TempDir::new().unwrap();
        let mut path = tempdir.path().to_owned();
        path.push("disk_image");
        let f = File::create(&path).unwrap();
        f.set_len(0x1000).unwrap();

        let b = Block::new(Box::new(f), true, false, 4096, None).unwrap();
        let mut blk_size = [0u8; 4];
        b.read_config(20, &mut blk_size);
        // blk_size should be 4096 (0x1000).
        assert_eq!([0x00, 0x10, 0x00, 0x00], blk_size);
    }

    #[test]
    fn read_features() {
        let tempdir = TempDir::new().unwrap();
        let mut path = tempdir.path().to_owned();
        path.push("disk_image");

        // read-write block device
        {
            let f = File::create(&path).unwrap();
            let b = Block::new(Box::new(f), false, true, 512, None).unwrap();
            // writable device should set VIRTIO_BLK_F_FLUSH + VIRTIO_BLK_F_DISCARD
            // + VIRTIO_BLK_F_WRITE_ZEROES + VIRTIO_F_VERSION_1 + VIRTIO_BLK_F_BLK_SIZE
            // + VIRTIO_BLK_F_SEG_MAX + VIRTIO_RING_F_EVENT_IDX + VIRTIO_BLK_F_MQ
            assert_eq!(0x120007244, b.features());
        }

        // read-write block device, non-sparse
        {
            let f = File::create(&path).unwrap();
            let b = Block::new(Box::new(f), false, false, 512, None).unwrap();
            // writable device should set VIRTIO_BLK_F_FLUSH
            // + VIRTIO_BLK_F_WRITE_ZEROES + VIRTIO_F_VERSION_1 + VIRTIO_BLK_F_BLK_SIZE
            // + VIRTIO_BLK_F_SEG_MAX + VIRTIO_RING_F_EVENT_IDX + VIRTIO_BLK_F_MQ
            assert_eq!(0x120005244, b.features());
        }

        // read-only block device
        {
            let f = File::create(&path).unwrap();
            let b = Block::new(Box::new(f), true, true, 512, None).unwrap();
            // read-only device should set VIRTIO_BLK_F_FLUSH and VIRTIO_BLK_F_RO
            // + VIRTIO_F_VERSION_1 + VIRTIO_BLK_F_BLK_SIZE + VIRTIO_BLK_F_SEG_MAX
            // + VIRTIO_RING_F_EVENT_IDX + VIRTIO_BLK_F_MQ
            assert_eq!(0x120001264, b.features());
        }
    }

    #[test]
    fn read_last_sector() {
        let tempdir = TempDir::new().unwrap();
        let mut path = tempdir.path().to_owned();
        path.push("disk_image");
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .unwrap();
        let disk_size = 0x1000;
        f.set_len(disk_size).unwrap();

        let mem = GuestMemory::new(&[(GuestAddress(0u64), 4 * 1024 * 1024)])
            .expect("Creating guest memory failed.");

        let req_hdr = virtio_blk_req_header {
            req_type: Le32::from(VIRTIO_BLK_T_IN),
            reserved: Le32::from(0),
            sector: Le64::from(7), // Disk is 8 sectors long, so this is the last valid sector.
        };
        mem.write_obj_at_addr(req_hdr, GuestAddress(0x1000))
            .expect("writing req failed");

        let avail_desc = create_descriptor_chain(
            &mem,
            GuestAddress(0x100),  // Place descriptor chain at 0x100.
            GuestAddress(0x1000), // Describe buffer at 0x1000.
            vec![
                // Request header
                (DescriptorType::Readable, size_of_val(&req_hdr) as u32),
                // I/O buffer (1 sector of data)
                (DescriptorType::Writable, 512),
                // Request status
                (DescriptorType::Writable, 1),
            ],
            0,
        )
        .expect("create_descriptor_chain failed");

        let flush_timer = TimerFd::new().expect("failed to create flush_timer");
        let flush_timer_armed = AtomicBool::new(false);

        process_one_request(
            avail_desc,
            false,
            true,
            &mut f,
            disk_size,
            &flush_timer,
            &flush_timer_armed,
            &mem,
        )
        .expect("execute failed");

        let status_offset = GuestAddress((0x1000 + size_of_val(&req_hdr) + 512) as u64);
        let status = mem.read_obj_from_addr::<u8>(status_offset).unwrap();
        assert_eq!(status, VIRTIO_BLK_S_OK);
    }

    #[test]
    fn read_beyond_last_sector() {
        let tempdir = TempDir::new().unwrap();
        let mut path = tempdir.path().to_owned();
        path.push("disk_image");
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .unwrap();
        let disk_size = 0x1000;
        f.set_len(disk_size).unwrap();

        let mem = GuestMemory::new(&[(GuestAddress(0u64), 4 * 1024 * 1024)])
            .expect("Creating guest memory failed.");

        let req_hdr = virtio_blk_req_header {
            req_type: Le32::from(VIRTIO_BLK_T_IN),
            reserved: Le32::from(0),
            sector: Le64::from(7), // Disk is 8 sectors long, so this is the last valid sector.
        };
        mem.write_obj_at_addr(req_hdr, GuestAddress(0x1000))
            .expect("writing req failed");

        let avail_desc = create_descriptor_chain(
            &mem,
            GuestAddress(0x100),  // Place descriptor chain at 0x100.
            GuestAddress(0x1000), // Describe buffer at 0x1000.
            vec![
                // Request header
                (DescriptorType::Readable, size_of_val(&req_hdr) as u32),
                // I/O buffer (2 sectors of data - overlap the end of the disk).
                (DescriptorType::Writable, 512 * 2),
                // Request status
                (DescriptorType::Writable, 1),
            ],
            0,
        )
        .expect("create_descriptor_chain failed");

        let flush_timer = TimerFd::new().expect("failed to create flush_timer");
        let flush_timer_armed = AtomicBool::new(false);

        process_one_request(
            avail_desc,
            false,
            true,
            &mut f,
            disk_size,
            &flush_timer,
            &flush_timer_armed,
            &mem,
        )
        .expect("execute failed");

        let status_offset = GuestAddress((0x1000 + size_of_val(&req_hdr) + 512 * 2) as u64);
        let status = mem.read_obj_from_addr::<u8>(status_offset).unwrap();
        assert_eq!(status, VIRTIO_BLK_S_IOERR);
    }
}

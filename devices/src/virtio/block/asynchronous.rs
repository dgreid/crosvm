// Copyright 2021 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cell::RefCell;
use std::cmp::{max, min};
use std::io::{self, Write};
use std::mem::size_of;
use std::rc::Rc;
use std::result;
use std::sync::{atomic::AtomicU64, atomic::Ordering, Arc};
use std::thread;
use std::time::Duration;
use std::u32;

use futures::pin_mut;
use futures::stream::{FuturesUnordered, StreamExt};
use remain::sorted;
use thiserror::Error as ThisError;

use base::Error as SysError;
use base::Result as SysResult;
use base::{
    error, info, iov_max, warn, AsRawDescriptor, AsyncTube, Event, RawDescriptor, Timer, Tube,
    TubeError,
};
use cros_async::{
    select5, sync::Mutex as AsyncMutex, AsyncError, EventAsync, Executor, SelectResult, TimerAsync,
};
use data_model::DataInit;
use disk::{AsyncDisk, ToAsyncDisk};
use vm_control::{DiskControlCommand, DiskControlResult};
use vm_memory::GuestMemory;

use super::common::*;
use crate::virtio::{
    copy_config, DescriptorChain, DescriptorError, Interrupt, Queue, Reader, SignalableInterrupt,
    VirtioDevice, Writer, TYPE_BLOCK,
};

const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: u16 = 16;
const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE; NUM_QUEUES as usize];

#[sorted]
#[derive(ThisError, Debug)]
enum ExecuteError {
    #[error("failed to copy ID string: {0}")]
    CopyId(io::Error),
    #[error("virtio descriptor error: {0}")]
    Descriptor(DescriptorError),
    #[error("failed to perform discard or write zeroes; sector={sector} num_sectors={num_sectors} flags={flags}; {ioerr:?}")]
    DiscardWriteZeroes {
        ioerr: Option<disk::Error>,
        sector: u64,
        num_sectors: u32,
        flags: u32,
    },
    #[error("failed to flush: {0}")]
    Flush(disk::Error),
    #[error("not enough space in descriptor chain to write status")]
    MissingStatus,
    #[error("out of range")]
    OutOfRange,
    #[error("failed to read message: {0}")]
    Read(io::Error),
    #[error("io error reading {length} bytes from sector {sector}: {desc_error}")]
    ReadIo {
        length: usize,
        sector: u64,
        desc_error: disk::Error,
    },
    #[error("read only; request_type={request_type}")]
    ReadOnly { request_type: u32 },
    #[error("failed to recieve command message: {0}")]
    ReceivingCommand(TubeError),
    #[error("failed to send command response: {0}")]
    SendingResponse(TubeError),
    #[error("couldn't reset the timer: {0}")]
    TimerReset(base::Error),
    #[error("unsupported ({0})")]
    Unsupported(u32),
    #[error("io error writing {length} bytes from sector {sector}: {desc_error}")]
    WriteIo {
        length: usize,
        sector: u64,
        desc_error: disk::Error,
    },
    #[error("failed to write request status: {0}")]
    WriteStatus(io::Error),
}

impl ExecuteError {
    fn status(&self) -> u8 {
        match self {
            ExecuteError::CopyId(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Descriptor(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::DiscardWriteZeroes { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::Flush(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::MissingStatus => VIRTIO_BLK_S_IOERR,
            ExecuteError::OutOfRange { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::Read(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::ReadIo { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::ReadOnly { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::ReceivingCommand(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::SendingResponse(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::TimerReset(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::WriteIo { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::WriteStatus(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Unsupported(_) => VIRTIO_BLK_S_UNSUPP,
        }
    }
}

// Errors that happen in block outside of executing a request.
#[derive(ThisError, Debug)]
enum OtherError {
    #[error("couldn't create an async resample event: {0}")]
    AsyncResampleCreate(AsyncError),
    #[error("couldn't clone the resample event: {0}")]
    CloneResampleEvent(base::Error),
    #[error("couldn't get a value from a timer for flushing: {0}")]
    FlushTimer(AsyncError),
    #[error("failed to fsync the disk: {0}")]
    FsyncDisk(disk::Error),
    #[error("couldn't read the resample event: {0}")]
    ReadResampleEvent(AsyncError),
}

/// Tracks the state of an anynchronous disk.
pub struct DiskState {
    pub disk_image: Box<dyn AsyncDisk>,
    pub disk_size: Arc<AtomicU64>,
    pub read_only: bool,
    pub sparse: bool,
    pub id: Option<BlockId>,
}

impl DiskState {
    /// Creates a `DiskState` with the given params.
    pub fn new(
        disk_image: Box<dyn AsyncDisk>,
        disk_size: Arc<AtomicU64>,
        read_only: bool,
        sparse: bool,
        id: Option<BlockId>,
    ) -> DiskState {
        DiskState {
            disk_image,
            disk_size,
            read_only,
            sparse,
            id,
        }
    }
}

async fn process_one_request(
    avail_desc: DescriptorChain,
    disk_state: Rc<AsyncMutex<DiskState>>,
    flush_timer: Rc<RefCell<TimerAsync>>,
    flush_timer_armed: Rc<RefCell<bool>>,
    mem: &GuestMemory,
) -> result::Result<usize, ExecuteError> {
    let mut reader =
        Reader::new(mem.clone(), avail_desc.clone()).map_err(ExecuteError::Descriptor)?;
    let mut writer = Writer::new(mem.clone(), avail_desc).map_err(ExecuteError::Descriptor)?;

    // The last byte of the buffer is virtio_blk_req::status.
    // Split it into a separate Writer so that status_writer is the final byte and
    // the original writer is left with just the actual block I/O data.
    let available_bytes = writer.available_bytes();
    let status_offset = available_bytes
        .checked_sub(1)
        .ok_or(ExecuteError::MissingStatus)?;
    let mut status_writer = writer.split_at(status_offset);

    let status = match BlockAsync::execute_request(
        &mut reader,
        &mut writer,
        disk_state,
        flush_timer,
        flush_timer_armed,
    )
    .await
    {
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

/// Process one descriptor chain asynchronously.
pub async fn process_one_chain<I: SignalableInterrupt>(
    queue: Rc<RefCell<Queue>>,
    avail_desc: DescriptorChain,
    disk_state: Rc<AsyncMutex<DiskState>>,
    mem: GuestMemory,
    interrupt: &I,
    flush_timer: Rc<RefCell<TimerAsync>>,
    flush_timer_armed: Rc<RefCell<bool>>,
) {
    let descriptor_index = avail_desc.index;
    let len =
        match process_one_request(avail_desc, disk_state, flush_timer, flush_timer_armed, &mem)
            .await
        {
            Ok(len) => len,
            Err(e) => {
                error!("block: failed to handle request: {}", e);
                0
            }
        };

    let mut queue = queue.borrow_mut();
    queue.add_used(&mem, descriptor_index, len as u32);
    queue.trigger_interrupt(&mem, interrupt);
    queue.update_int_required(&mem);
}

// There is one async task running `handle_queue` per virtio queue in use.
// Receives messages from the guest and queues a task to complete the operations with the async
// executor.
async fn handle_queue(
    ex: &Executor,
    mem: &GuestMemory,
    disk_state: Rc<AsyncMutex<DiskState>>,
    queue: Rc<RefCell<Queue>>,
    evt: EventAsync,
    interrupt: Rc<RefCell<Interrupt>>,
    flush_timer: Rc<RefCell<TimerAsync>>,
    flush_timer_armed: Rc<RefCell<bool>>,
) {
    loop {
        if let Err(e) = evt.next_val().await {
            error!("Failed to read the next queue event: {}", e);
            continue;
        }
        while let Some(descriptor_chain) = queue.borrow_mut().pop(&mem) {
            let queue = Rc::clone(&queue);
            let disk_state = Rc::clone(&disk_state);
            let mem = mem.clone();
            let interrupt = Rc::clone(&interrupt);
            let flush_timer = Rc::clone(&flush_timer);
            let flush_timer_armed = Rc::clone(&flush_timer_armed);

            ex.spawn_local(async move {
                process_one_chain(
                    queue,
                    descriptor_chain,
                    disk_state,
                    mem,
                    &*interrupt.borrow(),
                    flush_timer,
                    flush_timer_armed,
                )
                .await
            })
            .detach();
        }
    }
}

async fn handle_irq_resample(
    ex: &Executor,
    interrupt: Rc<RefCell<Interrupt>>,
) -> result::Result<(), OtherError> {
    let resample_evt = if let Some(resample_evt) = interrupt.borrow().get_resample_evt() {
        let resample_evt = resample_evt
            .try_clone()
            .map_err(OtherError::CloneResampleEvent)?;
        let resample_evt =
            EventAsync::new(resample_evt.0, ex).map_err(OtherError::AsyncResampleCreate)?;
        Some(resample_evt)
    } else {
        None
    };
    if let Some(resample_evt) = resample_evt {
        loop {
            let _ = resample_evt
                .next_val()
                .await
                .map_err(OtherError::ReadResampleEvent)?;
            interrupt.borrow().do_interrupt_resample();
        }
    } else {
        // no resample event, park the future.
        let () = futures::future::pending().await;
        Ok(())
    }
}

async fn wait_kill(kill_evt: EventAsync) {
    // Once this event is readable, exit. Exiting this future will cause the main loop to
    // break and the device process to exit.
    let _ = kill_evt.next_val().await;
}

async fn handle_command_tube(
    command_tube: &Option<AsyncTube>,
    interrupt: Rc<RefCell<Interrupt>>,
    disk_state: Rc<AsyncMutex<DiskState>>,
) -> Result<(), ExecuteError> {
    let command_tube = match command_tube {
        Some(c) => c,
        None => {
            let () = futures::future::pending().await;
            return Ok(());
        }
    };
    loop {
        match command_tube.next().await {
            Ok(command) => {
                let resp = match command {
                    DiskControlCommand::Resize { new_size } => {
                        resize(Rc::clone(&disk_state), new_size).await
                    }
                };

                command_tube
                    .send(&resp)
                    .map_err(ExecuteError::SendingResponse)?;
                if let DiskControlResult::Ok = resp {
                    interrupt.borrow_mut().signal_config_changed();
                }
            }
            Err(e) => return Err(ExecuteError::ReceivingCommand(e)),
        }
    }
}

async fn resize(disk_state: Rc<AsyncMutex<DiskState>>, new_size: u64) -> DiskControlResult {
    // Acquire exclusive, mutable access to the state so the virtqueue task won't be able to read
    // the state while resizing.
    let mut disk_state = disk_state.lock().await;

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
        disk_state.disk_size.store(new_disk_size, Ordering::Release);
    }
    DiskControlResult::Ok
}

async fn flush_disk(
    disk_state: Rc<AsyncMutex<DiskState>>,
    timer: TimerAsync,
    armed: Rc<RefCell<bool>>,
) -> Result<(), OtherError> {
    loop {
        timer.next_val().await.map_err(OtherError::FlushTimer)?;
        if !*armed.borrow() {
            continue;
        }

        // Reset armed before calling fsync to guarantee that IO requests that started after we call
        // fsync will be committed eventually.
        *armed.borrow_mut() = false;

        disk_state
            .read_lock()
            .await
            .disk_image
            .fsync()
            .await
            .map_err(OtherError::FsyncDisk)?;
    }
}

// The main worker thread. Initialized the asynchronous worker tasks and passes them to the executor
// to be processed.
//
// `disk_state` is wrapped by `AsyncMutex`, which provides both shared and exclusive locks. It's
// because the state can be read from the virtqueue task while the control task is processing
// a resizing command.
fn run_worker(
    ex: Executor,
    interrupt: Interrupt,
    queues: Vec<Queue>,
    mem: GuestMemory,
    disk_state: &Rc<AsyncMutex<DiskState>>,
    control_tube: &Option<AsyncTube>,
    queue_evts: Vec<Event>,
    kill_evt: Event,
) -> Result<(), String> {
    if queues.len() != queue_evts.len() {
        return Err("Number of queues and events must match.".to_string());
    }

    let interrupt = Rc::new(RefCell::new(interrupt));

    // One flush timer per disk.
    let timer = Timer::new().expect("Failed to create a timer");
    let flush_timer_armed = Rc::new(RefCell::new(false));

    // Process any requests to resample the irq value.
    let resample = handle_irq_resample(&ex, Rc::clone(&interrupt));
    pin_mut!(resample);

    // Handles control requests.
    let control = handle_command_tube(control_tube, Rc::clone(&interrupt), disk_state.clone());
    pin_mut!(control);

    // Handle all the queues in one sub-select call.
    let flush_timer = Rc::new(RefCell::new(
        TimerAsync::new(
            // Call try_clone() to share the same underlying FD with the `flush_disk` task.
            timer.0.try_clone().expect("Failed to clone flush_timer"),
            &ex,
        )
        .expect("Failed to create an async timer"),
    ));

    let queue_handlers =
        queues
            .into_iter()
            .map(|q| Rc::new(RefCell::new(q)))
            .zip(queue_evts.into_iter().map(|e| {
                EventAsync::new(e.0, &ex).expect("Failed to create async event for queue")
            }))
            .map(|(queue, event)| {
                // alias some refs so the lifetimes work.
                let mem = &mem;
                let disk_state = &disk_state;
                let interrupt = &interrupt;
                handle_queue(
                    &ex,
                    mem,
                    Rc::clone(&disk_state),
                    Rc::clone(&queue),
                    event,
                    Rc::clone(&interrupt),
                    Rc::clone(&flush_timer),
                    Rc::clone(&flush_timer_armed),
                )
            })
            .collect::<FuturesUnordered<_>>()
            .into_future();

    // Flushes the disk periodically.
    let flush_timer = TimerAsync::new(timer.0, &ex).expect("Failed to create an async timer");
    let disk_flush = flush_disk(disk_state.clone(), flush_timer, flush_timer_armed.clone());
    pin_mut!(disk_flush);

    // Exit if the kill event is triggered.
    let kill_evt = EventAsync::new(kill_evt.0, &ex).expect("Failed to create async kill event fd");
    let kill = wait_kill(kill_evt);
    pin_mut!(kill);

    match ex.run_until(select5(queue_handlers, disk_flush, control, resample, kill)) {
        Ok((_, flush_res, control_res, resample_res, _)) => {
            if let SelectResult::Finished(Err(e)) = flush_res {
                return Err(format!("failed to flush a disk: {}", e));
            }
            if let SelectResult::Finished(Err(e)) = control_res {
                return Err(format!("failed to handle a control request: {}", e));
            }
            if let SelectResult::Finished(Err(e)) = resample_res {
                return Err(format!("failed to resample a irq value: {:?}", e));
            }
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Virtio device for exposing block level read/write operations on a host file.
pub struct BlockAsync {
    kill_evt: Option<Event>,
    worker_thread: Option<thread::JoinHandle<(Box<dyn ToAsyncDisk>, Option<Tube>)>>,
    disk_image: Option<Box<dyn ToAsyncDisk>>,
    disk_size: Arc<AtomicU64>,
    avail_features: u64,
    read_only: bool,
    sparse: bool,
    seg_max: u32,
    block_size: u32,
    id: Option<BlockId>,
    control_tube: Option<Tube>,
}

impl BlockAsync {
    /// Create a new virtio block device that operates on the given AsyncDisk.
    pub fn new(
        base_features: u64,
        disk_image: Box<dyn ToAsyncDisk>,
        read_only: bool,
        sparse: bool,
        block_size: u32,
        id: Option<BlockId>,
        control_tube: Option<Tube>,
    ) -> SysResult<BlockAsync> {
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

        let avail_features = build_avail_features(base_features, read_only, sparse, true);

        let seg_max = min(max(iov_max(), 1), u32::max_value() as usize) as u32;

        // Since we do not currently support indirect descriptors, the maximum
        // number of segments must be smaller than the queue size.
        // In addition, the request header and status each consume a descriptor.
        let seg_max = min(seg_max, u32::from(QUEUE_SIZE) - 2);

        Ok(BlockAsync {
            kill_evt: None,
            worker_thread: None,
            disk_image: Some(disk_image),
            disk_size: Arc::new(AtomicU64::new(disk_size)),
            avail_features,
            read_only,
            sparse,
            seg_max,
            block_size,
            id,
            control_tube,
        })
    }

    // Execute a single block device request.
    // `writer` includes the data region only; the status byte is not included.
    // It is up to the caller to convert the result of this function into a status byte
    // and write it to the expected location in guest memory.
    async fn execute_request(
        reader: &mut Reader,
        writer: &mut Writer,
        disk_state: Rc<AsyncMutex<DiskState>>,
        flush_timer: Rc<RefCell<TimerAsync>>,
        flush_timer_armed: Rc<RefCell<bool>>,
    ) -> result::Result<(), ExecuteError> {
        // Acquire immutable access to disk_state to prevent the disk from being resized.
        let disk_state = disk_state.read_lock().await;

        let req_header: virtio_blk_req_header = reader.read_obj().map_err(ExecuteError::Read)?;

        let req_type = req_header.req_type.to_native();
        let sector = req_header.sector.to_native();

        if disk_state.read_only && req_type != VIRTIO_BLK_T_IN && req_type != VIRTIO_BLK_T_GET_ID {
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

        let disk_size = disk_state.disk_size.load(Ordering::Relaxed);
        match req_type {
            VIRTIO_BLK_T_IN => {
                let data_len = writer.available_bytes();
                if data_len == 0 {
                    return Ok(());
                }
                let offset = sector
                    .checked_shl(u32::from(SECTOR_SHIFT))
                    .ok_or(ExecuteError::OutOfRange)?;
                check_range(offset, data_len as u64, disk_size)?;
                let disk_image = &disk_state.disk_image;
                writer
                    .write_all_from_at_fut(&**disk_image, data_len, offset)
                    .await
                    .map_err(|desc_error| ExecuteError::ReadIo {
                        length: data_len,
                        sector,
                        desc_error,
                    })?;
            }
            VIRTIO_BLK_T_OUT => {
                let data_len = reader.available_bytes();
                if data_len == 0 {
                    return Ok(());
                }
                let offset = sector
                    .checked_shl(u32::from(SECTOR_SHIFT))
                    .ok_or(ExecuteError::OutOfRange)?;
                check_range(offset, data_len as u64, disk_size)?;
                let disk_image = &disk_state.disk_image;
                reader
                    .read_exact_to_at_fut(&**disk_image, data_len, offset)
                    .await
                    .map_err(|desc_error| ExecuteError::WriteIo {
                        length: data_len,
                        sector,
                        desc_error,
                    })?;

                if !*flush_timer_armed.borrow() {
                    *flush_timer_armed.borrow_mut() = true;

                    let flush_delay = Duration::from_secs(60);
                    flush_timer
                        .borrow_mut()
                        .reset(flush_delay, None)
                        .map_err(ExecuteError::TimerReset)?;
                }
            }
            VIRTIO_BLK_T_DISCARD | VIRTIO_BLK_T_WRITE_ZEROES => {
                if req_type == VIRTIO_BLK_T_DISCARD && !disk_state.sparse {
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
                        let _ = disk_state.disk_image.punch_hole(offset, length).await;
                    } else {
                        disk_state
                            .disk_image
                            .write_zeroes_at(offset, length)
                            .await
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
                disk_state
                    .disk_image
                    .fsync()
                    .await
                    .map_err(ExecuteError::Flush)?;
            }
            VIRTIO_BLK_T_GET_ID => {
                if let Some(id) = disk_state.id {
                    writer.write_all(&id).map_err(ExecuteError::CopyId)?;
                } else {
                    return Err(ExecuteError::Unsupported(req_type));
                }
            }
            t => return Err(ExecuteError::Unsupported(t)),
        };
        Ok(())
    }
}

impl Drop for BlockAsync {
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

impl VirtioDevice for BlockAsync {
    fn keep_rds(&self) -> Vec<RawDescriptor> {
        let mut keep_rds = Vec::new();

        if let Some(disk_image) = &self.disk_image {
            keep_rds.extend(disk_image.as_raw_descriptors());
        }

        if let Some(control_tube) = &self.control_tube {
            keep_rds.push(control_tube.as_raw_descriptor());
        }

        keep_rds
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
            let disk_size = self.disk_size.load(Ordering::Acquire);
            build_config_space(disk_size, self.seg_max, self.block_size, NUM_QUEUES)
        };
        copy_config(data, 0, config_space.as_slice(), offset);
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Interrupt,
        queues: Vec<Queue>,
        queue_evts: Vec<Event>,
    ) {
        let (self_kill_evt, kill_evt) = match Event::new().and_then(|e| Ok((e.try_clone()?, e))) {
            Ok(v) => v,
            Err(e) => {
                error!("failed creating kill Event pair: {}", e);
                return;
            }
        };
        self.kill_evt = Some(self_kill_evt);

        let read_only = self.read_only;
        let sparse = self.sparse;
        let disk_size = self.disk_size.clone();
        let id = self.id.take();
        if let Some(disk_image) = self.disk_image.take() {
            let control_tube = self.control_tube.take();
            let worker_result =
                thread::Builder::new()
                    .name("virtio_blk".to_string())
                    .spawn(move || {
                        let ex = Executor::new().expect("Failed to create an executor");

                        let async_control = control_tube
                            .map(|c| c.into_async_tube(&ex).expect("failed to create async tube"));
                        let async_image = match disk_image.to_async_disk(&ex) {
                            Ok(d) => d,
                            Err(e) => panic!("Failed to create async disk {}", e),
                        };
                        let disk_state = Rc::new(AsyncMutex::new(DiskState {
                            disk_image: async_image,
                            disk_size,
                            read_only,
                            sparse,
                            id,
                        }));
                        if let Err(err_string) = run_worker(
                            ex,
                            interrupt,
                            queues,
                            mem,
                            &disk_state,
                            &async_control,
                            queue_evts,
                            kill_evt,
                        ) {
                            error!("{}", err_string);
                        }

                        let disk_state = match Rc::try_unwrap(disk_state) {
                            Ok(d) => d.into_inner(),
                            Err(_) => panic!("too many refs to the disk"),
                        };
                        (
                            disk_state.disk_image.into_inner(),
                            async_control.map(|c| c.into()),
                        )
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
                Ok((disk_image, control_tube)) => {
                    self.disk_image = Some(disk_image);
                    self.control_tube = control_tube;
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
    use std::sync::atomic::AtomicU64;

    use data_model::{Le32, Le64};
    use disk::SingleFileDisk;
    use tempfile::TempDir;
    use vm_memory::GuestAddress;

    use crate::virtio::base_features;
    use crate::virtio::block::common::*;
    use crate::virtio::descriptor_utils::{create_descriptor_chain, DescriptorType};
    use crate::ProtectionType;

    use super::*;

    #[test]
    fn read_size() {
        let tempdir = TempDir::new().unwrap();
        let mut path = tempdir.path().to_owned();
        path.push("disk_image");
        let f = File::create(&path).unwrap();
        f.set_len(0x1000).unwrap();

        let features = base_features(ProtectionType::Unprotected);
        let b = BlockAsync::new(features, Box::new(f), true, false, 512, None, None).unwrap();
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

        let features = base_features(ProtectionType::Unprotected);
        let b = BlockAsync::new(features, Box::new(f), true, false, 4096, None, None).unwrap();
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
            let features = base_features(ProtectionType::Unprotected);
            let b = BlockAsync::new(features, Box::new(f), false, true, 512, None, None).unwrap();
            // writable device should set VIRTIO_BLK_F_FLUSH + VIRTIO_BLK_F_DISCARD
            // + VIRTIO_BLK_F_WRITE_ZEROES + VIRTIO_F_VERSION_1 + VIRTIO_BLK_F_BLK_SIZE
            // + VIRTIO_BLK_F_SEG_MAX + VIRTIO_BLK_F_MQ
            assert_eq!(0x100007244, b.features());
        }

        // read-write block device, non-sparse
        {
            let f = File::create(&path).unwrap();
            let features = base_features(ProtectionType::Unprotected);
            let b = BlockAsync::new(features, Box::new(f), false, false, 512, None, None).unwrap();
            // read-only device should set VIRTIO_BLK_F_FLUSH and VIRTIO_BLK_F_RO
            // + VIRTIO_F_VERSION_1 + VIRTIO_BLK_F_BLK_SIZE + VIRTIO_BLK_F_SEG_MAX
            // + VIRTIO_BLK_F_MQ
            assert_eq!(0x100005244, b.features());
        }

        // read-only block device
        {
            let f = File::create(&path).unwrap();
            let features = base_features(ProtectionType::Unprotected);
            let b = BlockAsync::new(features, Box::new(f), true, true, 512, None, None).unwrap();
            // read-only device should set VIRTIO_BLK_F_FLUSH and VIRTIO_BLK_F_RO
            // + VIRTIO_F_VERSION_1 + VIRTIO_BLK_F_BLK_SIZE + VIRTIO_BLK_F_SEG_MAX
            // + VIRTIO_BLK_F_MQ
            assert_eq!(0x100001264, b.features());
        }
    }

    #[test]
    fn read_last_sector() {
        let ex = Executor::new().expect("creating an executor failed");

        let tempdir = TempDir::new().unwrap();
        let mut path = tempdir.path().to_owned();
        path.push("disk_image");
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .unwrap();
        let disk_size = 0x1000;
        f.set_len(disk_size).unwrap();
        let af = SingleFileDisk::new(f, &ex).expect("Failed to create SFD");

        let mem = Rc::new(
            GuestMemory::new(&[(GuestAddress(0u64), 4 * 1024 * 1024)])
                .expect("Creating guest memory failed."),
        );

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

        let timer = Timer::new().expect("Failed to create a timer");
        let flush_timer = Rc::new(RefCell::new(
            TimerAsync::new(timer.0, &ex).expect("Failed to create an async timer"),
        ));
        let flush_timer_armed = Rc::new(RefCell::new(false));

        let disk_state = Rc::new(AsyncMutex::new(DiskState {
            disk_image: Box::new(af),
            disk_size: Arc::new(AtomicU64::new(disk_size)),
            read_only: false,
            sparse: true,
            id: None,
        }));

        let fut = process_one_request(avail_desc, disk_state, flush_timer, flush_timer_armed, &mem);

        ex.run_until(fut)
            .expect("running executor failed")
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
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .unwrap();
        let disk_size = 0x1000;
        f.set_len(disk_size).unwrap();
        let mem = Rc::new(
            GuestMemory::new(&[(GuestAddress(0u64), 4 * 1024 * 1024)])
                .expect("Creating guest memory failed."),
        );

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

        let ex = Executor::new().expect("creating an executor failed");

        let af = SingleFileDisk::new(f, &ex).expect("Failed to create SFD");
        let timer = Timer::new().expect("Failed to create a timer");
        let flush_timer = Rc::new(RefCell::new(
            TimerAsync::new(timer.0, &ex).expect("Failed to create an async timer"),
        ));
        let flush_timer_armed = Rc::new(RefCell::new(false));
        let disk_state = Rc::new(AsyncMutex::new(DiskState {
            disk_image: Box::new(af),
            disk_size: Arc::new(AtomicU64::new(disk_size)),
            read_only: false,
            sparse: true,
            id: None,
        }));

        let fut = process_one_request(avail_desc, disk_state, flush_timer, flush_timer_armed, &mem);

        ex.run_until(fut)
            .expect("running executor failed")
            .expect("execute failed");

        let status_offset = GuestAddress((0x1000 + size_of_val(&req_hdr) + 512 * 2) as u64);
        let status = mem.read_obj_from_addr::<u8>(status_offset).unwrap();
        assert_eq!(status, VIRTIO_BLK_S_IOERR);
    }

    #[test]
    fn get_id() {
        let ex = Executor::new().expect("creating an executor failed");

        let tempdir = TempDir::new().unwrap();
        let mut path = tempdir.path().to_owned();
        path.push("disk_image");
        let f = OpenOptions::new()
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
            req_type: Le32::from(VIRTIO_BLK_T_GET_ID),
            reserved: Le32::from(0),
            sector: Le64::from(0),
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
                // I/O buffer (20 bytes for serial)
                (DescriptorType::Writable, 20),
                // Request status
                (DescriptorType::Writable, 1),
            ],
            0,
        )
        .expect("create_descriptor_chain failed");

        let af = SingleFileDisk::new(f, &ex).expect("Failed to create SFD");
        let timer = Timer::new().expect("Failed to create a timer");
        let flush_timer = Rc::new(RefCell::new(
            TimerAsync::new(timer.0, &ex).expect("Failed to create an async timer"),
        ));
        let flush_timer_armed = Rc::new(RefCell::new(false));

        let id = b"a20-byteserialnumber";

        let disk_state = Rc::new(AsyncMutex::new(DiskState {
            disk_image: Box::new(af),
            disk_size: Arc::new(AtomicU64::new(disk_size)),
            read_only: false,
            sparse: true,
            id: Some(*id),
        }));

        let fut = process_one_request(avail_desc, disk_state, flush_timer, flush_timer_armed, &mem);

        ex.run_until(fut)
            .expect("running executor failed")
            .expect("execute failed");

        let status_offset = GuestAddress((0x1000 + size_of_val(&req_hdr) + 512) as u64);
        let status = mem.read_obj_from_addr::<u8>(status_offset).unwrap();
        assert_eq!(status, VIRTIO_BLK_S_OK);

        let id_offset = GuestAddress(0x1000 + size_of_val(&req_hdr) as u64);
        let returned_id = mem.read_obj_from_addr::<[u8; 20]>(id_offset).unwrap();
        assert_eq!(returned_id, *id);
    }
}

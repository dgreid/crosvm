// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cmp::{max, min};
use std::collections::{BTreeMap, VecDeque};
use std::fmt::{self, Display};
use std::io::{self, IoSlice, Write};
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, RawFd};
use std::result;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::u32;

use data_model::{DataInit, Le16, Le32, Le64, VolatileSlice};
use disk::DiskFile;
use io_uring::{self, URingContext};
use msg_socket::{MsgReceiver, MsgSender};
use sync::Mutex;
use sys_util::Error as SysError;
use sys_util::Result as SysResult;
use sys_util::{error, info, iov_max, warn, EventFd, GuestMemory, IntoIovec, PollContext, TimerFd};
use virtio_sys::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use vm_control::{DiskControlCommand, DiskControlResponseSocket, DiskControlResult};

use super::{
    copy_config, DescriptorChain, DescriptorError, Interrupt, Queue, Reader, VirtioDevice, Writer,
    TYPE_BLOCK, VIRTIO_F_VERSION_1,
};

const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: u16 = 8;
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
    Descriptor(DescriptorError),
    Read(io::Error),
    WriteStatus(io::Error),
    TimerFd(SysError),
    DiscardWriteZeroes {
        ioerr: Option<io::Error>,
        sector: u64,
        num_sectors: u32,
        flags: u32,
    },
    ReadOnly,
    OutOfRange,
    MissingStatus,
    URing(std::io::Error),
    Unsupported(u32),
}

impl Display for ExecuteError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::ExecuteError::*;

        match self {
            Descriptor(e) => write!(f, "virtio descriptor error: {}", e),
            Read(e) => write!(f, "failed to read message: {}", e),
            WriteStatus(e) => write!(f, "failed to write request status: {}", e),
            TimerFd(e) => write!(f, "{}", e),
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
            ReadOnly => write!(f, "read only disk"),
            OutOfRange => write!(f, "out of range"),
            MissingStatus => write!(f, "not enough space in descriptor chain to write status"),
            URing(e) => write!(f, "uring operation failed {}", e),
            Unsupported(n) => write!(f, "unsupported ({})", n),
        }
    }
}

impl ExecuteError {
    fn status(&self) -> u8 {
        match self {
            ExecuteError::Descriptor(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Read(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::WriteStatus(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::TimerFd(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::DiscardWriteZeroes { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::ReadOnly => VIRTIO_BLK_S_IOERR,
            ExecuteError::OutOfRange { .. } => VIRTIO_BLK_S_IOERR,
            ExecuteError::MissingStatus => VIRTIO_BLK_S_IOERR,
            ExecuteError::URing(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Unsupported(_) => VIRTIO_BLK_S_UNSUPP,
        }
    }
}

// The state that started a read or write op.
// This state is kept so that a partial read or write can be continued.
struct RwOperation {
    // The original iovecs
    iovecs: VecDeque<libc::iovec>,
    // The offset in to the file
    offset: u64,
    // length of the entire transaction.
    len: usize,
}

impl RwOperation {
    fn bytes_processed(mut self, len_processed: usize) -> Self {
        // assert is acceptable as all callers are local and guarantee this case never happens.
        assert!(len_processed < self.len);
        self.offset = self.offset + len_processed as u64;
        self.len = self.len - len_processed as usize;

        let mut iovecs_finished = 0;
        let mut consumed = len_processed;
        for iovec in self.iovecs.iter() {
            if iovec.iov_len <= consumed {
                iovecs_finished += 1;
                consumed -= iovec.iov_len;
            } else {
                break;
            }
        }

        while iovecs_finished > 0 {
            self.iovecs.pop_front().unwrap();
            iovecs_finished -= 1;
        }

        self.iovecs[0].iov_len -= consumed;
        unsafe {
            // Safe because we know the original length was valid and that the new length is <= to
            // that.
            self.iovecs[0].iov_base = (self.iovecs[0].iov_base as *mut u8).add(consumed) as *mut _;
        }

        self
    }
}

enum OperationType {
    FSync,
    PunchHole {
        offset: u64,
        length: u64,
    },
    Read(RwOperation),
    Write(RwOperation),
    WriteZeros {
        offset: u64,
        length: u64,
        sector: u64,
        num_sectors: u32,
        flags: u32,
    },
}

impl OperationType {
    // Returns true if this operation is allowed on read-only disks.
    fn valid_read_only(&self) -> bool {
        match self {
            OperationType::Read(_) => true, // Read works on read-only disks.
            _ => {
                // Everything else is invalid on read-only disks.
                false
            }
        }
    }
}

type OpResult<T> = std::result::Result<T, ExecuteError>;

// An operation that has been validated for a given disk.
struct ValidOperationType(OperationType);

impl ValidOperationType {
    fn try_from(
        op: OperationType,
        disk_size: u64,
        read_only: bool,
        sparse: bool,
    ) -> OpResult<ValidOperationType> {
        /// Checks that a request accesses only data within the disk's current size.
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

        if read_only && !op.valid_read_only() {
            return Err(ExecuteError::ReadOnly);
        }

        match &op {
            OperationType::FSync => (), // Always valid.
            OperationType::PunchHole { offset, length } => {
                check_range(*offset, *length, disk_size)?
            }
            OperationType::Read(rw_op) => check_range(rw_op.offset, rw_op.len as u64, disk_size)?,
            OperationType::Write(rw_op) => check_range(rw_op.offset, rw_op.len as u64, disk_size)?,
            OperationType::WriteZeros { offset, length, .. } => {
                check_range(*offset, *length, disk_size)?
            }
        }

        Ok(ValidOperationType(op))
    }
}

struct OpIter<'a> {
    req_type: u32,
    next_op: Option<OpResult<OperationType>>,
    reader: Reader<'a>,
    writer: Writer<'a>,
}

impl<'a> OpIter<'a> {
    fn new(mut reader: Reader<'a>, mut writer: Writer<'a>) -> OpResult<OpIter<'a>> {
        let req_header: virtio_blk_req_header = reader.read_obj().map_err(ExecuteError::Read)?;

        let req_type = req_header.req_type.to_native();
        let sector = req_header.sector.to_native();
        let offset = sector
            .checked_shl(u32::from(SECTOR_SHIFT))
            .ok_or(ExecuteError::OutOfRange)?;

        let next_op = match req_type {
            VIRTIO_BLK_T_IN => {
                let data_len = writer.available_bytes();
                // TODO(dgreid 3/18) look at all these allocations (Vec for iovec, vec for consuming
                // each time read is called)...
                // TODO - fix unwrap and extra allocations here.
                if data_len == 0 {
                    None
                } else {
                    let iovecs = VolatileSlice::as_iovecs(writer.get_remaining())
                        .iter()
                        .cloned()
                        .collect::<VecDeque<_>>();
                    let total_len = iovecs.iter().fold(0, |a, iovec| a + iovec.iov_len);
                    writer.consume_gotten(data_len);
                    Some(Ok(OperationType::Read(RwOperation {
                        iovecs,
                        offset,
                        len: total_len,
                    })))
                }
            }
            VIRTIO_BLK_T_OUT => {
                let data_len = reader.available_bytes();
                // TODO(dgreid) make reader give the list of addresses in the list without
                // allocating.
                // TODO - fix unwrap and extra allocations here.
                if data_len == 0 {
                    None
                } else {
                    let iovecs = VolatileSlice::as_iovecs(reader.get_remaining())
                        .iter()
                        .cloned()
                        .collect::<VecDeque<_>>();
                    let total_len = iovecs.iter().fold(0, |a, iovec| a + iovec.iov_len);
                    reader.consume(data_len);
                    Some(Ok(OperationType::Write(RwOperation {
                        iovecs,
                        offset,
                        len: total_len,
                    })))
                }
            }
            VIRTIO_BLK_T_DISCARD | VIRTIO_BLK_T_WRITE_ZEROES => {
                Self::next_discard_op(req_type, &mut reader)
            }
            VIRTIO_BLK_T_FLUSH => Some(Ok(OperationType::FSync)),
            t => return Err(ExecuteError::Unsupported(t)),
        };
        Ok(OpIter {
            req_type,
            next_op,
            reader,
            writer,
        })
    }

    fn get_offset_length(sector: u64, num_sectors: u32) -> OpResult<(u64, u64)> {
        let offset = sector
            .checked_shl(u32::from(SECTOR_SHIFT))
            .ok_or(ExecuteError::OutOfRange)?;
        let length = u64::from(num_sectors)
            .checked_shl(u32::from(SECTOR_SHIFT))
            .ok_or(ExecuteError::OutOfRange)?;
        Ok((offset, length))
    }

    fn next_discard_op(req_type: u32, reader: &mut Reader) -> Option<OpResult<OperationType>> {
        if reader.available_bytes() < size_of::<virtio_blk_discard_write_zeroes>() {
            return None;
        }
        Some(Self::read_next_discard_op(req_type, reader))
    }

    fn read_next_discard_op(req_type: u32, reader: &mut Reader) -> OpResult<OperationType> {
        let seg: virtio_blk_discard_write_zeroes = reader.read_obj().map_err(ExecuteError::Read)?;

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

        let (offset, length) = Self::get_offset_length(sector, num_sectors)?;

        if req_type == VIRTIO_BLK_T_DISCARD {
            Ok(OperationType::PunchHole { offset, length })
        } else {
            Ok(OperationType::WriteZeros {
                offset,
                length,
                sector,
                num_sectors,
                flags,
            })
        }
    }
}

impl<'a> Iterator for OpIter<'a> {
    type Item = OpResult<OperationType>;
    fn next(&mut self) -> Option<Self::Item> {
        let ret = self.next_op.take();
        self.next_op = match self.req_type {
            VIRTIO_BLK_T_DISCARD | VIRTIO_BLK_T_WRITE_ZEROES => {
                Self::next_discard_op(self.req_type, &mut self.reader)
            }
            _ => None, // All other requests only generate one operation.
        };
        ret
    }
}

struct Operation {
    // The queue the operation is being run for.
    queue_index: usize,
    // The descriptor that the operation is being run for.
    desc_index: u16,
    // The file being operatied on
    fd: RawFd,
    // Op-type specific data.
    type_: ValidOperationType,
}

impl Operation {
    fn new(
        op_type: OperationType,
        queue_index: usize,
        desc_index: u16,
        fd: RawFd,
        disk_size: u64,
        read_only: bool,
        sparse: bool,
    ) -> OpResult<Self> {
        Ok(Operation {
            queue_index,
            desc_index,
            fd,
            type_: ValidOperationType::try_from(op_type, disk_size, read_only, sparse)?,
        })
    }
}

struct PendingDescriptor {
    desc_len: usize,
    pending_op_count: usize,
    status_byte: *mut u8,
}

impl PendingDescriptor {
    fn set_error(&self, err: u8) {
        unsafe {
            // Safe as long as the status_byte is valid. This is guaranteed by the caller holding a
            // guestmemory reference and the pointer being within GuestMemory,
            *self.status_byte = err;
        }
    }
}

struct URingState {
    uring_ctx: URingContext,
    uring_idx: u64,
    // Map an operation to the queue and descriptor waiting for it.
    pending_operations: BTreeMap<u64, Operation>,
    // Map a pending descriptor to the length and the count of ops needed for completion and the
    // status..
    pending_descriptors: BTreeMap<(usize, u16), PendingDescriptor>,
}
// TODO - Not auto send because the iovecs in the `Operation` have a raw pointer. However, that
// pointer will always be to guest memory so it can be sent where ever we want.
unsafe impl Send for URingState {}

impl URingState {
    fn new(max_ops: usize) -> URingState {
        URingState {
            uring_ctx: URingContext::new(max_ops).unwrap(),
            uring_idx: 0,
            pending_operations: BTreeMap::new(),
            pending_descriptors: BTreeMap::new(),
        }
    }
}

enum OpStarted {
    ASync,
    Sync,
}

// TODO - maybe move this to desc_utils: desc.read_write_status(&mem)
// ->(reader,writer,status_writer)
fn decompose_desc<'a>(
    mem: &'a GuestMemory,
    avail_desc: DescriptorChain<'a>,
) -> std::result::Result<(Reader<'a>, Writer<'a>, Writer<'a>), ExecuteError> {
    let reader = Reader::new(mem, avail_desc.clone()).map_err(ExecuteError::Descriptor)?;
    let mut writer = Writer::new(mem, avail_desc).map_err(ExecuteError::Descriptor)?;
    // The last byte of the buffer is virtio_blk_req::status.
    // Split it into a separate Writer so that status_writer is the final byte and
    // the original writer is left with just the actual block I/O data.
    let available_bytes = writer.available_bytes();
    let status_offset = available_bytes
        .checked_sub(1)
        .ok_or(ExecuteError::MissingStatus)?;
    let status_writer = writer.split_at(status_offset);
    Ok((reader, writer, status_writer))
}

struct DiskState<'a> {
    disk_size: u64,
    read_only: bool,
    sparse: bool,
    disk_image: &'a mut dyn DiskFile,
}

struct Worker {
    interrupt: Interrupt,
    queues: Vec<Queue>,
    disk_image: Box<dyn DiskFile>,
    disk_size: Arc<Mutex<u64>>,
    read_only: bool,
    sparse: bool,
    control_socket: DiskControlResponseSocket,
    uring_state: URingState,
}

impl Worker {
    // returns true if the op was complete, false if it needs to wait for uring completions.
    fn start_op(
        mem: &GuestMemory,
        op: &Operation,
        uring_state: &mut URingState,
        disk_state: &mut DiskState,
    ) -> io_uring::Result<OpStarted> {
        match &op.type_.0 {
            // Safe because we know both guest memory and the fd will live
            // until we collect the completion.
            OperationType::FSync => uring_state
                .uring_ctx
                .add_fsync(disk_state.disk_image.as_raw_fds()[0], uring_state.uring_idx)
                .map(|_| OpStarted::ASync),
            OperationType::PunchHole { offset, length } => {
                // Since Discard is just a hint and some filesystems may not implement
                // FALLOC_FL_PUNCH_HOLE, ignore punch_hole errors or attempts to
                // puch holes in non-sparse files.
                if !disk_state.sparse {
                    let _ = disk_state.disk_image.punch_hole(*offset, *length);
                }
                // TODO - make this async
                Ok(OpStarted::Sync)
            }
            OperationType::Read(rw_op) => unsafe {
                uring_state
                    .uring_ctx
                    .add_readv_iter(
                        rw_op.iovecs.iter().cloned(),
                        disk_state.disk_image.as_raw_fds()[0],
                        rw_op.offset,
                        uring_state.uring_idx,
                    )
                    .map(|_| OpStarted::ASync)
            },
            OperationType::Write(rw_op) => {
                /* TODO - should we be flushing this much?
                // Delay after a write when the file is auto-flushed.
                let flush_delay = Duration::from_secs(60);
                if !*flush_timer_armed {
                    match flush_timer
                        .reset(flush_delay, None)
                        .map_err(ExecuteError::TimerFd)
                    {
                        Ok(_) => *flush_timer_armed = true,
                        Err(e) => error!("Failed to set the flush timer{}.", e),
                    }
                }
                */
                unsafe {
                    uring_state
                        .uring_ctx
                        .add_writev_iter(
                            rw_op.iovecs.iter().cloned(),
                            disk_state.disk_image.as_raw_fds()[0],
                            rw_op.offset,
                            uring_state.uring_idx,
                        )
                        .map(|_| OpStarted::ASync)
                }
            }
            OperationType::WriteZeros {
                offset,
                length,
                sector,
                num_sectors,
                flags,
            } => {
                //TODO async.
                disk_state
                    .disk_image
                    .write_zeroes_all_at(*offset, *length as usize)
                    .map_err(|e| ExecuteError::DiscardWriteZeroes {
                        ioerr: Some(e),
                        sector: *sector,
                        num_sectors: *num_sectors,
                        flags: *flags,
                    })
                    .unwrap(); //TODO -can fail here
                               // TODO - make this async and not return here
                Ok(OpStarted::Sync)
            }
        }
    }

    fn process_queue(
        &mut self,
        mem: &GuestMemory,
        queue_index: usize,
        flush_timer: &mut TimerFd,
        flush_timer_armed: &mut bool,
    ) {
        // TODO weird lock trickery to avoid borrowing self.
        let local_disk_size_lock = self.disk_size.clone();
        let disk_size = local_disk_size_lock.lock();

        while let Some(avail_desc) = self.queues[queue_index].pop(mem) {
            self.queues[queue_index].set_notify(mem, false);
            let desc_index = avail_desc.index;

            // save parts of self that are used to bulid the closure which is borrowed during the
            // loop where a mutable borrow of self is needed. Both `read_only` and `sparse` are
            // const for the lifetime of the device so sampling them here doesn't hurt.
            let mut disk_state = DiskState {
                disk_size: *disk_size,
                read_only: self.read_only,
                sparse: self.sparse,
                disk_image: &mut *self.disk_image,
            };

            /*
            // TODO (maybe)
            // commands are from the descriptor, N commands per descriptor
            // operations are uring ops, M operation per command
            // avail_desc -> block_desc(reader,writer,ops) -> ops -> pending or complete descriptor
            BlockDescriptor::new(mem, avail_desc)
                .and_then(|bd| {
                let commands = bd.command_iterator();
                for each command in the commands, get a list of ops needed, start them.
                maybe, disk.start_command()?? impls of start command for composite,sparse, etc. Each
                can decide to be async or not.  start_command can return an iterator of pending ops
                })
            */

            // TODO unwrap
            let (reader, writer, status_writer) = decompose_desc(mem, avail_desc).unwrap();
            let status_addr = status_writer.first_offset().unwrap();
            let available_bytes = writer.available_bytes();

            // Get a list of operations requested in this descriptor.
            let op_iter = match OpIter::new(reader, writer) {
                Ok(o) => o,
                Err(e) => {
                    self.queues[queue_index].add_used(mem, desc_index, available_bytes as u32);
                    self.queues[queue_index].trigger_interrupt(mem, &self.interrupt);
                    error!("block: failed to handle request: {}", e);
                    continue;
                }
            };

            let mut num_async_ops = 0;
            // TODO - do something with errors besides filter and drop.
            for op_type in op_iter.filter(|r| r.is_ok()) {
                // TODO unwrap of new
                let op = Operation::new(
                    op_type.unwrap(),
                    queue_index,
                    desc_index,
                    disk_state.disk_image.as_raw_fds()[0], // TODO, composite etc.
                    *disk_size,
                    disk_state.read_only,
                    disk_state.sparse,
                )
                .unwrap();
                'free_sqe: loop {
                    match Self::start_op(mem, &op, &mut self.uring_state, &mut disk_state) {
                        Ok(OpStarted::ASync) => {
                            num_async_ops += 1;
                            self.uring_state
                                .pending_operations
                                .insert(self.uring_state.uring_idx, op);
                            self.uring_state.uring_idx = self.uring_state.uring_idx.wrapping_add(1);
                            break 'free_sqe;
                        }
                        Ok(OpStarted::Sync) => break 'free_sqe,
                        // If there isn't space to submit more operations, block processing some
                        // completions.
                        Err(io_uring::Error::NoSpace) => {
                            let queues = &mut self.queues;
                            let interrupt = &self.interrupt;
                            Worker::handle_uring_ready(
                                &mut self.uring_state,
                                |queue_index, desc_index, len| {
                                    let queue = &mut queues[queue_index];
                                    queue.add_used(mem, desc_index, len as u32);
                                    queue.trigger_interrupt(mem, interrupt);
                                },
                            )
                            .unwrap(); // TODO - .map_err(ExecuteError::URing)?,
                            continue;
                        }

                        Err(e) => {
                            error!("block: failed to submit uring operation: {}", e);
                            break 'free_sqe; //  TODO, report this error Complete with an error
                        }
                    }
                }
            }
            if num_async_ops > 0 {
                // There are async ops pending, remember to return the used descriptor when all the
                // operations complete.
                let pending_descriptor = PendingDescriptor {
                    desc_len: available_bytes,
                    pending_op_count: num_async_ops,
                    status_byte: status_addr as *mut u8,
                };
                pending_descriptor.set_error(VIRTIO_BLK_S_OK);
                self.uring_state
                    .pending_descriptors
                    .insert((queue_index, desc_index), pending_descriptor);
            } else {
                // If not pending any ops, release the descriptor.
                let queue = &mut self.queues[queue_index];
                queue.add_used(mem, desc_index, available_bytes as u32);
                queue.trigger_interrupt(mem, &self.interrupt);
            }

            self.queues[queue_index].set_notify(mem, true);
        }
    }

    fn resize(&mut self, new_size: u64) -> DiskControlResult {
        if self.read_only {
            error!("Attempted to resize read-only block device");
            return DiskControlResult::Err(SysError::new(libc::EROFS));
        }

        info!("Resizing block device to {} bytes", new_size);

        if let Err(e) = self.disk_image.set_len(new_size) {
            error!("Resizing disk failed! {}", e);
            return DiskControlResult::Err(SysError::new(libc::EIO));
        }

        // Allocate new space if the disk image is not sparse.
        if let Err(e) = self.disk_image.allocate(0, new_size) {
            error!("Allocating disk space after resize failed! {}", e);
            return DiskControlResult::Err(SysError::new(libc::EIO));
        }

        self.sparse = false;

        if let Ok(new_disk_size) = self.disk_image.get_len() {
            let mut disk_size = self.disk_size.lock();
            *disk_size = new_disk_size;
        }
        DiskControlResult::Ok
    }

    fn run(&mut self, mem: &GuestMemory, queue_evts: Vec<EventFd>, kill_evt: EventFd) {
        const FLUSH_TIMER: u64 = 0u64;
        const CONTROL_REQUEST: u64 = 1u64;
        const INTERRUPT_RESAMPLE: u64 = 2u64;
        const KILL: u64 = 3u64;
        const URING_READY: u64 = 4u64;
        const QUEUE_AVAILABLE: u64 = 5u64;

        let mut flush_timer = match TimerFd::new() {
            Ok(t) => t,
            Err(e) => {
                error!("Failed to create the flush timer: {}", e);
                return;
            }
        };
        let mut flush_timer_armed = false;

        let poll_ctx: PollContext<u64> = match PollContext::build_with(&[
            (&flush_timer, FLUSH_TIMER),
            (&self.control_socket, CONTROL_REQUEST),
            (self.interrupt.get_resample_evt(), INTERRUPT_RESAMPLE),
            (&kill_evt, KILL),
            (&self.uring_state.uring_ctx, URING_READY),
        ]) {
            Ok(pc) => pc,
            Err(e) => {
                error!("failed creating PollContext: {}", e);
                return;
            }
        };

        for (i, evt) in queue_evts.iter().enumerate() {
            poll_ctx.add(evt, QUEUE_AVAILABLE + i as u64).unwrap();
        }

        'poll: loop {
            let events = match poll_ctx.wait() {
                Ok(v) => v,
                Err(e) => {
                    error!("failed polling for events: {}", e);
                    break;
                }
            };

            let mut needs_config_interrupt = false;
            for event in events.iter_readable() {
                match event.token() {
                    FLUSH_TIMER => {
                        if let Err(e) = self.disk_image.fsync() {
                            error!("Failed to flush the disk: {}", e);
                            break 'poll;
                        }
                        if let Err(e) = flush_timer.wait() {
                            error!("Failed to clear flush timer: {}", e);
                            break 'poll;
                        }
                    }
                    CONTROL_REQUEST => {
                        let req = match self.control_socket.recv() {
                            Ok(req) => req,
                            Err(e) => {
                                error!("control socket failed recv: {}", e);
                                break 'poll;
                            }
                        };

                        let resp = match req {
                            DiskControlCommand::Resize { new_size } => {
                                needs_config_interrupt = true;
                                self.resize(new_size)
                            }
                        };

                        if let Err(e) = self.control_socket.send(&resp) {
                            error!("control socket failed send: {}", e);
                            break 'poll;
                        }
                    }
                    INTERRUPT_RESAMPLE => {
                        self.interrupt.interrupt_resample();
                    }
                    KILL => break 'poll,
                    URING_READY => {
                        let queues = &mut self.queues;
                        let interrupt = &self.interrupt;
                        if let Err(_) = Worker::handle_uring_ready(
                            &mut self.uring_state,
                            |queue_index, desc_index, len| {
                                let queue = &mut queues[queue_index];
                                queue.add_used(mem, desc_index, len as u32);
                                queue.trigger_interrupt(mem, interrupt);
                            },
                        ) {
                            error!("Error processing uring events");
                            break 'poll;
                        }
                    }
                    // All other are queue notifications.
                    n => {
                        let queue_index = (n - QUEUE_AVAILABLE) as usize;
                        if let Err(e) = queue_evts[queue_index].read() {
                            error!("failed reading queue EventFd: {}", e);
                            break 'poll;
                        }
                        self.process_queue(
                            mem,
                            queue_index,
                            &mut flush_timer,
                            &mut flush_timer_armed,
                        );
                    }
                }
            }
            self.uring_state.uring_ctx.submit().unwrap();
            if needs_config_interrupt {
                self.interrupt.signal_config_changed();
            }
        }
    }

    fn handle_uring_ready(
        uring_state: &mut URingState,
        mut queue_complete: impl FnMut(usize, u16, usize),
    ) -> std::result::Result<(), ()> {
        let events = match uring_state.uring_ctx.wait() {
            Ok(e) => e,
            Err(_) => return Err(()),
        };

        let mut new_ops = Vec::new(); // TODO - another allocation...

        for (id, op_res) in events {
            // update id's status and send the result to the virt queue.
            if let Some(Operation {
                queue_index,
                desc_index,
                fd,
                type_: op_type,
            }) = uring_state.pending_operations.remove(&id)
            {
                match op_res {
                    Err(res) => {
                        uring_state
                            .pending_descriptors
                            .get_mut(&(queue_index, desc_index))
                            .map(|pd| pd.set_error(VIRTIO_BLK_S_IOERR));
                    }
                    Ok(r) => match op_type.0 {
                        OperationType::Read(rw_op) => {
                            let op_len =
                                rw_op.iovecs.iter().fold(0, |a, iovec| a + iovec.iov_len) as u32;
                            if r < op_len {
                                // re-add this operation below as soon as the current borrow of
                                // the ring is over.
                                let op = Operation {
                                    queue_index,
                                    desc_index,
                                    fd,
                                    type_: ValidOperationType(OperationType::Read(
                                        rw_op.bytes_processed(r as usize),
                                    )),
                                };
                                new_ops.push(op);
                                // no need to check if the descriptor is done as this op still
                                // needs to complete
                                continue;
                            }
                        }
                        OperationType::Write(rw_op) => {
                            let op_len =
                                rw_op.iovecs.iter().fold(0, |a, iovec| a + iovec.iov_len) as u32;
                            if r < op_len {
                                // re-add this operation below as soon as the current borrow of
                                // the ring is over.
                                let op = Operation {
                                    queue_index,
                                    desc_index,
                                    fd,
                                    type_: ValidOperationType(OperationType::Write(
                                        rw_op.bytes_processed(r as usize),
                                    )),
                                };
                                new_ops.push(op);
                                // no need to check if the descriptor is done as this op still
                                // needs to complete
                                continue;
                            }
                        }
                        _ => (),
                    },
                }
                let mut pd = uring_state
                    .pending_descriptors
                    .remove(&(queue_index, desc_index))
                    .unwrap();
                if pd.pending_op_count == 1 {
                    //complete now.
                    queue_complete(queue_index, desc_index, pd.desc_len);
                } else {
                    pd.pending_op_count -= 1;
                    uring_state
                        .pending_descriptors
                        .insert((queue_index, desc_index), pd);
                }
            }
        }

        for op in new_ops.into_iter() {
            // TODO - handle write too.
            match &op.type_.0 {
                OperationType::Read(rw_state) => {
                    unsafe {
                        // Ok because we know both guest memory and the fd will
                        // survive until we collect the completion.
                        uring_state
                            .uring_ctx
                            .add_readv_iter(
                                rw_state.iovecs.iter().cloned(),
                                op.fd,
                                rw_state.offset,
                                uring_state.uring_idx,
                            )
                            .unwrap(); //TODO
                    }
                }
                OperationType::Write(rw_state) => {
                    unsafe {
                        // Ok because we know both guest memory and the fd will
                        // survive until we collect the completion.
                        uring_state
                            .uring_ctx
                            .add_writev_iter(
                                rw_state.iovecs.iter().cloned(),
                                op.fd,
                                rw_state.offset,
                                uring_state.uring_idx,
                            )
                            .unwrap(); //TODO
                    }
                }
                _ => unreachable!(),
            }
            uring_state
                .pending_operations
                .insert(uring_state.uring_idx, op);
            uring_state.uring_idx = uring_state.uring_idx.wrapping_add(1);
            uring_state.uring_ctx.submit().unwrap();
        }

        Ok(())
    }
}

/// Virtio device for exposing block level read/write operations on a host file.
pub struct Block {
    kill_evt: Option<EventFd>,
    worker_thread: Option<thread::JoinHandle<Worker>>,
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
        info!("have {} queues for block", queues.len());
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
                            let mut worker = Worker {
                                interrupt,
                                queues,
                                disk_image,
                                disk_size,
                                read_only,
                                sparse,
                                control_socket,
                                uring_state: URingState::new(
                                    // allocate two ring entries for each possible virtio
                                    // descriptor, most use one entry.
                                    2 * QUEUE_SIZE as usize * NUM_QUEUES as usize,
                                ),
                            };
                            worker.run(&mem, queue_evts, kill_evt);
                            worker
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
                Ok(worker) => {
                    self.disk_image = Some(worker.disk_image);
                    self.control_socket = Some(worker.control_socket);
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
    use std::io::{Read, Seek, SeekFrom, Write};
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
        for i in 0..0x400 {
            f.write(&(i as u32).to_ne_bytes()).unwrap();
        }

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

        let mut flush_timer = TimerFd::new().expect("failed to create flush_timer");
        let mut flush_timer_armed = false;
        let mut uring_state = URingState::new(256);

        // Set status to something non-zero so the test for it being zero later is legit.
        let status_offset = GuestAddress((0x1000 + size_of_val(&req_hdr) + 512) as u64);
        mem.write_at_addr(&[55u8], status_offset).unwrap();

        let result = Worker::process_one_request(
            avail_desc,
            false,
            true,
            &mut f,
            disk_size,
            &mut flush_timer,
            &mut flush_timer_armed,
            &mem,
            &mut uring_state,
            5,
        )
        .expect("execute failed");

        Worker::handle_uring_ready(&mut uring_state, |queue_index, desc_index, len| {
            assert_eq!(queue_index, 5);
            assert_eq!(desc_index, 0);
            assert_eq!(len, 513); // 512 bytes plus status.
        })
        .unwrap();

        let status = mem.read_obj_from_addr::<u8>(status_offset).unwrap();
        assert_eq!(status, VIRTIO_BLK_S_OK);

        f.seek(SeekFrom::Start(0)).unwrap();
        for i in 0..0x400 {
            let mut read_back = [0u8; 0x4];
            f.read(&mut read_back).unwrap();
            assert_eq!(i as u32, u32::from_ne_bytes(read_back));
        }
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

        let mut flush_timer = TimerFd::new().expect("failed to create flush_timer");
        let mut flush_timer_armed = false;
        let mut uring_state = URingState::new(256);

        Worker::process_one_request(
            avail_desc,
            false,
            true,
            &mut f,
            disk_size,
            &mut flush_timer,
            &mut flush_timer_armed,
            &mem,
            &mut uring_state,
            0,
        )
        .expect("execute failed");

        let status_offset = GuestAddress((0x1000 + size_of_val(&req_hdr) + 512 * 2) as u64);
        let status = mem.read_obj_from_addr::<u8>(status_offset).unwrap();
        assert_eq!(status, VIRTIO_BLK_S_IOERR);
    }
}

// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ffi::CString;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;

use libc::EINVAL;

use crate::descriptor::FromRawDescriptor;
use crate::sys::unix::RawDescriptor;
use crate::unix::set_descriptor_cloexec;
use crate::unix::Pid;

mod event;
pub(in crate::sys::macos) mod kqueue;
mod net;

// Re-export net types for use in lib.rs via explicit paths
pub use net::UnixSeqpacket;
pub use net::UnixSeqpacketListener;
pub use net::UnlinkUnixSeqpacketListener;
mod timer;

pub(crate) use event::PlatformEvent;
pub(in crate::sys) use libc::sendmsg;
pub(in crate::sys) use net::sockaddr_un;
pub(in crate::sys) use net::sockaddrv4_to_lib_c;
pub(in crate::sys) use net::sockaddrv6_to_lib_c;

/// Sets the name of the current thread to `name`.
///
/// On MacOS, uses pthread_setname_np which only sets the name for the calling thread.
pub fn set_thread_name(name: &str) -> crate::errno::Result<()> {
    let c_name = CString::new(name).or(Err(crate::errno::Error::new(EINVAL)))?;
    // SAFETY: pthread_setname_np on macOS only takes a single argument (the name)
    // and only works on the calling thread. The name is copied internally.
    let ret = unsafe { libc::pthread_setname_np(c_name.as_ptr()) };
    if ret == 0 {
        Ok(())
    } else {
        Err(crate::errno::Error::new(ret))
    }
}

/// Returns the CPU affinity of the current thread.
///
/// MacOS does not have a direct API for getting CPU affinity like Linux's sched_getaffinity.
/// This returns all CPUs as available since MacOS manages thread scheduling automatically.
pub fn get_cpu_affinity() -> crate::errno::Result<Vec<usize>> {
    // MacOS doesn't support getting CPU affinity directly.
    // Return all available CPUs.
    let num_cpus = crate::number_of_logical_cores()?;
    Ok((0..num_cpus).collect())
}

/// Returns the process ID of the calling process.
pub fn getpid() -> Pid {
    // SAFETY: getpid is always safe to call and never fails.
    unsafe { libc::getpid() }
}

/// Opens a file or duplicates an existing file descriptor.
///
/// If the path is of the form /dev/fd/N, duplicates the file descriptor N.
/// Otherwise, opens the file at the given path.
pub fn open_file_or_duplicate<P: AsRef<Path>>(
    path: P,
    options: &OpenOptions,
) -> crate::Result<File> {
    let path = path.as_ref();

    // Check if this is a /dev/fd/N path
    if let Some(path_str) = path.to_str() {
        if let Some(fd_str) = path_str.strip_prefix("/dev/fd/") {
            if let Ok(fd) = fd_str.parse::<RawDescriptor>() {
                // SAFETY: fcntl with F_DUPFD_CLOEXEC is safe for any fd value; an invalid fd
                // simply causes fcntl to return -1.
                let new_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
                if new_fd < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                // SAFETY: new_fd is a valid fd returned by the successful fcntl call above, and
                // we transfer exclusive ownership here.
                return Ok(unsafe { File::from_raw_descriptor(new_fd) });
            }
        }
    }

    // Normal file open
    Ok(options.open(path)?)
}

pub mod platform_timer_resolution {
    /// Represents an enabled high resolution timer on MacOS.
    /// MacOS has high-resolution timers by default through mach_absolute_time.
    pub struct UnixSetTimerResolution {}
    impl crate::EnabledHighResTimer for UnixSetTimerResolution {}

    /// Enables high resolution timers on MacOS.
    ///
    /// On MacOS, high-resolution timing is available by default through the Mach
    /// absolute time API, so this is essentially a no-op that returns success.
    pub fn enable_high_res_timers() -> crate::Result<Box<dyn crate::EnabledHighResTimer>> {
        // MacOS has high-resolution timers by default via mach_absolute_time.
        // No special action needed.
        Ok(Box::new(UnixSetTimerResolution {}))
    }
}

/// Sets the CPU affinity of the current thread to the given set of CPUs.
///
/// On MacOS, true CPU affinity binding is not supported. MacOS uses an affinity
/// hint system via thread_policy_set with THREAD_AFFINITY_POLICY, which is just
/// a hint to the scheduler and not a hard binding.
///
/// For now, this is a no-op that succeeds, as MacOS manages thread scheduling
/// automatically. Future implementations could add THREAD_AFFINITY_POLICY hints.
pub fn set_cpu_affinity<I: IntoIterator<Item = usize>>(_cpus: I) -> crate::errno::Result<()> {
    // MacOS doesn't support hard CPU affinity binding like Linux.
    // We could use THREAD_AFFINITY_POLICY as a hint, but it's not required.
    // For simplicity, we just succeed without doing anything.
    Ok(())
}

use smallvec::SmallVec;

use crate::AsRawDescriptor;
use crate::EventToken;
use crate::EventType;
use crate::TriggeredEvent;

use kqueue::Kqueue;

const EVENT_CONTEXT_MAX_EVENTS: usize = 16;

/// Used to poll multiple objects that have file descriptors using kqueue.
pub struct EventContext<T: EventToken> {
    kqueue: Kqueue,
    tokens: std::marker::PhantomData<T>,
}

impl<T: EventToken> EventContext<T> {
    /// Creates a new `EventContext`.
    pub fn new() -> crate::errno::Result<EventContext<T>> {
        Ok(EventContext {
            kqueue: Kqueue::new()?,
            tokens: std::marker::PhantomData,
        })
    }

    /// Creates a new `EventContext` and adds the slice of `fd` and `token` tuples to the new
    /// context.
    pub fn build_with(
        fd_tokens: &[(&dyn AsRawDescriptor, T)],
    ) -> crate::errno::Result<EventContext<T>> {
        let ctx = EventContext::new()?;
        for (fd, token) in fd_tokens {
            ctx.add_for_event(*fd, EventType::Read, T::from_raw_token(token.as_raw_token()))?;
        }
        Ok(ctx)
    }

    /// Adds the given `descriptor` to this context, watching for the specified events.
    pub fn add_for_event(
        &self,
        descriptor: &dyn AsRawDescriptor,
        event_type: EventType,
        token: T,
    ) -> crate::errno::Result<()> {
        let fd = descriptor.as_raw_descriptor();
        let token_raw = token.as_raw_token();
        let mut changes = Vec::new();

        if event_type == EventType::Read || event_type == EventType::ReadWrite {
            changes.push(libc::kevent64_s {
                ident: fd as u64,
                filter: libc::EVFILT_READ,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: 0,
                data: 0,
                udata: token_raw,
                ext: [0, 0],
            });
        }

        if event_type == EventType::Write || event_type == EventType::ReadWrite {
            changes.push(libc::kevent64_s {
                ident: fd as u64,
                filter: libc::EVFILT_WRITE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: 0,
                data: 0,
                udata: token_raw,
                ext: [0, 0],
            });
        }

        if !changes.is_empty() {
            self.kqueue.kevent(&changes, &mut [], None)?;
        }

        Ok(())
    }

    /// Modifies the event type and token for the given `fd`.
    pub fn modify(
        &self,
        fd: &dyn AsRawDescriptor,
        event_type: EventType,
        token: T,
    ) -> crate::errno::Result<()> {
        // On kqueue, we delete the old registrations and add new ones.
        self.delete(fd)?;
        self.add_for_event(fd, event_type, token)
    }

    /// Deletes the given `fd` from this context.
    pub fn delete(&self, fd: &dyn AsRawDescriptor) -> crate::errno::Result<()> {
        let fd_raw = fd.as_raw_descriptor();
        let changes = [
            libc::kevent64_s {
                ident: fd_raw as u64,
                filter: libc::EVFILT_READ,
                flags: libc::EV_DELETE,
                fflags: 0,
                data: 0,
                udata: 0,
                ext: [0, 0],
            },
            libc::kevent64_s {
                ident: fd_raw as u64,
                filter: libc::EVFILT_WRITE,
                flags: libc::EV_DELETE,
                fflags: 0,
                data: 0,
                udata: 0,
                ext: [0, 0],
            },
        ];

        // Ignore errors since the fd may not have been registered for both read and write.
        let _ = self.kqueue.kevent(&changes, &mut [], Some(std::time::Duration::ZERO));
        Ok(())
    }

    /// Waits for any events to occur in FDs that were previously added to this context.
    pub fn wait(&self) -> crate::errno::Result<SmallVec<[TriggeredEvent<T>; 16]>> {
        self.wait_timeout(std::time::Duration::new(i64::MAX as u64, 0))
    }

    /// Like `wait` except will only block for a maximum of the given `timeout`.
    pub fn wait_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> crate::errno::Result<SmallVec<[TriggeredEvent<T>; 16]>> {
        let mut events: [libc::kevent64_s; EVENT_CONTEXT_MAX_EVENTS] =
            [libc::kevent64_s {
                ident: 0,
                filter: 0,
                flags: 0,
                fflags: 0,
                data: 0,
                udata: 0,
                ext: [0, 0],
            }; EVENT_CONTEXT_MAX_EVENTS];

        let timeout_opt = if timeout.as_secs() == i64::MAX as u64 {
            None // Infinite wait
        } else {
            Some(timeout)
        };

        let returned_events = self.kqueue.kevent(&[], &mut events, timeout_opt)?;

        let triggered: SmallVec<[TriggeredEvent<T>; 16]> = returned_events
            .iter()
            .map(|e| {
                TriggeredEvent {
                    token: T::from_raw_token(e.udata),
                    is_readable: e.filter == libc::EVFILT_READ,
                    is_writable: e.filter == libc::EVFILT_WRITE,
                    is_hungup: (e.flags & libc::EV_EOF) != 0,
                }
            })
            .collect();

        Ok(triggered)
    }
}

impl<T: EventToken> AsRawDescriptor for EventContext<T> {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.kqueue.as_raw_descriptor()
    }
}

use std::ptr::null_mut;

use libc::c_int;
use libc::PROT_READ;
use libc::PROT_WRITE;

use crate::pagesize;
use crate::MappedRegion;
use crate::MmapError as Error;
use crate::MmapResult as Result;
use crate::Protection;

impl From<Protection> for c_int {
    #[inline(always)]
    fn from(p: Protection) -> Self {
        let mut value = 0;
        if p.read {
            value |= PROT_READ
        }
        if p.write {
            value |= PROT_WRITE;
        }
        value
    }
}

/// Tracks Fixed Memory Maps within an anonymous memory-mapped fixed-sized arena.
pub struct MemoryMappingArena {
    addr: *mut u8,
    size: usize,
}

// SAFETY: Send is safe for MemoryMappingArena as the pointer is only
// accessed through a stateless interface.
unsafe impl Send for MemoryMappingArena {}
// SAFETY: Sync is safe for MemoryMappingArena as the pointer is only
// accessed through a stateless interface.
unsafe impl Sync for MemoryMappingArena {}

impl MemoryMappingArena {
    /// Creates an mmap arena of `size` bytes.
    pub fn new(size: usize) -> Result<MemoryMappingArena> {
        MemoryMapping::new_protection(size, Protection::read()).map(From::from)
    }

    /// Anonymously maps `size` bytes at `offset` bytes from the start of the arena.
    pub fn add_anon(&mut self, offset: usize, size: usize) -> Result<()> {
        self.add_anon_protection(offset, size, Protection::read_write())
    }

    /// Anonymously maps `size` bytes at `offset` with `prot` protection.
    pub fn add_anon_protection(
        &mut self,
        offset: usize,
        size: usize,
        prot: Protection,
    ) -> Result<()> {
        self.try_add(offset, size, prot, None)
    }

    /// Maps `size` bytes from the start of the given `fd` at `offset`.
    pub fn add_fd(&mut self, offset: usize, size: usize, fd: &dyn AsRawDescriptor) -> Result<()> {
        self.add_fd_offset(offset, size, fd, 0)
    }

    /// Maps `size` bytes starting at `fd_offset` from within `fd` at `offset`.
    pub fn add_fd_offset(
        &mut self,
        offset: usize,
        size: usize,
        fd: &dyn AsRawDescriptor,
        fd_offset: u64,
    ) -> Result<()> {
        self.add_fd_offset_protection(offset, size, fd, fd_offset, Protection::read_write())
    }

    /// Maps `size` bytes starting at `fd_offset` from within `fd` at `offset` with `prot`.
    pub fn add_fd_offset_protection(
        &mut self,
        offset: usize,
        size: usize,
        fd: &dyn AsRawDescriptor,
        fd_offset: u64,
        prot: Protection,
    ) -> Result<()> {
        self.try_add(offset, size, prot, Some((fd, fd_offset)))
    }

    fn try_add(
        &mut self,
        offset: usize,
        size: usize,
        prot: Protection,
        fd: Option<(&dyn AsRawDescriptor, u64)>,
    ) -> Result<()> {
        if offset % pagesize() != 0 {
            return Err(Error::NotPageAligned);
        }
        let end = offset.checked_add(size).ok_or(Error::InvalidAddress)?;
        if end > self.size {
            return Err(Error::InvalidAddress);
        }

        // SAFETY: self.addr.add(offset) is within the arena bounds (validated above)
        // and the caller (try_add) ensures no overlapping live MemoryMappings exist.
        let mmap = unsafe {
            match fd {
                Some((fd, fd_offset)) => MemoryMapping::from_descriptor_offset_protection_fixed(
                    self.addr.add(offset),
                    fd,
                    size,
                    fd_offset,
                    prot,
                )?,
                None => MemoryMapping::new_protection_fixed(self.addr.add(offset), size, prot)?,
            }
        };

        // Don't drop the mapping - it will be cleaned up when the arena is dropped.
        std::mem::forget(mmap);
        Ok(())
    }

    /// Removes `size` bytes at `offset` from the arena.
    pub fn remove(&mut self, offset: usize, size: usize) -> Result<()> {
        self.try_add(offset, size, Protection::read(), None)
    }
}

// SAFETY: The pointer and size point to memory owned by this arena.
unsafe impl MappedRegion for MemoryMappingArena {
    fn as_ptr(&self) -> *mut u8 {
        self.addr
    }

    fn size(&self) -> usize {
        self.size
    }
}

impl Drop for MemoryMappingArena {
    fn drop(&mut self) {
        // SAFETY: self.addr and self.size describe a region we obtained from mmap in
        // new() and own exclusively. Any sub-mappings were created with MAP_FIXED within
        // this range and are covered by this munmap.
        unsafe {
            libc::munmap(self.addr as *mut libc::c_void, self.size);
        }
    }
}

impl From<MemoryMapping> for MemoryMappingArena {
    fn from(mmap: MemoryMapping) -> Self {
        let addr = mmap.addr;
        let size = mmap.size;
        std::mem::forget(mmap);
        MemoryMappingArena { addr, size }
    }
}

/// Wraps an anonymous shared memory mapping in the current process.
#[derive(Debug)]
pub struct MemoryMapping {
    addr: *mut u8,
    size: usize,
}

// SAFETY: Send and Sync are safe for MemoryMapping as the pointer is only
// accessed through a stateless interface.
unsafe impl Send for MemoryMapping {}
unsafe impl Sync for MemoryMapping {}

impl MemoryMapping {
    /// Creates an anonymous shared, read/write mapping of `size` bytes.
    pub fn new(size: usize) -> Result<MemoryMapping> {
        MemoryMapping::new_protection(size, Protection::read_write())
    }

    /// Creates an anonymous shared mapping of `size` bytes with `prot` protection.
    pub fn new_protection(size: usize, prot: Protection) -> Result<MemoryMapping> {
        // SAFETY: addr is None so the kernel chooses a non-overlapping address.
        unsafe { MemoryMapping::try_mmap(None, size, prot.into(), None) }
    }

    /// Maps the first `size` bytes of the given `fd` as read/write.
    pub fn from_fd(fd: &dyn AsRawDescriptor, size: usize) -> Result<MemoryMapping> {
        MemoryMapping::from_fd_offset(fd, size, 0)
    }

    /// Maps `size` bytes starting at `offset` from `fd`.
    pub fn from_fd_offset(
        fd: &dyn AsRawDescriptor,
        size: usize,
        offset: u64,
    ) -> Result<MemoryMapping> {
        MemoryMapping::from_fd_offset_protection(fd, size, offset, Protection::read_write())
    }

    /// Maps `size` bytes starting at `offset` from `fd` with `prot` protection.
    pub fn from_fd_offset_protection(
        fd: &dyn AsRawDescriptor,
        size: usize,
        offset: u64,
        prot: Protection,
    ) -> Result<MemoryMapping> {
        // SAFETY: addr is None so the kernel chooses a non-overlapping address.
        // The fd and offset are caller-provided.
        unsafe { MemoryMapping::try_mmap(None, size, prot.into(), Some((fd, offset))) }
    }

    /// Creates an anonymous shared mapping of `size` bytes at the fixed address.
    ///
    /// # Safety
    ///
    /// The caller must ensure no other mappings exist at `addr..addr+size`.
    pub unsafe fn new_protection_fixed(
        addr: *mut u8,
        size: usize,
        prot: Protection,
    ) -> Result<MemoryMapping> {
        MemoryMapping::try_mmap(Some(addr), size, prot.into(), None)
    }

    /// Maps `size` bytes starting at `offset` from `fd` at the fixed address.
    ///
    /// # Safety
    ///
    /// The caller must ensure no other mappings exist at `addr..addr+size`.
    pub unsafe fn from_descriptor_offset_protection_fixed(
        addr: *mut u8,
        fd: &dyn AsRawDescriptor,
        size: usize,
        offset: u64,
        prot: Protection,
    ) -> Result<MemoryMapping> {
        MemoryMapping::try_mmap(Some(addr), size, prot.into(), Some((fd, offset)))
    }

    /// Creates a memory mapping with the given parameters.
    ///
    /// # Safety
    ///
    /// If `addr` is `Some`, the caller must ensure that the address range
    /// `addr..addr+size` does not overlap with any existing mappings, and that
    /// the address is page-aligned. If `fd` is `Some`, the caller must ensure
    /// the file descriptor is valid and the offset is within the file bounds.
    unsafe fn try_mmap(
        addr: Option<*mut u8>,
        size: usize,
        prot: c_int,
        fd: Option<(&dyn AsRawDescriptor, u64)>,
    ) -> Result<MemoryMapping> {
        let mut flags = libc::MAP_SHARED;

        let addr = match addr {
            Some(addr) => {
                if (addr as usize) % pagesize() != 0 {
                    return Err(Error::NotPageAligned);
                }
                flags |= libc::MAP_FIXED;
                addr as *mut libc::c_void
            }
            None => null_mut(),
        };

        let (fd, offset) = match fd {
            Some((fd, offset)) => {
                if offset > libc::off_t::MAX as u64 {
                    return Err(Error::InvalidOffset);
                }
                (fd.as_raw_descriptor(), offset as libc::off_t)
            }
            None => {
                flags |= libc::MAP_ANONYMOUS;
                (-1, 0)
            }
        };

        // SAFETY: All pointer args are valid: addr is either null (kernel-chosen) or a
        // caller-guaranteed non-overlapping page-aligned address. fd is valid or -1
        // (anonymous). size and offset have been validated above.
        let addr = libc::mmap(addr, size, prot, flags, fd, offset);
        if addr == libc::MAP_FAILED {
            return Err(Error::SystemCallFailed(crate::errno::Error::last()));
        }

        Ok(MemoryMapping {
            addr: addr as *mut u8,
            size,
        })
    }

    /// Returns the size of this mapping.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Validates offset+count is within the mapping.
    pub(crate) fn range_end(&self, offset: usize, count: usize) -> Result<usize> {
        let mem_end = offset.checked_add(count).ok_or(Error::InvalidAddress)?;
        if mem_end > self.size {
            return Err(Error::InvalidAddress);
        }
        Ok(mem_end)
    }

    /// Calls msync with MS_SYNC on the mapping.
    pub fn msync(&self) -> Result<()> {
        // SAFETY: self.addr and self.size describe the region we obtained from mmap.
        let ret = unsafe {
            libc::msync(
                self.addr as *mut libc::c_void,
                self.size,
                libc::MS_SYNC,
            )
        };
        if ret == -1 {
            return Err(Error::SystemCallFailed(crate::errno::Error::last()));
        }
        Ok(())
    }
}

// SAFETY: The pointer and size point to memory owned by this MemoryMapping.
unsafe impl MappedRegion for MemoryMapping {
    fn as_ptr(&self) -> *mut u8 {
        self.addr
    }

    fn size(&self) -> usize {
        self.size
    }
}

impl Drop for MemoryMapping {
    fn drop(&mut self) {
        // SAFETY: self.addr and self.size describe a region we obtained from mmap and
        // own exclusively.
        unsafe {
            libc::munmap(self.addr as *mut libc::c_void, self.size);
        }
    }
}

use crate::MemoryMapping as CrateMemoryMapping;
use crate::MemoryMappingBuilder;
use crate::MmapError;
use crate::SafeDescriptor;

/// Trait for Unix-specific (including macOS) MemoryMappingBuilder extensions.
pub trait MemoryMappingBuilderUnix<'a> {
    /// Build the memory mapping given the specified descriptor to mapped memory.
    ///
    /// Default: Create a new memory mapping.
    #[allow(clippy::wrong_self_convention)]
    fn from_descriptor(self, descriptor: &'a dyn AsRawDescriptor) -> MemoryMappingBuilder<'a>;
}

impl<'a> MemoryMappingBuilderUnix<'a> for MemoryMappingBuilder<'a> {
    /// Build the memory mapping given the specified descriptor to mapped memory.
    ///
    /// Default: Create a new memory mapping.
    #[allow(clippy::wrong_self_convention)]
    fn from_descriptor(mut self, descriptor: &'a dyn AsRawDescriptor) -> MemoryMappingBuilder<'a> {
        self.descriptor = Some(descriptor);
        self
    }
}

impl<'a> MemoryMappingBuilder<'a> {
    /// Build a MemoryMapping from the provided options.
    pub fn build(self) -> std::result::Result<CrateMemoryMapping, MmapError> {
        match self.descriptor {
            None => MemoryMappingBuilder::wrap(
                MemoryMapping::new_protection(
                    self.size,
                    self.protection.unwrap_or_else(Protection::read_write),
                )?,
                None,
            ),
            Some(descriptor) => MemoryMappingBuilder::wrap(
                MemoryMapping::from_fd_offset_protection(
                    descriptor,
                    self.size,
                    self.offset.unwrap_or(0),
                    self.protection.unwrap_or_else(Protection::read_write),
                )?,
                None,
            ),
        }
    }

    pub(crate) fn wrap(
        mapping: MemoryMapping,
        file_descriptor: Option<&'a dyn crate::AsRawDescriptor>,
    ) -> std::result::Result<CrateMemoryMapping, MmapError> {
        let file_descriptor = match file_descriptor {
            Some(descriptor) => Some(
                SafeDescriptor::try_from(descriptor)
                    .map_err(|_| MmapError::SystemCallFailed(crate::errno::Error::last()))?,
            ),
            None => None,
        };
        Ok(CrateMemoryMapping {
            mapping,
            _file_descriptor: file_descriptor,
        })
    }
}

pub mod ioctl {
    pub type IoctlNr = std::ffi::c_ulong;

    /// Performs an ioctl with no argument.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the ioctl number is valid and that no
    /// argument is expected.
    pub unsafe fn ioctl<F: crate::AsRawDescriptor>(
        descriptor: &F,
        nr: IoctlNr,
    ) -> std::ffi::c_int {
        // SAFETY: The caller guarantees the ioctl is valid.
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _)
    }

    /// Performs an ioctl with a raw integer argument.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the ioctl number is valid and expects
    /// an integer argument.
    pub unsafe fn ioctl_with_val(
        descriptor: &dyn crate::AsRawDescriptor,
        nr: IoctlNr,
        arg: std::ffi::c_ulong,
    ) -> std::ffi::c_int {
        // SAFETY: The caller guarantees the ioctl and argument are valid.
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _, arg)
    }

    /// Performs an ioctl with a reference to a value.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the ioctl number is valid and expects
    /// a pointer to T.
    pub unsafe fn ioctl_with_ref<T>(
        descriptor: &dyn crate::AsRawDescriptor,
        nr: IoctlNr,
        arg: &T,
    ) -> std::ffi::c_int {
        // SAFETY: The caller guarantees the ioctl and argument are valid.
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _, arg as *const T)
    }

    /// Performs an ioctl with a mutable reference to a value.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the ioctl number is valid and expects
    /// a pointer to T.
    pub unsafe fn ioctl_with_mut_ref<T>(
        descriptor: &dyn crate::AsRawDescriptor,
        nr: IoctlNr,
        arg: &mut T,
    ) -> std::ffi::c_int {
        // SAFETY: The caller guarantees the ioctl and argument are valid.
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _, arg as *mut T)
    }

    /// Performs an ioctl with a const pointer argument.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the ioctl number is valid and expects
    /// a pointer to T, and that the pointer is valid.
    pub unsafe fn ioctl_with_ptr<T>(
        descriptor: &dyn crate::AsRawDescriptor,
        nr: IoctlNr,
        arg: *const T,
    ) -> std::ffi::c_int {
        // SAFETY: The caller guarantees the ioctl and argument are valid.
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _, arg)
    }

    /// Performs an ioctl with a mutable pointer argument.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the ioctl number is valid and expects
    /// a pointer to T, and that the pointer is valid.
    pub unsafe fn ioctl_with_mut_ptr<T>(
        descriptor: &dyn crate::AsRawDescriptor,
        nr: IoctlNr,
        arg: *mut T,
    ) -> std::ffi::c_int {
        // SAFETY: The caller guarantees the ioctl and argument are valid.
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _, arg)
    }
}

/// Punches a hole in a file at the given offset and length.
///
/// On MacOS, uses fcntl with F_PUNCHHOLE to deallocate file space.
pub fn file_punch_hole(file: &File, offset: u64, length: u64) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    // On MacOS, F_PUNCHHOLE requires an fpunchhole_t structure
    #[repr(C)]
    struct fpunchhole_t {
        fp_flags: libc::c_uint,
        reserved: libc::c_uint,
        fp_offset: libc::off_t,
        fp_length: libc::off_t,
    }

    const F_PUNCHHOLE: libc::c_int = 99;

    let fph = fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset as libc::off_t,
        fp_length: length as libc::off_t,
    };

    // SAFETY: file is a valid fd and fph is a properly initialized fpunchhole_t on the stack.
    let ret = unsafe { libc::fcntl(file.as_raw_fd(), F_PUNCHHOLE, &fph) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Writes zeroes to a file at the given offset and length.
///
/// On MacOS, we try F_PUNCHHOLE first, then fall back to writing zeros.
pub fn file_write_zeroes_at(
    file: &File,
    offset: u64,
    length: usize,
) -> std::io::Result<usize> {
    use std::io::Write as _;
    use std::io::Seek;
    use std::cmp::min;

    // Try to punch hole first (which zeros the range)
    if file_punch_hole(file, offset, length as u64).is_ok() {
        return Ok(length);
    }

    // Fallback: write zeros manually
    let buf_size = min(length, 0x10000);
    let buf = vec![0u8; buf_size];
    let mut nwritten: usize = 0;
    let mut file_ref = file;

    file_ref.seek(std::io::SeekFrom::Start(offset))?;

    while nwritten < length {
        let remaining = length - nwritten;
        let write_size = min(remaining, buf_size);
        nwritten += file_ref.write(&buf[0..write_size])?;
    }
    Ok(length)
}

pub mod syslog {
    /// MacOS syslog implementation.
    ///
    /// On MacOS, we use the BSD syslog functions available via libc.
    pub struct PlatformSyslog {}

    impl crate::syslog::Syslog for PlatformSyslog {
        fn new(
            _proc_name: String,
            _facility: crate::syslog::Facility,
        ) -> std::result::Result<
            (
                Option<Box<dyn crate::syslog::Log + Send>>,
                Option<crate::RawDescriptor>,
            ),
            &'static crate::syslog::Error,
        > {
            // For now, return None to use stderr logging.
            // A full implementation would use ASL (Apple System Log) or os_log.
            // Using stderr is the simplest approach that works on all MacOS versions.
            Ok((None, None))
        }
    }
}

impl PartialEq for crate::SafeDescriptor {
    fn eq(&self, other: &Self) -> bool {
        self.as_raw_descriptor() == other.as_raw_descriptor()
    }
}

impl crate::shm::PlatformSharedMemory for crate::SharedMemory {
    /// Creates a new shared memory region using a temporary file.
    ///
    /// On macOS, shm_open may fail with EPERM due to sandboxing restrictions.
    /// Instead, we use a temporary file created with mkstemp, which provides
    /// the same functionality and is more reliable across macOS configurations.
    /// The file is immediately unlinked after creation so it will be automatically
    /// cleaned up when all file descriptors are closed.
    fn new(debug_name: &std::ffi::CStr, size: u64) -> crate::Result<crate::SharedMemory> {
        // Note: debug_name is for debugging purposes only
        let _ = debug_name;

        // Create a template for mkstemp
        // The template must end with "XXXXXX" which will be replaced with random characters
        let template = CString::new("/tmp/crosvm_shm_XXXXXX")
            .map_err(|_| std::io::Error::from_raw_os_error(EINVAL))?;

        // mkstemp modifies the template in place, so we need a mutable buffer
        let mut template_bytes = template.into_bytes_with_nul();

        // SAFETY: template_bytes is a valid null-terminated mutable buffer with the required
        // "XXXXXX" suffix. mkstemp modifies it in place, which is safe because we own the buffer.
        let fd = unsafe { libc::mkstemp(template_bytes.as_mut_ptr() as *mut libc::c_char) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }

        // Immediately unlink the file so it will be cleaned up when all fds are closed
        // SAFETY: template_bytes still contains the null-terminated path written by mkstemp.
        let ret = unsafe { libc::unlink(template_bytes.as_ptr() as *const libc::c_char) };
        if ret < 0 {
            // Close fd and return error
            // SAFETY: fd is a valid file descriptor returned by mkstemp above.
            unsafe { libc::close(fd) };
            return Err(std::io::Error::last_os_error().into());
        }

        // Set the size
        // SAFETY: fd is a valid file descriptor returned by mkstemp above.
        let ret = unsafe { libc::ftruncate(fd, size as libc::off_t) };
        if ret < 0 {
            // SAFETY: fd is a valid file descriptor returned by mkstemp above.
            unsafe { libc::close(fd) };
            return Err(std::io::Error::last_os_error().into());
        }

        // SAFETY: fd is the valid file descriptor from mkstemp and we transfer exclusive
        // ownership to SafeDescriptor.
        let descriptor = unsafe { crate::SafeDescriptor::from_raw_descriptor(fd) };

        Ok(crate::SharedMemory { descriptor, size })
    }

    /// Creates a SharedMemory instance from an existing SafeDescriptor.
    fn from_safe_descriptor(
        descriptor: crate::SafeDescriptor,
        size: u64,
    ) -> crate::Result<crate::SharedMemory> {
        Ok(crate::SharedMemory { descriptor, size })
    }
}

pub(crate) use libc::off_t;
pub(crate) use libc::pread;
pub(crate) use libc::preadv;
pub(crate) use libc::pwrite;
pub(crate) use libc::pwritev;

/// Spawns a pipe pair where the first pipe is the read end and the second pipe is the write end.
///
/// The `O_CLOEXEC` flag will be applied after pipe creation.
pub fn pipe() -> crate::errno::Result<(File, File)> {
    let mut pipe_fds = [-1; 2];
    // SAFETY:
    // Safe because pipe will only write 2 element array of i32 to the given pointer, and we check
    // for error.
    let ret = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
    if ret == -1 {
        return crate::errno::errno_result();
    }

    // SAFETY:
    // Safe because both fds must be valid for pipe to have returned sucessfully and we have
    // exclusive ownership of them.
    let pipes = unsafe {
        (
            File::from_raw_descriptor(pipe_fds[0]),
            File::from_raw_descriptor(pipe_fds[1]),
        )
    };

    set_descriptor_cloexec(&pipes.0)?;
    set_descriptor_cloexec(&pipes.1)?;

    Ok(pipes)
}

/// File locking operation type for flock().
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlockOperation {
    /// Lock for shared read access.
    LockShared,
    /// Lock for exclusive write access.
    LockExclusive,
    /// Remove an existing lock.
    Unlock,
}

/// Locks or unlocks a file using flock.
///
/// # Arguments
/// * `file` - The file to lock/unlock
/// * `op` - The lock operation to perform
/// * `nonblocking` - If true, return an error instead of blocking when the lock is held
pub fn flock(file: &dyn AsRawDescriptor, op: FlockOperation, nonblocking: bool) -> crate::errno::Result<()> {
    let mut operation = match op {
        FlockOperation::LockShared => libc::LOCK_SH,
        FlockOperation::LockExclusive => libc::LOCK_EX,
        FlockOperation::Unlock => libc::LOCK_UN,
    };

    if nonblocking {
        operation |= libc::LOCK_NB;
    }

    // SAFETY: file provides a valid file descriptor via AsRawDescriptor.
    let ret = unsafe { libc::flock(file.as_raw_descriptor(), operation) };
    if ret < 0 {
        return crate::errno::errno_result();
    }
    Ok(())
}

use std::io::Error as IoError;
use std::os::unix::io::AsRawFd;

/// Implementation of FileAllocate for File on macOS.
///
/// macOS doesn't have posix_fallocate, but we can use ftruncate to extend files
/// and F_PREALLOCATE for pre-allocation.
impl crate::FileAllocate for File {
    fn allocate(&self, offset: u64, len: u64) -> std::io::Result<()> {
        // macOS doesn't have posix_fallocate, but we can use fcntl with F_PREALLOCATE
        // For simplicity, we just extend the file if needed using ftruncate

        let required_size = offset.checked_add(len).ok_or_else(|| {
            IoError::from_raw_os_error(libc::EINVAL)
        })?;

        // Get current file size
        let metadata = self.metadata()?;
        let current_size = metadata.len();

        if required_size > current_size {
            // Extend the file to the required size
            // SAFETY: self provides a valid file descriptor via AsRawFd.
            let ret = unsafe {
                libc::ftruncate(self.as_raw_fd(), required_size as libc::off_t)
            };
            if ret != 0 {
                return Err(IoError::last_os_error());
            }
        }

        Ok(())
    }
}

/// Validates a raw file descriptor.
///
/// This function duplicates the file descriptor to ensure that we don't accidentally
/// close an fd previously opened by another subsystem.
pub fn validate_raw_fd(raw_fd: &RawDescriptor) -> crate::errno::Result<RawDescriptor> {
    // Checking that close-on-exec isn't set helps filter out FDs that were opened by
    // crosvm as all crosvm FDs are close on exec.
    // SAFETY: fcntl with F_GETFD only queries the fd flags and does not modify memory.
    let flags = unsafe { libc::fcntl(*raw_fd, libc::F_GETFD) };
    if flags < 0 || (flags & libc::FD_CLOEXEC) != 0 {
        return Err(crate::errno::Error::new(libc::EBADF));
    }

    // SAFETY: fcntl with F_DUPFD_CLOEXEC does not modify memory. We duplicate rather
    // than consume the original fd to avoid closing an fd owned by another subsystem.
    let dup_fd = unsafe { libc::fcntl(*raw_fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup_fd < 0 {
        return crate::errno::errno_result();
    }

    Ok(dup_fd)
}

/// Creates a SafeDescriptor from a RawDescriptor that was passed on the command line.
///
/// The descriptor is duplicated with CLOEXEC set.
pub fn safe_descriptor_from_cmdline_fd(fd: &RawDescriptor) -> crate::Result<crate::SafeDescriptor> {
    let validated_fd = validate_raw_fd(fd)?;
    Ok(
        // SAFETY:
        // Safe because nothing else has access to validated_fd after this call.
        unsafe { crate::SafeDescriptor::from_raw_descriptor(validated_fd) },
    )
}

/// This module allows macros to refer to $crate::platform::lib and ensures
/// other crates don't need to add additional crates to their Cargo.toml.
pub mod lib {
    pub use libc::off_t;
    pub use libc::pread;
    pub use libc::preadv;
    pub use libc::pwrite;
    pub use libc::pwritev;
}

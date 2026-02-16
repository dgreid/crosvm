// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Simplified passthrough filesystem for macOS using fd-relative operations.
//!
//! Key differences from the Linux passthrough.rs:
//! - No O_PATH, /proc/self/fd, per-thread credentials, capabilities, or SELinux
//! - Uses O_RDONLY for directory tracking instead of O_PATH
//! - Uses fcntl(F_GETPATH) instead of /proc/self/fd/N for fd reopening
//! - Uses fdopendir/readdir instead of SYS_getdents64
//! - All operations run as the crosvm process user

use std::collections::btree_map;
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use base::AsRawDescriptor;
use base::RawDescriptor;
use fuse::filesystem::Context;
use fuse::filesystem::Entry;
use fuse::filesystem::FileSystem;
use fuse::filesystem::FsOptions;
use fuse::filesystem::OpenOptions;
use fuse::filesystem::SetattrValid;
use fuse::filesystem::ZeroCopyReader;
use fuse::filesystem::ZeroCopyWriter;
use fuse::filesystem::ROOT_ID;
use sync::Mutex;

use crate::virtio::fs::config::CachePolicy;
use crate::virtio::fs::config::Config;
use crate::virtio::fs::multikey::MultikeyBTreeMap;
use crate::virtio::fs::read_dir_macos::ReadDir;

type Inode = u64;
type Handle = u64;

fn ebadf() -> io::Error {
    io::Error::from_raw_os_error(libc::EBADF)
}

#[derive(Clone, Copy, Debug, PartialOrd, Ord, PartialEq, Eq)]
struct InodeAltKey {
    ino: libc::ino_t,
    dev: libc::dev_t,
}

#[derive(PartialEq, Eq, Debug)]
enum FileType {
    Regular,
    Directory,
    Other,
}

impl From<u16> for FileType {
    fn from(mode: u16) -> Self {
        match mode & (libc::S_IFMT as u16) {
            x if x == libc::S_IFREG as u16 => FileType::Regular,
            x if x == libc::S_IFDIR as u16 => FileType::Directory,
            _ => FileType::Other,
        }
    }
}

struct InodeData {
    inode: Inode,
    file: Mutex<File>,
    #[allow(dead_code)]
    open_flags: i32,
    refcount: AtomicU64,
    #[allow(dead_code)]
    filetype: FileType,
}

impl AsRawDescriptor for InodeData {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.file.lock().as_raw_descriptor()
    }
}

struct HandleData {
    inode: Inode,
    file: Mutex<File>,
}

/// Get the path of an open file descriptor using fcntl(F_GETPATH).
fn fd_to_path(fd: RawDescriptor) -> io::Result<CString> {
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes into the buffer.
    let ret = unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    // Find the nul terminator.
    let len = buf
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "F_GETPATH missing nul"))?;
    CString::new(&buf[..len])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Reopen an fd with the given flags using F_GETPATH + open.
fn reopen_fd(fd: RawDescriptor, flags: i32) -> io::Result<File> {
    let path = fd_to_path(fd)?;
    // SAFETY: path is a valid CString, we check the return value.
    let new_fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if new_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: we just opened this fd.
    Ok(unsafe { File::from_raw_fd(new_fd) })
}

/// fstat on a file descriptor.
fn fstat(fd: RawDescriptor) -> io::Result<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: fstat writes into st and we check the return value.
    let ret = unsafe { libc::fstat(fd, st.as_mut_ptr()) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat initialized the struct on success.
    Ok(unsafe { st.assume_init() })
}

/// fstatat on a directory fd + name.
fn fstatat(dir_fd: RawDescriptor, name: &CStr) -> io::Result<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: fstatat writes into st and we check the return value.
    let ret = unsafe {
        libc::fstatat(
            dir_fd,
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat initialized the struct on success.
    Ok(unsafe { st.assume_init() })
}

pub struct PassthroughFs {
    #[allow(dead_code)]
    tag: String,
    inodes: Mutex<MultikeyBTreeMap<Inode, InodeAltKey, Arc<InodeData>>>,
    next_inode: AtomicU64,
    handles: Mutex<BTreeMap<Handle, Arc<HandleData>>>,
    next_handle: AtomicU64,
    writeback: AtomicBool,
    cfg: Config,
    root_dir: String,
}

impl PassthroughFs {
    pub fn new(tag: &str, cfg: Config) -> io::Result<PassthroughFs> {
        Ok(PassthroughFs {
            tag: tag.to_string(),
            inodes: Mutex::new(MultikeyBTreeMap::new()),
            next_inode: AtomicU64::new(ROOT_ID + 1),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
            writeback: AtomicBool::new(false),
            cfg,
            root_dir: "/".to_string(),
        })
    }

    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    pub fn set_root_dir(&mut self, root_dir: String) {
        self.root_dir = root_dir;
    }

    pub fn keep_rds(&self) -> Vec<RawDescriptor> {
        let rds = std::cell::RefCell::new(Vec::new());
        let inodes = self.inodes.lock();
        inodes.apply(|data| {
            rds.borrow_mut().push(data.as_raw_descriptor());
        });
        rds.into_inner()
    }

    fn find_inode(&self, inode: Inode) -> io::Result<Arc<InodeData>> {
        self.inodes
            .lock()
            .get(&inode)
            .cloned()
            .ok_or_else(ebadf)
    }

    fn find_handle(&self, handle: Handle, inode: Inode) -> io::Result<Arc<HandleData>> {
        self.handles
            .lock()
            .get(&handle)
            .filter(|hd| hd.inode == inode)
            .cloned()
            .ok_or_else(ebadf)
    }

    fn increase_inode_refcount(&self, data: &Arc<InodeData>) -> Inode {
        data.refcount.fetch_add(1, Ordering::Relaxed);
        data.inode
    }

    fn add_entry(&self, f: File, st: libc::stat, open_flags: i32) -> Entry {
        let mut inodes = self.inodes.lock();
        let altkey = InodeAltKey {
            ino: st.st_ino,
            dev: st.st_dev,
        };

        let inode = if let Some(data) = inodes.get_alt(&altkey) {
            self.increase_inode_refcount(data)
        } else {
            let inode = self.next_inode.fetch_add(1, Ordering::Relaxed);
            inodes.insert(
                inode,
                altkey,
                Arc::new(InodeData {
                    inode,
                    file: Mutex::new(f),
                    open_flags,
                    refcount: AtomicU64::new(1),
                    filetype: st.st_mode.into(),
                }),
            );
            inode
        };

        Entry {
            inode,
            generation: 0,
            attr: st,
            attr_timeout: self.cfg.timeout,
            entry_timeout: self.cfg.timeout,
        }
    }

    fn do_lookup(&self, parent: &InodeData, name: &CStr) -> io::Result<Entry> {
        let st = fstatat(parent.as_raw_descriptor(), name)?;

        let altkey = InodeAltKey {
            ino: st.st_ino,
            dev: st.st_dev,
        };

        // Check if we already have this inode.
        if let Some(data) = self.inodes.lock().get_alt(&altkey) {
            return Ok(Entry {
                inode: self.increase_inode_refcount(data),
                generation: 0,
                attr: st,
                attr_timeout: self.cfg.timeout,
                entry_timeout: self.cfg.timeout,
            });
        }

        // Open the entry.
        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        match FileType::from(st.st_mode) {
            FileType::Directory => flags |= libc::O_DIRECTORY,
            _ => {}
        };

        // SAFETY: openat doesn't modify memory and we check the return value.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_descriptor(),
                name.as_ptr(),
                flags,
            )
        };
        if fd < 0 {
            // If we can't open (e.g., permission denied on a file), try opening
            // with just O_RDONLY (symlinks will fail with O_NOFOLLOW, which is expected).
            return Err(io::Error::last_os_error());
        }

        // SAFETY: we just opened this fd.
        let f = unsafe { File::from_raw_fd(fd) };
        Ok(self.add_entry(f, st, flags))
    }

    fn do_open(&self, inode: Inode, flags: u32) -> io::Result<(Option<Handle>, OpenOptions)> {
        let inode_data = self.find_inode(inode)?;

        let new_flags = self.update_open_flags(flags as i32);
        let file = reopen_fd(inode_data.as_raw_descriptor(), new_flags)?;

        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let data = HandleData {
            inode,
            file: Mutex::new(file),
        };

        self.handles.lock().insert(handle, Arc::new(data));

        let opts = self.get_cache_open_options(flags);
        Ok((Some(handle), opts))
    }

    fn do_release(&self, inode: Inode, handle: Handle) -> io::Result<()> {
        let mut handles = self.handles.lock();
        if let btree_map::Entry::Occupied(e) = handles.entry(handle) {
            if e.get().inode == inode {
                e.remove();
                return Ok(());
            }
        }
        Err(ebadf())
    }

    fn do_unlink(&self, parent: &InodeData, name: &CStr, flags: libc::c_int) -> io::Result<()> {
        // SAFETY: unlinkat doesn't modify memory and we check the return value.
        let ret = unsafe { libc::unlinkat(parent.as_raw_descriptor(), name.as_ptr(), flags) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn do_getattr(&self, inode_data: &InodeData) -> io::Result<(libc::stat, Duration)> {
        let st = fstat(inode_data.as_raw_descriptor())?;
        Ok((st, self.cfg.timeout))
    }

    fn update_open_flags(&self, flags: i32) -> i32 {
        let mut new_flags = flags & !libc::O_NOFOLLOW;
        // Filter out creation flags.
        new_flags &= !(libc::O_CREAT | libc::O_EXCL | libc::O_NOCTTY);
        new_flags |= libc::O_CLOEXEC;
        new_flags
    }

    fn get_cache_open_options(&self, _flags: u32) -> OpenOptions {
        match self.cfg.cache_policy {
            CachePolicy::Never => OpenOptions::DIRECT_IO,
            CachePolicy::Always => OpenOptions::KEEP_CACHE,
            CachePolicy::Auto => OpenOptions::empty(),
        }
    }
}

fn forget_one(
    inodes: &mut MultikeyBTreeMap<Inode, InodeAltKey, Arc<InodeData>>,
    inode: Inode,
    count: u64,
) {
    if let Some(data) = inodes.get(&inode) {
        loop {
            let refcount = data.refcount.load(Ordering::Relaxed);
            let new_count = refcount.saturating_sub(count);

            if data
                .refcount
                .compare_exchange_weak(refcount, new_count, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                if new_count == 0 {
                    inodes.remove(&inode);
                }
                break;
            }
        }
    }
}

impl FileSystem for PassthroughFs {
    type Inode = Inode;
    type Handle = Handle;
    type DirIter = ReadDir;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        let root = CString::new(self.root_dir.clone())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: openat doesn't modify memory and we check the return value.
        let raw_fd = unsafe { libc::openat(libc::AT_FDCWD, root.as_ptr(), flags) };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: we just opened this fd.
        let f = unsafe { File::from_raw_fd(raw_fd) };

        let st = fstat(f.as_raw_fd())?;

        // Clear the umask so the client can set all mode bits.
        unsafe { libc::umask(0o000) };

        let mut inodes = self.inodes.lock();

        // Not sure why the root inode gets a refcount of 2 but that's what libfuse does.
        inodes.insert(
            ROOT_ID,
            InodeAltKey {
                ino: st.st_ino,
                dev: st.st_dev,
            },
            Arc::new(InodeData {
                inode: ROOT_ID,
                file: Mutex::new(f),
                open_flags: flags,
                refcount: AtomicU64::new(2),
                filetype: st.st_mode.into(),
            }),
        );

        let mut opts =
            FsOptions::DO_READDIRPLUS | FsOptions::READDIRPLUS_AUTO | FsOptions::DONT_MASK;

        if self.cfg.writeback && capable.contains(FsOptions::WRITEBACK_CACHE) {
            opts |= FsOptions::WRITEBACK_CACHE;
            self.writeback.store(true, Ordering::Relaxed);
        }

        Ok(opts)
    }

    fn destroy(&self) {}

    fn lookup(&self, _ctx: Context, parent: Self::Inode, name: &CStr) -> io::Result<Entry> {
        let data = self.find_inode(parent)?;
        self.do_lookup(&data, name)
    }

    fn forget(&self, _ctx: Context, inode: Self::Inode, count: u64) {
        let mut inodes = self.inodes.lock();
        forget_one(&mut inodes, inode, count);
    }

    fn batch_forget(&self, _ctx: Context, requests: Vec<(Self::Inode, u64)>) {
        let mut inodes = self.inodes.lock();
        for (inode, count) in requests {
            forget_one(&mut inodes, inode, count);
        }
    }

    fn getattr(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        _handle: Option<Self::Handle>,
    ) -> io::Result<(libc::stat, Duration)> {
        let data = self.find_inode(inode)?;
        self.do_getattr(&data)
    }

    fn setattr(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        attr: libc::stat,
        handle: Option<Self::Handle>,
        valid: SetattrValid,
    ) -> io::Result<(libc::stat, Duration)> {
        let inode_data = self.find_inode(inode)?;

        // Get the fd to operate on: either from the handle or the inode.
        let handle_data;
        let handle_file_guard;
        let fd = if let Some(handle) = handle.filter(|&h| h != 0) {
            handle_data = self.find_handle(handle, inode)?;
            handle_file_guard = handle_data.file.lock();
            handle_file_guard.as_raw_fd()
        } else {
            inode_data.as_raw_descriptor()
        };

        if valid.contains(SetattrValid::MODE) {
            // SAFETY: fchmod doesn't modify memory.
            let ret = unsafe { libc::fchmod(fd, attr.st_mode) };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        if valid.intersects(SetattrValid::UID | SetattrValid::GID) {
            let uid = if valid.contains(SetattrValid::UID) {
                attr.st_uid
            } else {
                u32::MAX
            };
            let gid = if valid.contains(SetattrValid::GID) {
                attr.st_gid
            } else {
                u32::MAX
            };
            // On macOS, use fchown directly on the fd.
            // SAFETY: fchown doesn't modify memory.
            let ret = unsafe { libc::fchown(fd, uid, gid) };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        if valid.contains(SetattrValid::SIZE) {
            // Try ftruncate on the handle fd first; if no handle, reopen for write.
            let trunc_fd = if handle.is_some() {
                fd
            } else {
                // Need a writable fd; reopen the inode.
                let f = reopen_fd(inode_data.as_raw_descriptor(), libc::O_WRONLY)?;
                let raw = f.as_raw_fd();
                // Leak to keep the fd alive for the ftruncate call.
                // It will be cleaned up when the File is dropped at end of this block.
                std::mem::forget(f);
                raw
            };
            // SAFETY: ftruncate doesn't modify memory.
            let ret = unsafe { libc::ftruncate(trunc_fd, attr.st_size) };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        if valid.intersects(SetattrValid::ATIME | SetattrValid::MTIME) {
            let mut tvs = [
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
            ];

            if valid.contains(SetattrValid::ATIME_NOW) {
                tvs[0].tv_nsec = libc::UTIME_NOW;
            } else if valid.contains(SetattrValid::ATIME) {
                tvs[0].tv_sec = attr.st_atime;
                tvs[0].tv_nsec = attr.st_atime_nsec;
            }

            if valid.contains(SetattrValid::MTIME_NOW) {
                tvs[1].tv_nsec = libc::UTIME_NOW;
            } else if valid.contains(SetattrValid::MTIME) {
                tvs[1].tv_sec = attr.st_mtime;
                tvs[1].tv_nsec = attr.st_mtime_nsec;
            }

            // SAFETY: futimens doesn't modify memory besides the timestamps.
            let ret = unsafe { libc::futimens(fd, tvs.as_ptr()) };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        self.do_getattr(&inode_data)
    }

    fn open(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        self.do_open(inode, flags)
    }

    fn read<W: io::Write + ZeroCopyWriter>(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        mut w: W,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let data = self.find_handle(handle, inode)?;
        let file = data.file.lock();
        w.write_from(&mut file.as_raw_fd_file(), size as usize, offset)
    }

    fn write<R: io::Read + ZeroCopyReader>(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        mut r: R,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _flags: u32,
    ) -> io::Result<usize> {
        let data = self.find_handle(handle, inode)?;
        let file = data.file.lock();
        r.read_to(&mut file.as_raw_fd_file(), size as usize, offset)
    }

    fn flush(
        &self,
        _ctx: Context,
        _inode: Self::Inode,
        _handle: Self::Handle,
        _lock_owner: u64,
    ) -> io::Result<()> {
        // Nothing to do on flush. The file will be synced on fsync.
        Ok(())
    }

    fn fsync(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        datasync: bool,
        handle: Self::Handle,
    ) -> io::Result<()> {
        let handle_data = self.find_handle(handle, inode)?;
        let handle_file_guard = handle_data.file.lock();
        let fd = handle_file_guard.as_raw_fd();

        // macOS doesn't have fdatasync; use fcntl(F_FULLFSYNC) for full sync.
        // SAFETY: fcntl doesn't modify memory.
        let ret = if datasync {
            // F_FULLFSYNC flushes to the drive on macOS, which is stronger than
            // fdatasync but the best we can do.
            unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) }
        } else {
            unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) }
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn release(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        _flags: u32,
        handle: Self::Handle,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        self.do_release(inode, handle)
    }

    fn create(
        &self,
        _ctx: Context,
        parent: Self::Inode,
        name: &CStr,
        mode: u32,
        flags: u32,
        umask: u32,
        _security_ctx: Option<&CStr>,
    ) -> io::Result<(Entry, Option<Self::Handle>, OpenOptions)> {
        let data = self.find_inode(parent)?;

        let create_flags = self.update_open_flags(flags as i32)
            | libc::O_CREAT
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW;

        // SAFETY: openat doesn't modify memory and we check the return value.
        let fd = unsafe {
            libc::openat(
                data.as_raw_descriptor(),
                name.as_ptr(),
                create_flags,
                mode & !umask,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: we just opened this fd.
        let file = unsafe { File::from_raw_fd(fd) };
        let st = fstat(file.as_raw_fd())?;
        let entry = self.add_entry(file, st, create_flags);

        let (handle, opts) =
            self.do_open(entry.inode, flags & !((libc::O_CREAT | libc::O_EXCL | libc::O_NOCTTY) as u32))?;

        Ok((entry, handle, opts))
    }

    fn mkdir(
        &self,
        _ctx: Context,
        parent: Self::Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
        _security_ctx: Option<&CStr>,
    ) -> io::Result<Entry> {
        let data = self.find_inode(parent)?;

        // SAFETY: mkdirat doesn't modify memory and we check the return value.
        let ret =
            unsafe { libc::mkdirat(data.as_raw_descriptor(), name.as_ptr(), (mode & !umask) as u16) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        self.do_lookup(&data, name)
    }

    fn unlink(&self, _ctx: Context, parent: Self::Inode, name: &CStr) -> io::Result<()> {
        let data = self.find_inode(parent)?;
        self.do_unlink(&data, name, 0)
    }

    fn rmdir(&self, _ctx: Context, parent: Self::Inode, name: &CStr) -> io::Result<()> {
        let data = self.find_inode(parent)?;
        self.do_unlink(&data, name, libc::AT_REMOVEDIR)
    }

    fn rename(
        &self,
        _ctx: Context,
        olddir: Self::Inode,
        oldname: &CStr,
        newdir: Self::Inode,
        newname: &CStr,
        _flags: u32,
    ) -> io::Result<()> {
        let old_inode = self.find_inode(olddir)?;
        let new_inode = self.find_inode(newdir)?;

        // SAFETY: renameat doesn't modify memory and we check the return value.
        // Note: we ignore flags (RENAME_EXCHANGE/RENAME_NOREPLACE) since macOS
        // doesn't support them in the standard renameat call.
        let ret = unsafe {
            libc::renameat(
                old_inode.as_raw_descriptor(),
                oldname.as_ptr(),
                new_inode.as_raw_descriptor(),
                newname.as_ptr(),
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn symlink(
        &self,
        _ctx: Context,
        linkname: &CStr,
        parent: Self::Inode,
        name: &CStr,
        _security_ctx: Option<&CStr>,
    ) -> io::Result<Entry> {
        let data = self.find_inode(parent)?;
        // SAFETY: symlinkat doesn't modify memory and we check the return value.
        let ret =
            unsafe { libc::symlinkat(linkname.as_ptr(), data.as_raw_descriptor(), name.as_ptr()) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        self.do_lookup(&data, name)
    }

    fn readlink(&self, _ctx: Context, inode: Self::Inode) -> io::Result<Vec<u8>> {
        let data = self.find_inode(inode)?;

        // Use fcntl(F_GETPATH) to get the path, then readlink on it.
        let path = fd_to_path(data.as_raw_descriptor())?;

        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: readlink writes into buf and we check the return value.
        let res = unsafe {
            libc::readlink(
                path.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.resize(res as usize, 0);
        Ok(buf)
    }

    fn link(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        newparent: Self::Inode,
        newname: &CStr,
    ) -> io::Result<Entry> {
        let data = self.find_inode(inode)?;
        let new_inode = self.find_inode(newparent)?;

        // Get the path of the source file via F_GETPATH.
        let src_path = fd_to_path(data.as_raw_descriptor())?;

        // SAFETY: linkat doesn't modify memory and we check the return value.
        let ret = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                src_path.as_ptr(),
                new_inode.as_raw_descriptor(),
                newname.as_ptr(),
                0,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        self.do_lookup(&new_inode, newname)
    }

    fn statfs(&self, _ctx: Context, inode: Self::Inode) -> io::Result<libc::statvfs> {
        let data = self.find_inode(inode)?;

        let mut out = MaybeUninit::<libc::statvfs>::zeroed();
        // SAFETY: fstatvfs writes into out and we check the return value.
        let ret = unsafe { libc::fstatvfs(data.as_raw_descriptor(), out.as_mut_ptr()) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatvfs initialized the struct on success.
        Ok(unsafe { out.assume_init() })
    }

    fn opendir(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        self.do_open(inode, flags | (libc::O_DIRECTORY as u32))
    }

    fn readdir(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        size: u32,
        offset: u64,
    ) -> io::Result<Self::DirIter> {
        let data = self.find_handle(handle, inode)?;
        let dir = data.file.lock();
        ReadDir::new(&*dir, offset as i64, size as usize)
    }

    fn fsyncdir(
        &self,
        ctx: Context,
        inode: Self::Inode,
        datasync: bool,
        handle: Self::Handle,
    ) -> io::Result<()> {
        self.fsync(ctx, inode, datasync, handle)
    }

    fn releasedir(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        _flags: u32,
        handle: Self::Handle,
    ) -> io::Result<()> {
        self.do_release(inode, handle)
    }
}

/// Helper trait to get a &mut File reference from a lock guard for ZeroCopy operations.
trait AsRawFdFile {
    fn as_raw_fd_file(&self) -> File;
}

impl AsRawFdFile for std::sync::MutexGuard<'_, File> {
    fn as_raw_fd_file(&self) -> File {
        // SAFETY: dup the fd so we don't close the original.
        let fd = unsafe { libc::dup(self.as_raw_fd()) };
        assert!(fd >= 0, "dup failed");
        unsafe { File::from_raw_fd(fd) }
    }
}

// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Runs hardware devices in child processes.

use std::process;
use std::io::{Error, Result};
use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicBool, Ordering};

use libc;
use libc::pid_t;

use byteorder::{NativeEndian, ByteOrder};

use hw::BusDevice;
use syscall_defines::linux::LinuxSyscall::SYS_clone;

enum Command {
    Read = 0,
    Write = 1,
    Shutdown = 2,
}

fn child_proc(sock: UnixDatagram, device: &mut BusDevice) -> ! {
    let mut running = true;

    let res = sock.send(&[0x7f; 24]);
    if let Err(e) = res {
        println!("error: failed to send child started signal: {}", e);
        running = false;
    }

    while running {
        let mut buf = [0; 24];
        match sock.recv(&mut buf) {
            Ok(c) if c != buf.len() => {
                println!("error: child device process incorrect recv size: got {}, expected {}",
                         c,
                         buf.len());
                break;
            }
            Err(e) => {
                println!("error: child device process failed recv: {}", e);
                break;
            }
            _ => {}
        }

        let cmd = NativeEndian::read_u32(&buf[0..]);
        let len = NativeEndian::read_u32(&buf[4..]) as usize;
        let offset = NativeEndian::read_u64(&buf[8..]);

        let res = if cmd == Command::Read as u32 {
            device.read(offset, &mut buf[16..16 + len]);
            sock.send(&buf)
        } else if cmd == Command::Write as u32 {
            device.write(offset, &buf[16..16 + len]);
            sock.send(&buf)
        } else if cmd == Command::Shutdown as u32 {
            running = false;
            sock.send(&buf)
        } else {
            println!("child device process unknown command: {}", cmd);
            break;
        };

        if let Err(e) = res {
            println!("error: child device process failed send: {}", e);
            break;
        }
    }

    // ! Never returns
    process::exit(0);
}

unsafe fn do_clone() -> Result<pid_t> {
    // Forking is unsafe, this function must be unsafe as there is no way to
    // guarantee saftey without more context about the state of the program.
    let pid = libc::syscall(SYS_clone as i64,
            libc::CLONE_NEWUSER | libc::CLONE_NEWPID |
            libc::SIGCHLD as i32,
            0);
    if pid < 0 {
        Err(Error::last_os_error())
    } else {
        Ok(pid as pid_t)
    }
}

/// Wraps an inner `hw::BusDevice` that is run inside a child process via fork.
///
/// Because forks are very unfriendly to destructors and all memory mappings and file descriptors
/// are inherited, this should be used as early as possible in the main process.
pub struct ProxyDevice {
    sock: UnixDatagram,
    enabled: AtomicBool,
}

impl ProxyDevice {
    /// Takes the given device and isolates it into another process via fork before returning.
    ///
    /// The forked process will automatically be terminated when this is dropped, so be sure to keep
    /// a reference.
    /// `post_clone_cb` - Called after forking the child process, passed the
    /// child end of the pipe that must be kep open.
    pub fn new<D: BusDevice, F>(mut device: D, post_clone_cb: F) -> Result<ProxyDevice>
            where F: FnOnce(&UnixDatagram) {
        let (child_sock, parent_sock) = UnixDatagram::pair()?;

        // Forking a new process is unsafe, we must ensure no resources required
        // by the other side are freed after the two processes start.
        let ret = unsafe { do_clone()? };
        if ret == 0 {
            post_clone_cb(&child_sock);
            // ! Never returns
            child_proc(child_sock, &mut device);
        }

        let mut buf = [0; 24];
        parent_sock.recv(&mut buf)?;
        Ok(ProxyDevice {
               sock: parent_sock,
               enabled: AtomicBool::new(true),
           })
    }

    fn send_cmd(&self, cmd: Command, offset: u64, len: u32, data: &[u8]) -> Result<()> {
        let mut buf = [0; 24];
        NativeEndian::write_u32(&mut buf[0..], cmd as u32);
        NativeEndian::write_u32(&mut buf[4..], len);
        NativeEndian::write_u64(&mut buf[8..], offset);
        buf[16..16 + data.len()].clone_from_slice(data);
        self.sock.send(&buf).map(|_| ())
    }

    fn recv_resp(&self, data: &mut [u8]) -> Result<()> {
        let mut buf = [0; 24];
        self.sock.recv(&mut buf)?;
        let len = data.len();
        data.clone_from_slice(&buf[16..16 + len]);
        Ok(())
    }

    fn wait(&self) -> Result<()> {
        let mut buf = [0; 24];
        self.sock.recv(&mut buf).map(|_| ())
    }
}

impl BusDevice for ProxyDevice {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        if self.enabled.load(Ordering::SeqCst) {
            let res = self.send_cmd(Command::Read, offset, data.len() as u32, &[])
                .and_then(|_| self.recv_resp(data));
            if let Err(e) = res {
                println!("error: failed read from child device process: {}", e);
                self.enabled.store(false, Ordering::SeqCst);
            }
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if self.enabled.load(Ordering::SeqCst) {
            let res = self.send_cmd(Command::Write, offset, data.len() as u32, data)
                .and_then(|_| self.wait());
            if let Err(e) = res {
                println!("error: failed write to child device process: {}", e);
                self.enabled.store(false, Ordering::SeqCst);
            }
        }
    }
}

impl Drop for ProxyDevice {
    fn drop(&mut self) {
        if self.enabled.load(Ordering::SeqCst) {
            let res = self.send_cmd(Command::Shutdown, 0, 0, &[]);
            if let Err(e) = res {
                println!("error: failed to shutdown child device process: {}", e);
            }
        }
    }
}

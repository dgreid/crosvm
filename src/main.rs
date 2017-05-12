// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Runs a virtual machine under KVM

extern crate clap;
extern crate libc;
extern crate io_jail;
extern crate kvm;
extern crate x86_64;
extern crate kernel_loader;
extern crate byteorder;
extern crate syscall_defines;
extern crate sys_util;

use std::io::{stdin, stdout};
use std::thread::spawn;
use std::ffi::{CString, CStr};
use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::string::String;
use std::sync::{Arc, Mutex};

use clap::{Arg, App};

use io_jail::Minijail;
use kvm::*;
use sys_util::{GuestAddress, GuestMemory, EventFd, Terminal, poll, Pollable};

pub mod hw;
pub mod kernel_cmdline;

enum Error {
    BlockDeviceJail(io_jail::Error),
    BlockDevicePivotRoot(io_jail::Error),
    Cmdline(kernel_cmdline::Error),
    Disk(std::io::Error),
    ProxyDeviceCreation(std::io::Error),
    Sys(sys_util::Error),
    VcpuExit(String),
}

impl std::convert::From<sys_util::Error> for Error {
    fn from(e: sys_util::Error) -> Error {
        Error::Sys(e)
    }
}

type Result<T> = std::result::Result<T, Error>;

struct Config {
    disk_path: Option<String>,
    vcpu_count: Option<u32>,
    memory: Option<usize>,
    kernel_image: File,
    params: Option<String>,
    multiprocess: bool,
    warn_unknown_ports: bool,
}

enum VmRequest {
    RegisterIoevent(EventFd, IoeventAddress, u32),
    RegisterIrqfd(EventFd, u32),
}

const KERNEL_START_OFFSET: usize = 0x200000;
const CMDLINE_OFFSET: usize = 0x20000;
const CMDLINE_MAX_SIZE: usize = KERNEL_START_OFFSET - CMDLINE_OFFSET;

fn create_block_device_jail() -> Result<Minijail> {
    // All child jails run in a new user namespace without any users mapped,
    // they run as nobody unless otherwise configured.
    let mut j = Minijail::new().map_err(|e| Error::BlockDeviceJail(e))?;
    // Don't need any capabilities.
    j.use_caps(0);
    // Create a new mount namespace with an empty root FS.
    j.namespace_vfs();
    j.enter_pivot_root(Path::new("/run/asdf"))
        .map_err(|e| Error::BlockDevicePivotRoot(e))?;
    // Run in an empty network namespace.
    j.namespace_net();
    // Apply the block device seccomp policy.
    j.no_new_privs();
    j.parse_seccomp_filters(Path::new("block_device.policy"))
        .map_err(|e| Error::BlockDeviceJail(e))?;
    j.use_seccomp_filter();
    Ok(j)
}

fn run_config(cfg: Config) -> Result<()> {
    let mem_size = cfg.memory.unwrap_or(256) << 20;
    let guest_mem = GuestMemory::new(&x86_64::arch_memory_regions(mem_size)).expect("new mmap failed");

    let mut cmdline = kernel_cmdline::Cmdline::new(CMDLINE_MAX_SIZE);
    cmdline.insert_str("console=ttyS0 noapic noacpi reboot=k panic=1 pci=off").unwrap();

    let mut vm_requests = Vec::new();

    let mut bus = hw::Bus::new();

    let mmio_len = 0x1000;
    let mut mmio_base: u64 = 0xd0000000;
    let mut irq: u32 = 5;

    if let Some(ref disk_path) = cfg.disk_path {
        // List of FDs to keep open in the child after it forks.
        let mut keep_fds: Vec<RawFd> = Vec::new();

        let disk_image = File::open(disk_path).map_err(|e| Error::Disk(e))?;
        keep_fds.push(disk_image.as_raw_fd());

        let block_box = Box::new(hw::virtio::Block::new(disk_image).map_err(|e| Error::Disk(e))?);
        let block_mmio = hw::virtio::MmioDevice::new(guest_mem.clone(), block_box)?;
        for (i, queue_evt) in block_mmio.queue_evts().iter().enumerate() {
            let io_addr = IoeventAddress::Mmio(mmio_base + hw::virtio::NOITFY_REG_OFFSET as u64);
            vm_requests.push(VmRequest::RegisterIoevent(queue_evt.try_clone()?, io_addr, i as u32));
            keep_fds.push(queue_evt.as_raw_fd());
        }

        if let Some(interrupt_evt) = block_mmio.interrupt_evt() {
            vm_requests.push(VmRequest::RegisterIrqfd(interrupt_evt.try_clone()?, irq));
            keep_fds.push(interrupt_evt.as_raw_fd());
        }

        if cfg.multiprocess {
            let jail = create_block_device_jail()?;
            let proxy_dev = hw::ProxyDevice::new(block_mmio, move |keep_pipe| {
                keep_fds.push(keep_pipe.as_raw_fd());
                // Need to panic here as there isn't a way to recover from a
                // partly-jailed process.
                unsafe {
                    // This is OK as we have whitelisted all the FDs we need open.
                    jail.enter(Some(&keep_fds)).unwrap();
                }
            }).map_err(|e| Error::ProxyDeviceCreation(e))?;
            bus.insert(Arc::new(Mutex::new(proxy_dev)), mmio_base, mmio_len);
        } else {
            bus.insert(Arc::new(Mutex::new(block_mmio)), mmio_base, mmio_len);
        }

        cmdline.insert("virtio_mmio.device", &format!("4K@0x{:08x}:{}", mmio_base, irq)).unwrap();
        cmdline.insert("root", "/dev/vda").unwrap();
        mmio_base += mmio_len;
        irq += 1;
    }

    if let Some(params) = cfg.params {
        cmdline.insert_str(params).map_err(|e| Error::Cmdline(e))?;
    }

    run_kvm(vm_requests, cfg.kernel_image, &CString::new(cmdline).unwrap(), guest_mem, bus, cfg.warn_unknown_ports)
}

fn run_kvm(requests: Vec<VmRequest>,
           mut kernel_image: File,
           cmdline: &CStr,
           guest_mem: GuestMemory,
           mmio_bus: hw::Bus,
           warn_unknown_ports: bool) -> Result<()> {
    let kvm = Kvm::new().expect("new kvm failed");
    let tss_addr = GuestAddress::new(0xfffbd000);
    let kernel_start_addr = GuestAddress::new(KERNEL_START_OFFSET);
    let cmdline_addr = GuestAddress::new(CMDLINE_OFFSET);

    let vm = Vm::new(&kvm, guest_mem).expect("new vm failed");
    vm.set_tss_addr(tss_addr).expect("set tss addr failed");
    vm.create_pit().expect("create pit failed");
    vm.create_irq_chip().expect("create irq chip failed");

    for request in requests {
        match request {
            VmRequest::RegisterIoevent(evt, addr, datamatch) => vm.register_ioevent(&evt, addr, datamatch).unwrap(),
            VmRequest::RegisterIrqfd(evt, irq) => vm.register_irqfd(&evt, irq).unwrap(),
        }
    }

    let vcpu = Vcpu::new(0, &kvm, &vm).expect("new vcpu failed");

    kernel_loader::load_kernel(vm.get_memory(),
                               kernel_start_addr,
                               &mut kernel_image)
            .expect("failed to load kernel");
    kernel_loader::load_cmdline(vm.get_memory(),
                                cmdline_addr,
                                cmdline)
            .expect("failed to load kernel");
    x86_64::configure_system(vm.get_memory(),
                             kernel_start_addr,
                             cmdline_addr,
                             cmdline.to_bytes().len() + 1)
            .unwrap();
    // TODO(dgreid) - call configure_vcpu from vcpu thread.
    x86_64::configure_vcpu(vm.get_memory(),
                           kernel_start_addr,
                           &kvm,
                           &vcpu,
                           1)
            .unwrap();

    let mut io_bus = hw::Bus::new();

    struct NoDevice;
    impl hw::BusDevice for NoDevice {}

    let com_evt_1_3 = EventFd::new().expect("failed to create eventfd");
    let com_evt_2_4 = EventFd::new().expect("failed to create eventfd");
    let stdio_serial = Arc::new(Mutex::new(hw::Serial::new_out(com_evt_1_3.try_clone().unwrap(), Box::new(stdout()))));
    let nul_device = Arc::new(Mutex::new(NoDevice));
    io_bus.insert(stdio_serial.clone(), 0x3f8, 0x8);
    io_bus.insert(Arc::new(Mutex::new(hw::Serial::new_sink(com_evt_2_4.try_clone().unwrap()))), 0x2f8, 0x8);
    io_bus.insert(Arc::new(Mutex::new(hw::Serial::new_sink(com_evt_1_3.try_clone().unwrap()))), 0x3e8, 0x8);
    io_bus.insert(Arc::new(Mutex::new(hw::Serial::new_sink(com_evt_2_4.try_clone().unwrap()))), 0x2e8, 0x8);
    io_bus.insert(Arc::new(Mutex::new(hw::Cmos::new())), 0x70, 0x2);
    io_bus.insert(nul_device.clone(), 0x061, 0x4); // ignore kb
    io_bus.insert(nul_device.clone(), 0x040, 0x8); // ignore pit
    io_bus.insert(nul_device.clone(), 0x0ed, 0x1); // most likely this one does nothing
    io_bus.insert(nul_device.clone(), 0x0f0, 0x2); // ignore fpu
    io_bus.insert(nul_device.clone(), 0xcf8, 0x8); // ignore pci

    vm.register_irqfd(&com_evt_1_3, 4).expect("failed to register IRQ#4");
    vm.register_irqfd(&com_evt_2_4, 3).expect("failed to register IRQ#3");

    let exit_evt = EventFd::new().expect("failed to create exit eventfd");
    let exit_evt_clone = exit_evt.try_clone()?;

    let control_join_handle = spawn(move|| {
        let stdin_handle = stdin();
        let stdin_lock = stdin_handle.lock();
        stdin_lock.set_raw_mode().expect("failed to set terminal raw mode");
        stdin_lock.set_non_block(true).expect("failed to set terminal non-blocking");

        'poll: loop {
            let poll_res = {
                let poll_array = &[&exit_evt_clone as &Pollable, &stdin_lock];
                match poll(poll_array) {
                    Ok(v) => v,
                    Err(e) => {
                        println!("failed to poll: {:?}", e);
                        break;
                    }
                }
            };
            for i in poll_res {
                match i {
                    0 => break 'poll,
                    1 => {
                        let mut out = [0u8; 64];
                        loop {
                            let count = stdin_lock.read_raw(&mut out[..]).unwrap_or_default();
                            if count == 0 {
                                break
                            }
                            stdio_serial.lock().unwrap().queue_bytes(&out[..count]).expect("failed to queue bytes into serial port");
                        }
                    },
                    _ => {}
                }
            }
        }

        stdin_lock.set_canon_mode().expect("failed to restore canonical mode for terminal");
    });

    loop {
        match vcpu.run().expect("run failed") {
            VcpuExit::IoIn(addr, data) => {
                if !io_bus.read(addr as u64, data) && warn_unknown_ports {
                    println!("warning: unhandled I/O port {}-bit read at 0x{:03x}",  data.len() << 3, addr);
                }
            },

            VcpuExit::IoOut(addr, data) => {
                if !io_bus.write(addr as u64, data) && warn_unknown_ports {
                    println!("warning: unhandled I/O port {}-bit write at 0x{:03x}",  data.len() << 3, addr);
                }
            },

            VcpuExit::MmioRead(addr, data) => {
                if !mmio_bus.read(addr, data) && warn_unknown_ports {
                    println!("warning: unhandled mmio {}-bit read at 0x{:08x}",  data.len() << 3, addr);
                }
            },

            VcpuExit::MmioWrite(addr, data) => {
                if !mmio_bus.write(addr, data) && warn_unknown_ports {
                    println!("warning: unhandled mmio {}-bit write at 0x{:08x}",  data.len() << 3, addr);
                }
            },

            VcpuExit::Hlt => break,
            VcpuExit::Shutdown => break,
            r => return Err(Error::VcpuExit(format!("{:?}", r))),
        }
    }

    exit_evt.write(1)?;
    control_join_handle.join().expect("failed to join with control thread");

    Ok(())
}

fn main() {
    let matches = App::new("crosvm")
        .version("0.1.0")
        .author("The Chromium OS Authors")
        .about("Runs a virtual machine under KVM")
        .arg(Arg::with_name("disk")
                 .short("d")
                 .long("disk")
                 .value_name("FILE")
                 .help("rootfs disk image")
                 .takes_value(true))
        .arg(Arg::with_name("cpus")
                 .short("c")
                 .long("cpus")
                 .value_name("N")
                 .help("number of VCPUs (WARNING: CURRENTLY UNUSED)")
                 .takes_value(true))
        .arg(Arg::with_name("memory")
                 .short("m")
                 .long("mem")
                 .value_name("N")
                 .help("amount of guest memory in MiB")
                 .takes_value(true))
        .arg(Arg::with_name("params")
                 .short("p")
                 .long("params")
                 .value_name("params")
                 .help("extra kernel command line arguments")
                 .takes_value(true))
        .arg(Arg::with_name("multiprocess")
            .short("u")
            .long("multiprocess")
            .help("run the devices in a child process"))
        .arg(Arg::with_name("warn-unknown-ports")
            .long("warn-unknown-ports")
            .help("warn when an the VM uses an unknown port"))
        .arg(Arg::with_name("KERNEL")
                 .required(true)
                 .index(1)
                 .help("bzImage of kernel to run"))
        .get_matches();



    let config = Config {
        disk_path: matches.value_of("disk").map(|s| s.to_string()),
        vcpu_count: matches.value_of("cpus").and_then(|v| v.parse().ok()),
        memory: matches.value_of("memory").and_then(|v| v.parse().ok()),
        kernel_image: File::open(matches.value_of("KERNEL").unwrap())
            .expect("Expected kernel image path to be valid"),
        params: matches.value_of("params").map(|s| s.to_string()),
        multiprocess: matches.is_present("multiprocess"),
        warn_unknown_ports: matches.is_present("warn-unknown-ports"),
    };

    match run_config(config) {
        Ok(_) => {},
        Err(e) => {
            match e {
                Error::BlockDeviceJail(e) => println!("failed to jail block device: {:?}", e),
                Error::BlockDevicePivotRoot(e) => {
                    println!("failed to pivot root block device: {:?}", e)
                }
                Error::Cmdline(e) => println!("the given kernel command line was invalid: {}", e),
                Error::Disk(e) => println!("failed to load disk image: {}", e),
                Error::ProxyDeviceCreation(e) => println!("failed to create proxy device: {}", e),
                Error::Sys(e) => println!("error with system call: {:?}", e),
                Error::VcpuExit(s) => println!("unexpected vcpu exit reason: {}", s),
            };
        }
    }
}

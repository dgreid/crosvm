// Copyright 2022 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fs::File;
use std::io::Read;

use cros_fdt::Error;
use cros_fdt::FdtWriter;
use cros_fdt::Result;
use devices::irqchip::aia_aplic_addr;
use devices::irqchip::aia_imsic_size;
use devices::irqchip::AIA_APLIC_SIZE;
use devices::irqchip::AIA_IMSIC_BASE;
use devices::PciAddress;
use devices::PciInterruptPin;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;

// This is the start of DRAM in the physical address space.
use crate::RISCV64_PHYS_MEM_START;

// CPUs are assigned phandles starting with this number.
const PHANDLE_CPU0: u32 = 0x100;

const PHANDLE_AIA_APLIC: u32 = 2;
const PHANDLE_AIA_IMSIC: u32 = 3;
const PHANDLE_CPU_INTC_BASE: u32 = 4;

fn create_memory_node(fdt: &mut FdtWriter, guest_mem: &GuestMemory) -> Result<()> {
    let mem_size = guest_mem.memory_size();
    let mem_reg_prop = [RISCV64_PHYS_MEM_START, mem_size];

    let memory_node = fdt.begin_node("memory")?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &mem_reg_prop)?;
    fdt.end_node(memory_node)?;
    Ok(())
}

fn create_cpu_nodes(fdt: &mut FdtWriter, num_cpus: u32, timebase_frequency: u32) -> Result<()> {
    let cpus_node = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", 0x1)?;
    fdt.property_u32("#size-cells", 0x0)?;
    fdt.property_u32("timebase-frequency", timebase_frequency)?;

    for cpu_id in 0..num_cpus {
        let cpu_name = format!("cpu@{:x}", cpu_id);
        let cpu_node = fdt.begin_node(&cpu_name)?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_string("compatible", "riscv")?;
        fdt.property_string("mmu-type", "sv48")?;
        fdt.property_string("riscv,isa", "rv64iafdcsu_smaia_ssaia")?;
        fdt.property_string("status", "okay")?;
        fdt.property_u32("reg", cpu_id)?;
        fdt.property_u32("phandle", PHANDLE_CPU0 + cpu_id)?;

        // Add interrupt controller node
        let intc_node = fdt.begin_node("interrupt-controller")?;
        fdt.property("compatible", b"riscv,cpu-intc")?;
        fdt.property_u32("#interrupt-cells", 1)?;
        fdt.property_null("interrupt-controller")?;
        fdt.property_u32("phandle", PHANDLE_CPU_INTC_BASE + cpu_id)?;
        fdt.end_node(intc_node)?;

        fdt.end_node(cpu_node)?;
    }

    fdt.end_node(cpus_node)?;
    Ok(())
}

fn create_chosen_node(
    fdt: &mut FdtWriter,
    cmdline: &str,
    initrd: Option<(GuestAddress, usize)>,
) -> Result<()> {
    let chosen_node = fdt.begin_node("chosen")?;
    fdt.property_u32("linux,pci-probe-only", 1)?;
    fdt.property_string("bootargs", cmdline)?;

    let mut random_file = File::open("/dev/urandom").map_err(Error::FdtIoError)?;
    let mut kaslr_seed_bytes = [0u8; 8];
    random_file
        .read_exact(&mut kaslr_seed_bytes)
        .map_err(Error::FdtIoError)?;
    let kaslr_seed = u64::from_le_bytes(kaslr_seed_bytes);
    fdt.property_u64("kaslr-seed", kaslr_seed)?;

    let mut rng_seed_bytes = [0u8; 256];
    random_file
        .read_exact(&mut rng_seed_bytes)
        .map_err(Error::FdtIoError)?;
    fdt.property("rng-seed", &rng_seed_bytes)?;

    if let Some((initrd_addr, initrd_size)) = initrd {
        let initrd_start = initrd_addr.offset() as u64;
        let initrd_end = initrd_start + initrd_size as u64;
        fdt.property_u64("linux,initrd-start", initrd_start)?;
        fdt.property_u64("linux,initrd-end", initrd_end)?;
    }

    fdt.property_string("stdout-path", "U6_16550A@10000000")?;

    fdt.end_node(chosen_node)?;

    Ok(())
}

// num_ids: number of imsic ids from the aia subsystem
// num_sources: number of aplic sources from the aia subsystem
fn create_aia_node(
    fdt: &mut FdtWriter,
    num_cpus: usize,
    num_ids: usize,
    num_sources: usize,
) -> Result<()> {
    let name = format!("imsics@{:#08x}", AIA_IMSIC_BASE);
    let imsic_node = fdt.begin_node(&name)?;
    fdt.property_string("compatible", "riscv,imsics")?;

    let regs = [
        0u32,
        AIA_IMSIC_BASE as u32,
        0,
        aia_imsic_size(num_cpus) as u32,
    ];
    fdt.property_array_u32("reg", &regs)?;
    fdt.property_u32("#interrupt-cells", 0)?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_null("msi-controller")?;
    fdt.property_u32("riscv,num-ids", num_ids as u32)?;
    fdt.property_u32("phandle", PHANDLE_AIA_IMSIC)?;

    const S_MODE_EXT_IRQ: u32 = 9;
    let mut cpu_intc_regs: Vec<u32> = Vec::with_capacity(num_cpus * 2);
    for hart in 0..num_cpus {
        cpu_intc_regs.push(PHANDLE_CPU_INTC_BASE + hart as u32);
        cpu_intc_regs.push(S_MODE_EXT_IRQ);
    }
    fdt.property_array_u32("interrupts-extended", &cpu_intc_regs)?;

    if (num_ids > num_sources) && (num_ids - num_sources) >= 7 {
        let cpu_intc_regs = vec![1u32, 7];
        fdt.property_array_u32("riscv,ipi-range", &cpu_intc_regs)?;
    }
    fdt.end_node(imsic_node)?;

    /* Skip APLIC node if we have no interrupt sources */
    if num_sources == 0 {
        return Ok(());
    }

    let name = format!("aplic@{:#08x}", aia_aplic_addr(num_cpus));
    let aplic_node = fdt.begin_node(&name)?;
    fdt.property_string("compatible", "riscv,aplic")?;

    let regs = [
        0u32,
        aia_aplic_addr(num_cpus) as u32,
        0,
        AIA_APLIC_SIZE as u32,
    ];
    fdt.property_array_u32("reg", &regs)?;
    fdt.property_u32("#interrupt-cells", 2)?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_u32("riscv,num-sources", num_sources as u32)?;
    fdt.property_u32("phandle", PHANDLE_AIA_APLIC)?;
    fdt.property_u32("msi-parent", PHANDLE_AIA_IMSIC)?;
    fdt.end_node(aplic_node)?;

    Ok(())
}

/// PCI host controller address range.
///
/// This represents a single entry in the "ranges" property for a PCI host controller.
///
/// See [PCI Bus Binding to Open Firmware](https://www.openfirmware.info/data/docs/bus.pci.pdf)
/// and https://www.kernel.org/doc/Documentation/devicetree/bindings/pci/host-generic-pci.txt
/// for more information.
#[derive(Copy, Clone)]
pub struct PciRange {
    pub space: PciAddressSpace,
    pub bus_address: u64,
    pub cpu_physical_address: u64,
    pub size: u64,
    pub prefetchable: bool,
}

/// PCI address space.
#[derive(Copy, Clone)]
#[allow(dead_code)]
pub enum PciAddressSpace {
    /// PCI configuration space
    Configuration = 0b00,
    /// I/O space
    Io = 0b01,
    /// 32-bit memory space
    Memory = 0b10,
    /// 64-bit memory space
    Memory64 = 0b11,
}

/// Location of memory-mapped PCI configuration space.
#[derive(Copy, Clone)]
pub struct PciConfigRegion {
    /// Physical address of the base of the memory-mapped PCI configuration region.
    pub base: u64,
    /// Size of the PCI configuration region in bytes.
    pub size: u64,
}

fn create_pci_nodes(
    fdt: &mut FdtWriter,
    pci_irqs: Vec<(PciAddress, u32, PciInterruptPin)>,
    cfg: PciConfigRegion,
    ranges: &[PciRange],
) -> Result<()> {
    // Add devicetree nodes describing a PCI generic host controller.
    // See Documentation/devicetree/bindings/pci/host-generic-pci.txt in the kernel
    // and "PCI Bus Binding to IEEE Std 1275-1994".
    let ranges: Vec<u32> = ranges
        .iter()
        .map(|r| {
            let ss = r.space as u32;
            let p = r.prefetchable as u32;
            [
                // BUS_ADDRESS(3) encoded as defined in OF PCI Bus Binding
                (ss << 24) | (p << 30),
                (r.bus_address >> 32) as u32,
                r.bus_address as u32,
                // CPU_PHYSICAL(2)
                (r.cpu_physical_address >> 32) as u32,
                r.cpu_physical_address as u32,
                // SIZE(2)
                (r.size >> 32) as u32,
                r.size as u32,
            ]
        })
        .flatten()
        .collect();

    let bus_range = [0, 0]; // Only bus 0
    let reg = [cfg.base, cfg.size];

    const IRQ_TYPE_LEVEL_HIGH: u32 = 0x00000004;
    let mut interrupts: Vec<u32> = Vec::new();
    let mut masks: Vec<u32> = Vec::new();

    for (address, irq_num, irq_pin) in pci_irqs.iter() {
        // PCI_DEVICE(3)
        interrupts.push(address.to_config_address(0, 8));
        interrupts.push(0);
        interrupts.push(0);

        // INT#(1)
        interrupts.push(irq_pin.to_mask() + 1);

        // INTERRUPT INFO
        interrupts.push(PHANDLE_AIA_APLIC);
        interrupts.push(*irq_num);
        interrupts.push(IRQ_TYPE_LEVEL_HIGH);

        // PCI_DEVICE(3)
        masks.push(0xf800); // bits 11..15 (device)
        masks.push(0);
        masks.push(0);

        // INT#(1)
        masks.push(0x7); // allow INTA#-INTD# (1 | 2 | 3 | 4)
    }

    let pci_node = fdt.begin_node("pci")?;
    fdt.property_string("compatible", "pci-host-cam-generic")?;
    fdt.property_string("device_type", "pci")?;
    fdt.property_array_u32("ranges", &ranges)?;
    fdt.property_array_u32("bus-range", &bus_range)?;
    fdt.property_u32("#address-cells", 3)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_array_u64("reg", &reg)?;
    fdt.property_u32("#interrupt-cells", 1)?;
    fdt.property_array_u32("interrupt-map", &interrupts)?;
    fdt.property_array_u32("interrupt-map-mask", &masks)?;
    fdt.property_u32("msi-parent", PHANDLE_AIA_IMSIC)?;
    fdt.property_null("dma-coherent")?;
    fdt.end_node(pci_node)?;

    Ok(())
}

/// Creates a flattened device tree containing all of the parameters for the
/// kernel and loads it into the guest memory at the specified offset.
///
/// # Arguments
///
/// * `fdt_max_size` - The amount of space reserved for the device tree
/// * `guest_mem` - The guest memory object
/// * `pci_irqs` - List of PCI device address to PCI interrupt number and pin mappings
/// * `pci_cfg` - Location of the memory-mapped PCI configuration space.
/// * `pci_ranges` - Memory ranges accessible via the PCI host controller.
/// * `num_cpus` - Number of virtual CPUs the guest will have
/// * `fdt_load_offset` - The offset into physical memory for the device tree
/// * `cmdline` - The kernel commandline
/// * `initrd` - An optional tuple of initrd guest physical address and size
/// * `timebase_frequency` - The time base frequency for the VM.
pub fn create_fdt(
    fdt_max_size: usize,
    guest_mem: &GuestMemory,
    pci_irqs: Vec<(PciAddress, u32, PciInterruptPin)>,
    pci_cfg: PciConfigRegion,
    pci_ranges: &[PciRange],
    num_cpus: u32,
    fdt_load_offset: u64,
    aia_num_ids: usize,
    aia_num_sources: usize,
    cmdline: &str,
    initrd: Option<(GuestAddress, usize)>,
    timebase_frequency: u32,
) -> Result<()> {
    let mut fdt = FdtWriter::new(&[]);

    // The whole thing is put into one giant node with some top level properties
    let root_node = fdt.begin_node("")?;
    fdt.property_string("compatible", "linux,dummy-virt")?;
    fdt.property_u32("#address-cells", 0x2)?;
    fdt.property_u32("#size-cells", 0x2)?;
    create_chosen_node(&mut fdt, cmdline, initrd)?;
    create_memory_node(&mut fdt, guest_mem)?;
    create_cpu_nodes(&mut fdt, num_cpus, timebase_frequency)?;
    create_aia_node(&mut fdt, num_cpus as usize, aia_num_ids, aia_num_sources)?;
    create_pci_nodes(&mut fdt, pci_irqs, pci_cfg, pci_ranges)?;

    // End giant node
    fdt.end_node(root_node)?;

    let fdt_final = fdt.finish(fdt_max_size)?;

    let fdt_address = GuestAddress(RISCV64_PHYS_MEM_START + fdt_load_offset);
    let written = guest_mem
        .write_at_addr(fdt_final.as_slice(), fdt_address)
        .map_err(|_| Error::FdtGuestMemoryWriteError)?;
    if written < fdt_max_size {
        return Err(Error::FdtGuestMemoryWriteError);
    }
    Ok(())
}

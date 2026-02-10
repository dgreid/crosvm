// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS-specific memory management tests for vm_memory.
//!
//! These tests verify that GuestMemory and MemoryMapping work correctly
//! on macOS. They cover allocation, read/write operations, and memory mapping.

#![cfg(target_os = "macos")]

use base::MappedRegion;
use base::MemoryMappingBuilder;
use base::SharedMemory;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;

/// Test that GuestMemory can be allocated with a single region.
#[test]
fn test_guest_memory_new_single_region() {
    let start_addr = GuestAddress(0x1000);
    let size: u64 = 0x10000; // 64KB, page-aligned

    let result = GuestMemory::new(&[(start_addr, size)]);
    assert!(result.is_ok(), "Failed to create GuestMemory: {:?}", result.err());

    let guest_mem = result.unwrap();
    assert_eq!(guest_mem.memory_size(), size);
    assert_eq!(guest_mem.num_regions(), 1);
    assert_eq!(guest_mem.end_addr(), start_addr.checked_add(size).unwrap());
}

/// Test that GuestMemory can be allocated with multiple regions.
#[test]
fn test_guest_memory_new_multiple_regions() {
    let regions = &[
        (GuestAddress(0x0), 0x10000),      // 64KB at 0
        (GuestAddress(0x20000), 0x20000),  // 128KB at 128KB
    ];

    let result = GuestMemory::new(regions);
    assert!(result.is_ok(), "Failed to create GuestMemory with multiple regions: {:?}", result.err());

    let guest_mem = result.unwrap();
    assert_eq!(guest_mem.memory_size(), 0x10000 + 0x20000);
    assert_eq!(guest_mem.num_regions(), 2);
}

/// Test various memory sizes including typical VM memory configurations.
#[test]
fn test_guest_memory_various_sizes() {
    // Test small size (1 page = 4KB on most systems, but we use 16KB for alignment safety)
    let small_size = 0x10000; // 64KB
    let small_result = GuestMemory::new(&[(GuestAddress(0), small_size)]);
    assert!(small_result.is_ok(), "Failed to allocate small memory: {:?}", small_result.err());

    // Test medium size (1MB)
    let medium_size = 0x100000;
    let medium_result = GuestMemory::new(&[(GuestAddress(0), medium_size)]);
    assert!(medium_result.is_ok(), "Failed to allocate medium memory: {:?}", medium_result.err());

    // Test larger size (16MB) - typical for small VMs
    let large_size = 0x1000000;
    let large_result = GuestMemory::new(&[(GuestAddress(0), large_size)]);
    assert!(large_result.is_ok(), "Failed to allocate larger memory: {:?}", large_result.err());
}

/// Test that overlapping memory regions are rejected.
#[test]
fn test_guest_memory_overlap_rejected() {
    let regions = &[
        (GuestAddress(0x0), 0x20000),
        (GuestAddress(0x10000), 0x10000), // Overlaps with first region
    ];

    let result = GuestMemory::new(regions);
    assert!(result.is_err(), "Overlapping regions should be rejected");
}

/// Test that non-page-aligned sizes are rejected.
#[test]
fn test_guest_memory_alignment() {
    // Non-page-aligned size should fail
    let unaligned_size = 0x1001; // Not page aligned
    let result = GuestMemory::new(&[(GuestAddress(0x0), unaligned_size)]);
    assert!(result.is_err(), "Non-page-aligned size should be rejected");
}

/// Test writing and reading data to/from guest memory.
#[test]
fn test_guest_memory_read_write() {
    let start_addr = GuestAddress(0x1000);
    let size: u64 = 0x10000;

    let guest_mem = GuestMemory::new(&[(start_addr, size)]).expect("Failed to create GuestMemory");

    // Test writing and reading a slice
    let write_data = b"Hello, macOS GuestMemory!";
    let write_addr = GuestAddress(0x1100);

    let bytes_written = guest_mem.write_at_addr(write_data, write_addr)
        .expect("Failed to write to guest memory");
    assert_eq!(bytes_written, write_data.len());

    let mut read_buf = vec![0u8; write_data.len()];
    let bytes_read = guest_mem.read_at_addr(&mut read_buf, write_addr)
        .expect("Failed to read from guest memory");
    assert_eq!(bytes_read, write_data.len());
    assert_eq!(&read_buf, write_data);
}

/// Test writing and reading typed objects to/from guest memory.
#[test]
fn test_guest_memory_read_write_obj() {
    let start_addr = GuestAddress(0x0);
    let size: u64 = 0x10000;

    let guest_mem = GuestMemory::new(&[(start_addr, size)]).expect("Failed to create GuestMemory");

    // Write a u64 value
    let test_value: u64 = 0xDEADBEEFCAFEBABE;
    let addr = GuestAddress(0x100);
    guest_mem.write_obj_at_addr(test_value, addr).expect("Failed to write u64");

    // Read it back
    let read_value: u64 = guest_mem.read_obj_from_addr(addr).expect("Failed to read u64");
    assert_eq!(read_value, test_value);

    // Write a u32 value at a different location
    let test_u32: u32 = 0x12345678;
    let addr2 = GuestAddress(0x200);
    guest_mem.write_obj_at_addr(test_u32, addr2).expect("Failed to write u32");

    let read_u32: u32 = guest_mem.read_obj_from_addr(addr2).expect("Failed to read u32");
    assert_eq!(read_u32, test_u32);
}

/// Test reading and writing across different memory regions.
#[test]
fn test_guest_memory_read_write_multiple_regions() {
    let regions = &[
        (GuestAddress(0x0), 0x10000),
        (GuestAddress(0x20000), 0x10000),
    ];

    let guest_mem = GuestMemory::new(regions).expect("Failed to create GuestMemory");

    // Write to first region
    let val1: u64 = 0x1111111111111111;
    guest_mem.write_obj_at_addr(val1, GuestAddress(0x100)).expect("Failed to write to region 1");

    // Write to second region
    let val2: u64 = 0x2222222222222222;
    guest_mem.write_obj_at_addr(val2, GuestAddress(0x20100)).expect("Failed to write to region 2");

    // Read back from both regions
    let read1: u64 = guest_mem.read_obj_from_addr(GuestAddress(0x100)).expect("Failed to read from region 1");
    let read2: u64 = guest_mem.read_obj_from_addr(GuestAddress(0x20100)).expect("Failed to read from region 2");

    assert_eq!(read1, val1);
    assert_eq!(read2, val2);
}

/// Test that reading/writing to invalid addresses fails.
#[test]
fn test_guest_memory_invalid_address() {
    let guest_mem = GuestMemory::new(&[(GuestAddress(0x1000), 0x10000)])
        .expect("Failed to create GuestMemory");

    // Try to write to an address outside the region
    let invalid_addr = GuestAddress(0x100000);
    let result = guest_mem.write_obj_at_addr(42u64, invalid_addr);
    assert!(result.is_err(), "Writing to invalid address should fail");

    // Try to read from an address outside the region
    let result: Result<u64, _> = guest_mem.read_obj_from_addr(invalid_addr);
    assert!(result.is_err(), "Reading from invalid address should fail");
}

/// Test write_all_at_addr and read_exact_at_addr.
#[test]
fn test_guest_memory_write_all_read_exact() {
    let guest_mem = GuestMemory::new(&[(GuestAddress(0x0), 0x10000)])
        .expect("Failed to create GuestMemory");

    let test_data = b"Test data for write_all and read_exact";
    let addr = GuestAddress(0x500);

    guest_mem.write_all_at_addr(test_data, addr)
        .expect("write_all_at_addr failed");

    let mut read_buf = vec![0u8; test_data.len()];
    guest_mem.read_exact_at_addr(&mut read_buf, addr)
        .expect("read_exact_at_addr failed");

    assert_eq!(&read_buf, test_data);
}

/// Test getting a volatile slice from guest memory.
#[test]
fn test_guest_memory_volatile_slice() {
    let guest_mem = GuestMemory::new(&[(GuestAddress(0x0), 0x10000)])
        .expect("Failed to create GuestMemory");

    let addr = GuestAddress(0x100);
    let slice_len = 64;

    let vslice = guest_mem.get_slice_at_addr(addr, slice_len)
        .expect("Failed to get volatile slice");

    // Write pattern to the slice
    vslice.write_bytes(0xAB);

    // Read back via guest_memory to verify
    let mut read_buf = vec![0u8; slice_len];
    guest_mem.read_exact_at_addr(&mut read_buf, addr)
        .expect("Failed to read back data");

    assert!(read_buf.iter().all(|&b| b == 0xAB), "Volatile slice write failed");
}

/// Test creating a basic MemoryMapping.
#[test]
fn test_memory_mapping_create() {
    let size = 0x10000; // 64KB

    let mapping = MemoryMappingBuilder::new(size)
        .build()
        .expect("Failed to create MemoryMapping");

    assert_eq!(mapping.size(), size);
}

/// Test creating a MemoryMapping from SharedMemory.
#[test]
fn test_memory_mapping_from_shared_memory() {
    let size: u64 = 0x10000;

    let shm = SharedMemory::new("test_shm", size)
        .expect("Failed to create SharedMemory");

    let mapping = MemoryMappingBuilder::new(size as usize)
        .from_shared_memory(&shm)
        .build()
        .expect("Failed to create MemoryMapping from SharedMemory");

    assert_eq!(mapping.size(), size as usize);
}

/// Test reading and writing to a MemoryMapping.
#[test]
fn test_memory_mapping_read_write() {
    let size = 0x10000;

    let shm = SharedMemory::new("test_rw_shm", size as u64)
        .expect("Failed to create SharedMemory");

    let mapping = MemoryMappingBuilder::new(size)
        .from_shared_memory(&shm)
        .build()
        .expect("Failed to create MemoryMapping");

    // Write a u64 value
    let test_value: u64 = 0xCAFEBABEDEADBEEF;
    let offset = 0x100;
    mapping.write_obj(test_value, offset)
        .expect("Failed to write to MemoryMapping");

    // Read it back
    let read_value: u64 = mapping.read_obj(offset)
        .expect("Failed to read from MemoryMapping");

    assert_eq!(read_value, test_value);
}

/// Test writing and reading slices to/from a MemoryMapping.
#[test]
fn test_memory_mapping_read_write_slice() {
    let size = 0x10000;

    let shm = SharedMemory::new("test_slice_shm", size as u64)
        .expect("Failed to create SharedMemory");

    let mapping = MemoryMappingBuilder::new(size)
        .from_shared_memory(&shm)
        .build()
        .expect("Failed to create MemoryMapping");

    let write_data = b"Hello from MemoryMapping slice test!";
    let offset = 0x200;

    let bytes_written = mapping.write_slice(write_data, offset)
        .expect("Failed to write slice to MemoryMapping");
    assert_eq!(bytes_written, write_data.len());

    let mut read_buf = vec![0u8; write_data.len()];
    let bytes_read = mapping.read_slice(&mut read_buf, offset)
        .expect("Failed to read slice from MemoryMapping");
    assert_eq!(bytes_read, write_data.len());
    assert_eq!(&read_buf, write_data);
}

/// Test MemoryMapping with offset into SharedMemory.
#[test]
fn test_memory_mapping_with_offset() {
    let shm_size: u64 = 0x20000; // 128KB shared memory
    let mapping_size = 0x10000; // 64KB mapping
    let offset: u64 = 0x10000;  // Start at 64KB offset

    let shm = SharedMemory::new("test_offset_shm", shm_size)
        .expect("Failed to create SharedMemory");

    let mapping = MemoryMappingBuilder::new(mapping_size)
        .from_shared_memory(&shm)
        .offset(offset)
        .build()
        .expect("Failed to create MemoryMapping with offset");

    assert_eq!(mapping.size(), mapping_size);

    // Write to the mapping
    let test_value: u32 = 0x12345678;
    mapping.write_obj(test_value, 0)
        .expect("Failed to write to offset mapping");

    let read_value: u32 = mapping.read_obj(0)
        .expect("Failed to read from offset mapping");
    assert_eq!(read_value, test_value);
}

/// Test that MemoryMapping reports correct size.
#[test]
fn test_memory_mapping_size() {
    let sizes = [0x1000, 0x10000, 0x100000]; // 4KB, 64KB, 1MB (page-aligned)

    for &size in &sizes {
        let mapping = MemoryMappingBuilder::new(size)
            .build()
            .expect("Failed to create MemoryMapping");

        assert_eq!(mapping.size(), size, "MemoryMapping size mismatch for size {}", size);
    }
}

/// Test MappedRegion trait implementation.
#[test]
fn test_memory_mapping_mapped_region() {
    let size = 0x10000;

    let mapping = MemoryMappingBuilder::new(size)
        .build()
        .expect("Failed to create MemoryMapping");

    // Test MappedRegion trait methods
    assert!(!mapping.as_ptr().is_null(), "MemoryMapping pointer should not be null");
    assert_eq!(mapping.size(), size);
}

/// Test that guest memory address range checks work correctly.
#[test]
fn test_guest_memory_address_in_range() {
    let guest_mem = GuestMemory::new(&[
        (GuestAddress(0x1000), 0x10000),
        (GuestAddress(0x30000), 0x10000),
    ]).expect("Failed to create GuestMemory");

    // Addresses in first region
    assert!(guest_mem.address_in_range(GuestAddress(0x1000)));
    assert!(guest_mem.address_in_range(GuestAddress(0x5000)));
    assert!(guest_mem.address_in_range(GuestAddress(0x10FFF)));

    // Address in gap between regions
    assert!(!guest_mem.address_in_range(GuestAddress(0x20000)));

    // Addresses in second region
    assert!(guest_mem.address_in_range(GuestAddress(0x30000)));
    assert!(guest_mem.address_in_range(GuestAddress(0x3FFFF)));

    // Address after all regions
    assert!(!guest_mem.address_in_range(GuestAddress(0x50000)));
}

/// Test is_valid_range for contiguous memory checking.
#[test]
fn test_guest_memory_is_valid_range() {
    let guest_mem = GuestMemory::new(&[
        (GuestAddress(0x0), 0x10000),
        (GuestAddress(0x20000), 0x10000),
    ]).expect("Failed to create GuestMemory");

    // Valid range within first region
    assert!(guest_mem.is_valid_range(GuestAddress(0x0), 0x1000));
    assert!(guest_mem.is_valid_range(GuestAddress(0x5000), 0x5000));

    // Valid range within second region
    assert!(guest_mem.is_valid_range(GuestAddress(0x20000), 0x10000));

    // Invalid: range spans across regions (including gap)
    assert!(!guest_mem.is_valid_range(GuestAddress(0x5000), 0x20000));

    // Invalid: extends past end of region
    assert!(!guest_mem.is_valid_range(GuestAddress(0x0), 0x20000));
}

/// Test get_host_address functionality.
#[test]
fn test_guest_memory_get_host_address() {
    let guest_mem = GuestMemory::new(&[(GuestAddress(0x1000), 0x10000)])
        .expect("Failed to create GuestMemory");

    let host_addr = guest_mem.get_host_address(GuestAddress(0x1000))
        .expect("Failed to get host address");

    assert!(!host_addr.is_null(), "Host address should not be null");

    // Getting address for different offset in same region should give different pointer
    let host_addr2 = guest_mem.get_host_address(GuestAddress(0x2000))
        .expect("Failed to get host address at offset");

    assert_ne!(host_addr, host_addr2);
    // The difference should be 0x1000 bytes
    assert_eq!(host_addr2 as usize - host_addr as usize, 0x1000);
}

/// Test get_host_address_range functionality.
#[test]
fn test_guest_memory_get_host_address_range() {
    let guest_mem = GuestMemory::new(&[(GuestAddress(0x0), 0x10000)])
        .expect("Failed to create GuestMemory");

    // Valid range
    let result = guest_mem.get_host_address_range(GuestAddress(0x0), 0x1000);
    assert!(result.is_ok());

    // Range too large (extends past region)
    let result = guest_mem.get_host_address_range(GuestAddress(0x0), 0x20000);
    assert!(result.is_err());

    // Zero size should fail
    let result = guest_mem.get_host_address_range(GuestAddress(0x0), 0);
    assert!(result.is_err());
}

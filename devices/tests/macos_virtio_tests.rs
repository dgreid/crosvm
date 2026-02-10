// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS-specific virtio device tests.
//!
//! These tests verify that virtio devices can be created and configured on macOS.
//! They focus on device instantiation and basic setup, not full I/O operations.
//!
//! Run with: `cargo test -p devices --test macos_virtio_tests`

#![cfg(target_os = "macos")]

use std::io::Write;

use hypervisor::ProtectionType;
use tempfile::tempfile;
use tempfile::NamedTempFile;

use devices::virtio::base_features;
use devices::virtio::block::BlockAsync;
use devices::virtio::block::DiskOption;
use devices::virtio::console::device::ConsoleDevice;
use devices::virtio::console::port::ConsolePort;
use devices::virtio::DeviceType;
use devices::virtio::Rng;
use devices::virtio::VirtioDevice;

// =============================================================================
// Test 3.1: Virtio Block Device Creation
// =============================================================================

#[test]
fn test_virtio_block_create() {
    // Create a temporary disk file
    let temp_file = NamedTempFile::new().expect("Failed to create temp file");
    let disk_size: u64 = 4096; // 4KB disk
    temp_file
        .as_file()
        .set_len(disk_size)
        .expect("Failed to set disk size");

    // Configure the disk options
    let disk_option = DiskOption {
        path: temp_file.path().to_path_buf(),
        read_only: false,
        sparse: true,
        block_size: 512,
        ..Default::default()
    };

    // Open the disk file
    let disk_file = disk_option.open().expect("Failed to open disk file");

    // Create the block device
    let features = base_features(ProtectionType::Unprotected);
    let block_device = BlockAsync::new(
        features,
        disk_file,
        &disk_option,
        None, // No control tube
        None, // Default queue size
        None, // Default number of queues
    )
    .expect("Failed to create BlockAsync device");

    // Verify device type
    assert_eq!(block_device.device_type(), DeviceType::Block);

    // Verify queue configuration
    let queue_sizes = block_device.queue_max_sizes();
    assert!(!queue_sizes.is_empty(), "Block device should have at least one queue");

    // Verify features are non-zero (should have base features at minimum)
    assert_ne!(block_device.features(), 0, "Block device should have features");

    // Verify config space is readable
    let mut config_data = [0u8; 8];
    block_device.read_config(0, &mut config_data);
    // Config should contain capacity (number of sectors)
    // For a 4KB disk with 512-byte sectors, we expect 8 sectors
    let capacity = u64::from_le_bytes(config_data);
    assert_eq!(capacity, 8, "Block device capacity should be 8 sectors");
}

#[test]
fn test_virtio_block_read_only() {
    // Create a temporary disk file
    let temp_file = NamedTempFile::new().expect("Failed to create temp file");
    temp_file
        .as_file()
        .set_len(4096)
        .expect("Failed to set disk size");

    // Configure read-only disk
    let disk_option = DiskOption {
        path: temp_file.path().to_path_buf(),
        read_only: true,
        sparse: false,
        block_size: 512,
        ..Default::default()
    };

    let disk_file = disk_option.open().expect("Failed to open disk file");
    let features = base_features(ProtectionType::Unprotected);

    let block_device = BlockAsync::new(features, disk_file, &disk_option, None, None, None)
        .expect("Failed to create read-only BlockAsync device");

    // Verify read-only feature bit is set (VIRTIO_BLK_F_RO = 5)
    const VIRTIO_BLK_F_RO: u64 = 1 << 5;
    assert!(
        block_device.features() & VIRTIO_BLK_F_RO != 0,
        "Read-only block device should have VIRTIO_BLK_F_RO feature"
    );
}

#[test]
fn test_virtio_block_custom_block_size() {
    let temp_file = NamedTempFile::new().expect("Failed to create temp file");
    let disk_size: u64 = 8192; // 8KB disk
    temp_file
        .as_file()
        .set_len(disk_size)
        .expect("Failed to set disk size");

    // Configure with 4096-byte block size
    let disk_option = DiskOption {
        path: temp_file.path().to_path_buf(),
        block_size: 4096,
        ..Default::default()
    };

    let disk_file = disk_option.open().expect("Failed to open disk file");
    let features = base_features(ProtectionType::Unprotected);

    let block_device = BlockAsync::new(features, disk_file, &disk_option, None, None, None)
        .expect("Failed to create BlockAsync device with 4K block size");

    // Block size feature should be set (VIRTIO_BLK_F_BLK_SIZE = 6)
    const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
    assert!(
        block_device.features() & VIRTIO_BLK_F_BLK_SIZE != 0,
        "Block device should have VIRTIO_BLK_F_BLK_SIZE feature"
    );

    // Read blk_size from config space (offset 20)
    let mut blk_size_data = [0u8; 4];
    block_device.read_config(20, &mut blk_size_data);
    let blk_size = u32::from_le_bytes(blk_size_data);
    assert_eq!(blk_size, 4096, "Block size in config should be 4096");
}

// =============================================================================
// Test 3.2: Virtio Console Device Creation
// =============================================================================

#[test]
fn test_virtio_console_create() {
    // Create input and output streams using temp files
    let input = Box::new(tempfile().expect("Failed to create input tempfile"));
    let output: Box<dyn Write + Send> =
        Box::new(tempfile().expect("Failed to create output tempfile"));

    // Create a console port
    let port = ConsolePort::new(Some(input), Some(output), None, Vec::new());

    // Create the console device (single port, no multiport)
    let console = ConsoleDevice::new_single_port(ProtectionType::Unprotected, port);

    // Verify the number of queues
    // Single port console has 2 queues: receiveq and transmitq
    assert_eq!(
        console.max_queues(),
        2,
        "Single-port console should have 2 queues"
    );

    // Verify features are set
    assert_ne!(console.features(), 0, "Console device should have features");

    // Verify max ports
    assert_eq!(
        console.max_ports(),
        1,
        "Single-port console should have 1 port"
    );
}

#[test]
fn test_virtio_console_multiport() {
    // Create multiple ports
    let mut ports = Vec::new();
    for _ in 0..3 {
        let input = Box::new(tempfile().expect("Failed to create input tempfile"));
        let output: Box<dyn Write + Send> =
            Box::new(tempfile().expect("Failed to create output tempfile"));
        ports.push(ConsolePort::new(Some(input), Some(output), None, Vec::new()));
    }

    // Create multi-port console
    let console = ConsoleDevice::new_multi_port(ProtectionType::Unprotected, ports);

    // Verify features include multiport
    const VIRTIO_CONSOLE_F_MULTIPORT: u64 = 1 << 1;
    assert!(
        console.features() & VIRTIO_CONSOLE_F_MULTIPORT != 0,
        "Multi-port console should have VIRTIO_CONSOLE_F_MULTIPORT feature"
    );

    // Verify max ports
    assert_eq!(
        console.max_ports(),
        3,
        "Multi-port console should have 3 ports"
    );

    // For multiport: Each port has 2 queues + 2 control queues
    // 3 ports * 2 + 2 = 8 queues
    assert_eq!(
        console.max_queues(),
        8,
        "Multi-port console should have 8 queues"
    );
}

#[test]
fn test_virtio_console_config() {
    let input = Box::new(tempfile().expect("Failed to create input tempfile"));
    let output: Box<dyn Write + Send> =
        Box::new(tempfile().expect("Failed to create output tempfile"));

    let port = ConsolePort::new(Some(input), Some(output), None, Vec::new());
    let console = ConsoleDevice::new_single_port(ProtectionType::Unprotected, port);

    // Read config space
    // virtio_console_config layout:
    //   cols: Le16 (offset 0)
    //   rows: Le16 (offset 2)
    //   max_nr_ports: Le32 (offset 4)
    //   emerg_wr: Le32 (offset 8)
    let mut config_data = [0u8; 4];
    console.read_config(4, &mut config_data); // max_nr_ports is at offset 4

    let max_nr_ports = u32::from_le_bytes(config_data);
    assert_eq!(max_nr_ports, 1, "Config should report 1 max port");
}

#[test]
fn test_virtio_console_keep_rds() {
    let input = Box::new(tempfile().expect("Failed to create input tempfile"));
    let output: Box<dyn Write + Send> =
        Box::new(tempfile().expect("Failed to create output tempfile"));

    let port = ConsolePort::new(Some(input), Some(output), None, Vec::new());
    let console = ConsoleDevice::new_single_port(ProtectionType::Unprotected, port);

    // Verify keep_rds returns a vector (may have file descriptors)
    let rds = console.keep_rds();
    // At minimum, we should have the in_avail_evt from the port
    assert!(
        !rds.is_empty(),
        "Console should have at least one raw descriptor to keep"
    );
}

// =============================================================================
// Test 3.3: Virtio RNG Device Creation
// =============================================================================

#[test]
fn test_virtio_rng_create() {
    let features = base_features(ProtectionType::Unprotected);
    let rng_device = Rng::new(features).expect("Failed to create Rng device");

    // Verify device type
    assert_eq!(rng_device.device_type(), DeviceType::Rng);

    // Verify queue configuration
    // RNG device has exactly 1 queue
    let queue_sizes = rng_device.queue_max_sizes();
    assert_eq!(
        queue_sizes.len(),
        1,
        "RNG device should have exactly 1 queue"
    );
    assert_eq!(queue_sizes[0], 256, "RNG queue size should be 256");

    // Verify features
    assert_ne!(rng_device.features(), 0, "RNG device should have features");
}

#[test]
fn test_virtio_rng_keep_rds() {
    let features = base_features(ProtectionType::Unprotected);
    let rng_device = Rng::new(features).expect("Failed to create Rng device");

    // RNG device should have an empty keep_rds list (no external file descriptors)
    let rds = rng_device.keep_rds();
    assert!(
        rds.is_empty(),
        "RNG device should have no raw descriptors to keep"
    );
}

#[test]
fn test_virtio_rng_features() {
    let base = base_features(ProtectionType::Unprotected);
    let rng_device = Rng::new(base).expect("Failed to create Rng device");

    // RNG device features should match the base features passed in
    // (RNG doesn't add any device-specific features)
    assert_eq!(
        rng_device.features(),
        base,
        "RNG features should match base features"
    );
}

// =============================================================================
// Test 3.4: Vhost-User Net Device Setup
// =============================================================================
// Note: On macOS, the vhost-user net backend is not fully functional because
// macOS doesn't have Linux-style TAP devices. These tests verify the module
// structure and types are available.

#[test]
fn test_virtio_net_vhost_user_create() {
    // On macOS, the vhost-user net backend is stubbed out because TAP devices
    // are not available. This test verifies that the code compiles and the
    // necessary types exist for the vhost-user net framework.
    //
    // The actual NetBackend type requires a TapT trait bound which is not
    // implementable on macOS without a TAP driver, so we verify the module
    // structure is correct without instantiating the backend.
    //
    // In a real macOS VM environment, networking would typically use:
    // - virtio-net with vhost-user connecting to a userspace network stack
    // - A bridge to the host network via vmnet.framework (Apple's VM networking)
    // - Slirp (user-mode networking) if the feature is enabled

    // Verify we can import the vhost_user_backend module
    // The module exists and compiles, even if TAP support is stubbed
    use devices::virtio::vhost_user_backend::VhostUserDevice;

    // Verify the VhostUserDevice trait is accessible
    let _: fn() -> std::any::TypeId = || std::any::TypeId::of::<dyn VhostUserDevice>();

    // The vhost-user connection types should be available for macOS
    use devices::virtio::vhost_user_backend::VhostUserConnectionTrait;
    let _: fn() -> std::any::TypeId = || std::any::TypeId::of::<dyn VhostUserConnectionTrait>();
}

// =============================================================================
// Additional Integration Tests
// =============================================================================

#[test]
fn test_multiple_devices_coexist() {
    // Verify that multiple virtio devices can be created simultaneously
    let features = base_features(ProtectionType::Unprotected);

    // Create RNG device
    let rng_device = Rng::new(features).expect("Failed to create Rng device");
    assert_eq!(rng_device.device_type(), DeviceType::Rng);

    // Create block device
    let temp_file = NamedTempFile::new().expect("Failed to create temp file");
    temp_file
        .as_file()
        .set_len(4096)
        .expect("Failed to set size");
    let disk_option = DiskOption {
        path: temp_file.path().to_path_buf(),
        read_only: true,
        ..Default::default()
    };
    let disk_file = disk_option.open().expect("Failed to open disk");
    let block_device = BlockAsync::new(features, disk_file, &disk_option, None, None, None)
        .expect("Failed to create BlockAsync");
    assert_eq!(block_device.device_type(), DeviceType::Block);

    // Create console device
    let input = Box::new(tempfile().expect("Failed to create tempfile"));
    let output: Box<dyn Write + Send> = Box::new(tempfile().expect("Failed to create tempfile"));
    let port = ConsolePort::new(Some(input), Some(output), None, Vec::new());
    let console = ConsoleDevice::new_single_port(ProtectionType::Unprotected, port);
    assert_eq!(console.max_queues(), 2);

    // All devices should have different device types
    assert_ne!(rng_device.device_type(), block_device.device_type());
}

#[test]
fn test_device_features_version_1() {
    // All virtio devices should have VIRTIO_F_VERSION_1 set
    const VIRTIO_F_VERSION_1: u64 = 1 << 32;
    let features = base_features(ProtectionType::Unprotected);

    // RNG
    let rng = Rng::new(features).expect("Failed to create Rng");
    assert!(
        rng.features() & VIRTIO_F_VERSION_1 != 0,
        "RNG should have VIRTIO_F_VERSION_1"
    );

    // Block
    let temp_file = NamedTempFile::new().expect("Failed to create temp file");
    temp_file
        .as_file()
        .set_len(4096)
        .expect("Failed to set size");
    let disk_option = DiskOption {
        path: temp_file.path().to_path_buf(),
        ..Default::default()
    };
    let disk_file = disk_option.open().expect("Failed to open disk");
    let block = BlockAsync::new(features, disk_file, &disk_option, None, None, None)
        .expect("Failed to create BlockAsync");
    assert!(
        block.features() & VIRTIO_F_VERSION_1 != 0,
        "Block should have VIRTIO_F_VERSION_1"
    );

    // Console
    let input = Box::new(tempfile().expect("Failed to create tempfile"));
    let output: Box<dyn Write + Send> = Box::new(tempfile().expect("Failed to create tempfile"));
    let port = ConsolePort::new(Some(input), Some(output), None, Vec::new());
    let console = ConsoleDevice::new_single_port(ProtectionType::Unprotected, port);
    assert!(
        console.features() & VIRTIO_F_VERSION_1 != 0,
        "Console should have VIRTIO_F_VERSION_1"
    );
}

#[test]
fn test_device_suspend_feature() {
    // All virtio devices created with base_features should have VIRTIO_F_SUSPEND
    // VIRTIO_F_SUSPEND is bit 42 according to virtio_sys/src/virtio_config.rs
    const VIRTIO_F_SUSPEND: u64 = 1 << 42;
    let features = base_features(ProtectionType::Unprotected);

    // Verify base features include suspend
    assert!(
        features & VIRTIO_F_SUSPEND != 0,
        "Base features should include VIRTIO_F_SUSPEND"
    );

    // RNG should have it
    let rng = Rng::new(features).expect("Failed to create Rng");
    assert!(
        rng.features() & VIRTIO_F_SUSPEND != 0,
        "RNG should have VIRTIO_F_SUSPEND"
    );

    // Block should have it
    let temp_file = NamedTempFile::new().expect("Failed to create temp file");
    temp_file.as_file().set_len(4096).expect("Failed to set size");
    let disk_option = DiskOption {
        path: temp_file.path().to_path_buf(),
        ..Default::default()
    };
    let disk_file = disk_option.open().expect("Failed to open disk");
    let block = BlockAsync::new(features, disk_file, &disk_option, None, None, None)
        .expect("Failed to create BlockAsync");
    assert!(
        block.features() & VIRTIO_F_SUSPEND != 0,
        "Block should have VIRTIO_F_SUSPEND"
    );
}

#[test]
fn test_block_queue_configuration() {
    let temp_file = NamedTempFile::new().expect("Failed to create temp file");
    temp_file
        .as_file()
        .set_len(4096)
        .expect("Failed to set size");

    let disk_option = DiskOption {
        path: temp_file.path().to_path_buf(),
        ..Default::default()
    };
    let disk_file = disk_option.open().expect("Failed to open disk");
    let features = base_features(ProtectionType::Unprotected);

    // Test with custom queue size and count
    let custom_queue_size = 128u16;
    let custom_num_queues = 4u16;

    let block = BlockAsync::new(
        features,
        disk_file,
        &disk_option,
        None,
        Some(custom_queue_size),
        Some(custom_num_queues),
    )
    .expect("Failed to create BlockAsync with custom queues");

    let queue_sizes = block.queue_max_sizes();
    assert_eq!(
        queue_sizes.len(),
        custom_num_queues as usize,
        "Should have {} queues",
        custom_num_queues
    );

    for (idx, &size) in queue_sizes.iter().enumerate() {
        assert_eq!(
            size, custom_queue_size,
            "Queue {} should have size {}",
            idx, custom_queue_size
        );
    }

    // Multi-queue feature should be set (VIRTIO_BLK_F_MQ = 12)
    const VIRTIO_BLK_F_MQ: u64 = 1 << 12;
    assert!(
        block.features() & VIRTIO_BLK_F_MQ != 0,
        "Block with multiple queues should have VIRTIO_BLK_F_MQ"
    );
}

#[test]
fn test_block_single_queue_no_mq_feature() {
    let temp_file = NamedTempFile::new().expect("Failed to create temp file");
    temp_file
        .as_file()
        .set_len(4096)
        .expect("Failed to set size");

    let disk_option = DiskOption {
        path: temp_file.path().to_path_buf(),
        ..Default::default()
    };
    let disk_file = disk_option.open().expect("Failed to open disk");
    let features = base_features(ProtectionType::Unprotected);

    // Create with single queue
    let block = BlockAsync::new(
        features,
        disk_file,
        &disk_option,
        None,
        None,
        Some(1), // Single queue
    )
    .expect("Failed to create BlockAsync with single queue");

    // Multi-queue feature should NOT be set for single queue
    const VIRTIO_BLK_F_MQ: u64 = 1 << 12;
    assert!(
        block.features() & VIRTIO_BLK_F_MQ == 0,
        "Block with single queue should NOT have VIRTIO_BLK_F_MQ"
    );
}

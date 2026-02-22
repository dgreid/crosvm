// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Integration test for hotplug of tap devices as virtio-net and block devices.

#![cfg(all(unix, target_arch = "x86_64"))]

use std::net::Ipv4Addr;
use std::process::Command;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use base::sys::linux::ioctl_with_val;
use base::test_utils::call_test_with_sudo;
use fixture::utils::create_vu_block_config;
use fixture::utils::prepare_disk_img;
use fixture::vhost_user::CmdType;
use fixture::vhost_user::VhostUserBackend;
use fixture::vm::Config;
use fixture::vm::TestVm;
use net_util::sys::linux::Tap;
use net_util::sys::linux::TapTLinux;
use net_util::MacAddress;
use net_util::TapTCommon;
use tempfile::NamedTempFile;

/// Count the number of virtio-net devices.
fn count_virtio_net_devices(vm: &mut TestVm) -> usize {
    let lspci_result = vm.exec_in_guest("lspci -n").unwrap();
    // Count occurance for virtio net device: 1af4:1041
    lspci_result.stdout.matches("1af4:1041").count()
}

/// Poll func until it returns true, or timeout is exceeded.
fn poll_until_true<F>(vm: &mut TestVm, func: F, timeout: Duration) -> bool
where
    F: Fn(&mut TestVm) -> bool,
{
    let poll_interval = Duration::from_millis(100);
    let start_time = Instant::now();
    while !func(vm) {
        if start_time.elapsed() > timeout {
            return false;
        }
        thread::sleep(poll_interval);
    }
    true
}

/// setup a tap device for test
fn setup_tap_device(tap_name: &[u8], ip_addr: Ipv4Addr, netmask: Ipv4Addr, mac_addr: MacAddress) {
    let tap = Tap::new_with_name(tap_name, true, false).unwrap();
    // SAFETY:
    // ioctl is safe since we call it with a valid tap fd and check the return value.
    let ret = unsafe { ioctl_with_val(&tap, net_sys::TUNSETPERSIST, 1) };
    if ret < 0 {
        panic!("Failed to persist tap interface");
    }
    tap.set_ip_addr(ip_addr).unwrap();
    tap.set_netmask(netmask).unwrap();
    tap.set_mac_address(mac_addr).unwrap();
    tap.set_vnet_hdr_size(16).unwrap();
    tap.set_offload(0).unwrap();
    tap.enable().unwrap();
    // Release tap to be used by the VM.
    drop(tap);
}

/// Implementation for tap_hotplug_two
///
/// This test will fail by itself due to permission.
#[ignore = "Only to be called by tap_hotplug_two"]
#[test]
fn tap_hotplug_two_impl() {
    let wait_timeout = Duration::from_secs(5);
    // Setup VM start parameter.
    let config = Config::new().extra_args(vec!["--pci-hotplug-slots".to_owned(), "2".to_owned()]);
    let mut vm = TestVm::new(config).unwrap();

    //Setup test taps. tap_name has to be distinct per test, or it may appear flaky (b/333090169).
    let tap1_name = "test_tap1";
    setup_tap_device(
        tap1_name.as_bytes(),
        "100.115.92.15".parse().unwrap(),
        "255.255.255.252".parse().unwrap(),
        "a0:b0:c0:d0:e0:f1".parse().unwrap(),
    );
    let tap2_name = "test_tap2";
    setup_tap_device(
        tap2_name.as_bytes(),
        "100.115.92.25".parse().unwrap(),
        "255.255.255.252".parse().unwrap(),
        "a0:b0:c0:d0:e0:f2".parse().unwrap(),
    );

    // Check number of virtio-net devices after each hotplug.
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 0 },
        wait_timeout
    ));
    vm.hotplug_tap(tap1_name).unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 1 },
        wait_timeout
    ));
    vm.hotplug_tap(tap2_name).unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 2 },
        wait_timeout
    ));

    // Check number of devices after each removal.
    vm.remove_pci_device(1).unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 1 },
        wait_timeout
    ));
    vm.remove_pci_device(2).unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 0 },
        wait_timeout
    ));

    drop(vm);
    Command::new("ip")
        .args(["link", "delete", tap1_name])
        .status()
        .unwrap();
    Command::new("ip")
        .args(["link", "delete", tap2_name])
        .status()
        .unwrap();
}

/// Checks hotplug works with two tap devices.
#[test]
fn tap_hotplug_two() {
    call_test_with_sudo("tap_hotplug_two_impl");
}

/// Implementation for tap_hotplug_add_remove_add
///
/// This test will fail by itself due to permission.
#[ignore = "Only to be called by tap_hotplug_add_remove_add"]
#[test]
fn tap_hotplug_add_remove_add_impl() {
    let wait_timeout = Duration::from_secs(5);
    // Setup VM start parameter.
    let config = Config::new().extra_args(vec!["--pci-hotplug-slots".to_owned(), "1".to_owned()]);
    let mut vm = TestVm::new(config).unwrap();

    //Setup test tap. tap_name has to be distinct per test, or it may appear flaky (b/333090169).
    let tap_name = "test_tap3";
    setup_tap_device(
        tap_name.as_bytes(),
        "100.115.92.5".parse().unwrap(),
        "255.255.255.252".parse().unwrap(),
        "a0:b0:c0:d0:e0:f0".parse().unwrap(),
    );

    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 0 },
        wait_timeout
    ));
    // Hotplug tap.
    vm.hotplug_tap(tap_name).unwrap();
    // Wait until virtio-net device appears in guest OS.
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 1 },
        wait_timeout
    ));

    // Remove hotplugged tap device.
    vm.remove_pci_device(1).unwrap();
    // Wait until virtio-net device disappears from guest OS.
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 0 },
        wait_timeout
    ));

    // Hotplug tap again.
    vm.hotplug_tap(tap_name).unwrap();
    // Wait until virtio-net device appears in guest OS.
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 1 },
        wait_timeout
    ));

    drop(vm);
    Command::new("ip")
        .args(["link", "delete", tap_name])
        .status()
        .unwrap();
}

/// Checks tap hotplug works with a device added, removed, then added again.
#[test]
fn tap_hotplug_add_remove_add() {
    call_test_with_sudo("tap_hotplug_add_remove_add_impl");
}

/// Implementation for tap_hotplug_add_remove_rapid_add
///
/// This test will fail by itself due to permission.
#[ignore = "Only to be called by tap_hotplug_add_remove_rapid_add"]
#[test]
fn tap_hotplug_add_remove_rapid_add_impl() {
    let wait_timeout = Duration::from_secs(5);
    // Setup VM start parameter.
    let config = Config::new().extra_args(vec!["--pci-hotplug-slots".to_owned(), "1".to_owned()]);
    let mut vm = TestVm::new(config).unwrap();

    //Setup test tap. tap_name has to be distinct per test, or it may appear flaky (b/333090169).
    let tap_name_a = "test_tap4";
    setup_tap_device(
        tap_name_a.as_bytes(),
        "100.115.92.9".parse().unwrap(),
        "255.255.255.252".parse().unwrap(),
        "a0:b0:c0:d0:e0:f0".parse().unwrap(),
    );

    let tap_name_b = "test_tap5";
    setup_tap_device(
        tap_name_b.as_bytes(),
        "100.115.92.1".parse().unwrap(),
        "255.255.255.252".parse().unwrap(),
        "a0:b0:c0:d0:e0:f0".parse().unwrap(),
    );

    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 0 },
        wait_timeout
    ));
    // Hotplug tap.
    vm.hotplug_tap(tap_name_a).unwrap();
    // Wait until virtio-net device appears in guest OS.
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 1 },
        wait_timeout
    ));

    // Remove hotplugged tap device, then hotplug again without waiting for guest.
    vm.remove_pci_device(1).unwrap();
    vm.hotplug_tap(tap_name_b).unwrap();

    // Wait for a while that the guest likely noticed the removal.
    thread::sleep(Duration::from_millis(500));
    // Wait until virtio-net device reappears in guest OS. This assertion would fail if the device
    // added later is not recognized.
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 1 },
        wait_timeout
    ));

    drop(vm);
    Command::new("ip")
        .args(["link", "delete", tap_name_a])
        .status()
        .unwrap();
    Command::new("ip")
        .args(["link", "delete", tap_name_b])
        .status()
        .unwrap();
}

/// Checks tap hotplug works with a device added, removed, then rapidly added again.
#[test]
fn tap_hotplug_add_remove_rapid_add() {
    call_test_with_sudo("tap_hotplug_add_remove_rapid_add_impl");
}

/// Implementation for tap_hotplug_exceed_slots
///
/// This test will fail by itself due to permission.
#[ignore = "Only to be called by tap_hotplug_exceed_slots"]
#[test]
fn tap_hotplug_exceed_slots_impl() {
    // Setup VM with only 1 hotplug slot.
    let config = Config::new().extra_args(vec!["--pci-hotplug-slots".to_owned(), "1".to_owned()]);
    let mut vm = TestVm::new(config).unwrap();
    let wait_timeout = Duration::from_secs(5);

    // Setup two test tap devices.
    let tap1_name = "test_tap_e1";
    setup_tap_device(
        tap1_name.as_bytes(),
        "100.115.92.41".parse().unwrap(),
        "255.255.255.252".parse().unwrap(),
        "a0:b0:c0:d0:e1:f1".parse().unwrap(),
    );
    let tap2_name = "test_tap_e2";
    setup_tap_device(
        tap2_name.as_bytes(),
        "100.115.92.45".parse().unwrap(),
        "255.255.255.252".parse().unwrap(),
        "a0:b0:c0:d0:e1:f2".parse().unwrap(),
    );

    // First hotplug should succeed.
    vm.hotplug_tap(tap1_name).unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_net_devices(vm) == 1 },
        wait_timeout
    ));

    // Second hotplug should fail: no available slots.
    let result = vm.hotplug_tap(tap2_name);
    assert!(
        result.is_err(),
        "Expected error when exceeding hotplug slots"
    );

    drop(vm);
    Command::new("ip")
        .args(["link", "delete", tap1_name])
        .status()
        .unwrap();
    Command::new("ip")
        .args(["link", "delete", tap2_name])
        .status()
        .unwrap();
}

/// Checks that hotplugging more devices than available slots returns an error.
#[test]
fn tap_hotplug_exceed_slots() {
    call_test_with_sudo("tap_hotplug_exceed_slots_impl");
}

/// Implementation for tap_hotplug_remove_empty_slot
///
/// This test will fail by itself due to permission.
#[ignore = "Only to be called by tap_hotplug_remove_empty_slot"]
#[test]
fn tap_hotplug_remove_empty_slot_impl() {
    // Setup VM with 1 hotplug slot but don't plug any device.
    let config = Config::new().extra_args(vec!["--pci-hotplug-slots".to_owned(), "1".to_owned()]);
    let mut vm = TestVm::new(config).unwrap();

    // Removing from an empty slot should fail.
    let result = vm.remove_pci_device(1);
    assert!(
        result.is_err(),
        "Expected error when removing from empty slot"
    );

    drop(vm);
}

/// Checks that removing from an already-empty slot returns an error.
#[test]
fn tap_hotplug_remove_empty_slot() {
    call_test_with_sudo("tap_hotplug_remove_empty_slot_impl");
}

/// Implementation for tap_hotplug_remove_nonexistent_bus
///
/// This test will fail by itself due to permission.
#[ignore = "Only to be called by tap_hotplug_remove_nonexistent_bus"]
#[test]
fn tap_hotplug_remove_nonexistent_bus_impl() {
    // Setup VM with 1 hotplug slot.
    let config = Config::new().extra_args(vec!["--pci-hotplug-slots".to_owned(), "1".to_owned()]);
    let mut vm = TestVm::new(config).unwrap();

    // Removing from a non-existent bus number should fail.
    let result = vm.remove_pci_device(99);
    assert!(
        result.is_err(),
        "Expected error when removing from non-existent bus"
    );

    drop(vm);
}

/// Checks that removing from a non-existent bus returns an error.
#[test]
fn tap_hotplug_remove_nonexistent_bus() {
    call_test_with_sudo("tap_hotplug_remove_nonexistent_bus_impl");
}

/// Create a temporary raw disk image of the given size in bytes.
fn create_temp_disk(size_bytes: u64) -> NamedTempFile {
    let disk_file = NamedTempFile::new().unwrap();
    disk_file.as_file().set_len(size_bytes).unwrap();
    disk_file
}

/// Count the number of virtio-block devices via lspci.
fn count_virtio_block_devices(vm: &mut TestVm) -> usize {
    let lspci_result = vm.exec_in_guest("lspci -n").unwrap();
    // Count occurrences for virtio block device: 1af4:1042
    lspci_result.stdout.matches("1af4:1042").count()
}

/// Checks hotplug of a single block device: add, verify in guest, remove, verify gone.
#[test]
fn block_hotplug_add_remove() {
    let wait_timeout = Duration::from_secs(5);
    let config = Config::new().extra_args(vec!["--pci-hotplug-slots".to_owned(), "1".to_owned()]);
    let mut vm = TestVm::new(config).unwrap();

    let disk = create_temp_disk(1024 * 1024); // 1 MiB

    // Record baseline count (includes root disk).
    let baseline = count_virtio_block_devices(&mut vm);

    // Hotplug block device.
    vm.hotplug_block(disk.path().to_str().unwrap(), false)
        .unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_block_devices(vm) == baseline + 1 },
        wait_timeout
    ));

    // Remove hotplugged block device.
    vm.remove_block_device(1).unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_block_devices(vm) == baseline },
        wait_timeout
    ));

    drop(vm);
}

/// Checks hotplug of two block devices.
#[test]
fn block_hotplug_two() {
    let wait_timeout = Duration::from_secs(5);
    let config = Config::new().extra_args(vec!["--pci-hotplug-slots".to_owned(), "2".to_owned()]);
    let mut vm = TestVm::new(config).unwrap();

    let disk1 = create_temp_disk(1024 * 1024);
    let disk2 = create_temp_disk(1024 * 1024);

    // Record baseline count (includes root disk).
    let baseline = count_virtio_block_devices(&mut vm);

    // Hotplug first disk.
    vm.hotplug_block(disk1.path().to_str().unwrap(), false)
        .unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_block_devices(vm) == baseline + 1 },
        wait_timeout
    ));

    // Hotplug second disk.
    vm.hotplug_block(disk2.path().to_str().unwrap(), false)
        .unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_block_devices(vm) == baseline + 2 },
        wait_timeout
    ));

    // Remove both.
    vm.remove_block_device(1).unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_block_devices(vm) == baseline + 1 },
        wait_timeout
    ));
    vm.remove_block_device(2).unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_block_devices(vm) == baseline },
        wait_timeout
    ));

    drop(vm);
}

/// Checks block hotplug works with add, remove, then add again on the same slot.
#[test]
fn block_hotplug_add_remove_add() {
    let wait_timeout = Duration::from_secs(5);
    let config = Config::new().extra_args(vec!["--pci-hotplug-slots".to_owned(), "1".to_owned()]);
    let mut vm = TestVm::new(config).unwrap();

    let disk1 = create_temp_disk(1024 * 1024);
    let disk2 = create_temp_disk(2 * 1024 * 1024);

    // Record baseline count (includes root disk).
    let baseline = count_virtio_block_devices(&mut vm);

    // Hotplug first disk.
    vm.hotplug_block(disk1.path().to_str().unwrap(), false)
        .unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_block_devices(vm) == baseline + 1 },
        wait_timeout
    ));

    // Remove it.
    vm.remove_block_device(1).unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_block_devices(vm) == baseline },
        wait_timeout
    ));

    // Hotplug a different disk on the same slot.
    vm.hotplug_block(disk2.path().to_str().unwrap(), true)
        .unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_block_devices(vm) == baseline + 1 },
        wait_timeout
    ));

    drop(vm);
}

/// Checks vhost-user-block hotplug: add and remove.
#[test]
fn vhost_user_block_hotplug_add_remove() {
    let wait_timeout = Duration::from_secs(5);
    let config = Config::new().extra_args(vec!["--pci-hotplug-slots".to_owned(), "1".to_owned()]);
    let mut vm = TestVm::new(config).unwrap();

    // Create a temporary disk and start a vhost-user-block backend.
    let socket = NamedTempFile::new().unwrap();
    let disk = prepare_disk_img();
    let vu_config = create_vu_block_config(CmdType::Device, socket.path(), disk.path());
    let _vu_device = VhostUserBackend::new(vu_config).unwrap();

    // Record baseline count of virtio-block devices (includes root disk).
    let baseline = count_virtio_block_devices(&mut vm);

    // Hotplug vhost-user-block device.
    vm.hotplug_vhost_user_block(socket.path().to_str().unwrap())
        .unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_block_devices(vm) == baseline + 1 },
        wait_timeout
    ));

    // Remove hotplugged device.
    vm.remove_vhost_user_block(1).unwrap();
    assert!(poll_until_true(
        &mut vm,
        |vm| { count_virtio_block_devices(vm) == baseline },
        wait_timeout
    ));

    drop(vm);
}

/// Checks that hotplugging a vhost-user-block device with an invalid socket path returns an
/// error without crashing crosvm.
#[test]
fn vhost_user_block_hotplug_invalid_socket() {
    let config = Config::new().extra_args(vec!["--pci-hotplug-slots".to_owned(), "1".to_owned()]);
    let mut vm = TestVm::new(config).unwrap();

    // Attempt to hotplug with a non-existent socket path. This should fail gracefully.
    let result = vm.hotplug_vhost_user_block("/nonexistent/invalid.sock");
    assert!(
        result.is_err(),
        "Expected error when hotplugging with invalid socket path"
    );

    // Verify the VM is still alive and responsive.
    let guest_result = vm.exec_in_guest("echo ok").unwrap();
    assert!(guest_result.stdout.contains("ok"));

    drop(vm);
}

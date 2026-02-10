// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS serial console device tests.
//!
//! Tests for verifying serial device functionality on macOS.

#![cfg(target_os = "macos")]

use std::io;
use std::sync::Arc;

use base::Event;
use devices::BusAccessInfo;
use devices::BusDevice;
use devices::Serial;
use devices::SerialDevice;
use hypervisor::ProtectionType;
use sync::Mutex;

// Serial port register offsets
const DATA: u8 = 0;
const IER: u8 = 1;

// Interrupt enable bits
const IER_RECV_BIT: u8 = 0x1;

/// Shared buffer for capturing serial output.
#[derive(Clone)]
struct SharedBuffer {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl SharedBuffer {
    fn new() -> SharedBuffer {
        SharedBuffer {
            buf: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.lock().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.buf.lock().flush()
    }
}

/// Helper to create a BusAccessInfo with the given offset.
/// Serial devices only use the offset field of BusAccessInfo.
fn serial_bus_address(offset: u8) -> BusAccessInfo {
    BusAccessInfo {
        offset: offset as u64,
        address: 0,
        id: 0,
    }
}

/// Test 4.1: Serial Device Creation
/// Create serial device with stdout output, verify IRQ configuration.
#[test]
fn test_serial_create() {
    // Create an interrupt event for the serial device
    let intr_evt = Event::new().expect("Failed to create interrupt event");

    // Create a serial device with a shared buffer for output capture
    let serial_out = SharedBuffer::new();

    let serial = Serial::new(
        ProtectionType::Unprotected,
        intr_evt.try_clone().expect("Failed to clone interrupt event"),
        None, // No input
        Some(Box::new(serial_out)),
        None, // No sync
        Default::default(),
        Vec::new(),
    );

    // Verify the serial device was created successfully
    // The debug_label should return "serial"
    assert_eq!(serial.debug_label(), "serial");

    // Verify we can access the interrupt event
    // The interrupt event should be valid and clonable
    let _evt_clone = serial
        .interrupt_event()
        .try_clone()
        .expect("Interrupt event should be clonable");
}

/// Test 4.2: Serial Write
/// Write bytes to serial device, capture and verify output.
#[test]
fn test_serial_write() {
    let intr_evt = Event::new().expect("Failed to create interrupt event");
    let serial_out = SharedBuffer::new();

    let mut serial = Serial::new(
        ProtectionType::Unprotected,
        intr_evt,
        None,
        Some(Box::new(serial_out.clone())),
        None,
        Default::default(),
        Vec::new(),
    );

    // Write individual bytes to the serial device using DATA register
    serial.write(serial_bus_address(DATA), b"H");
    serial.write(serial_bus_address(DATA), b"e");
    serial.write(serial_bus_address(DATA), b"l");
    serial.write(serial_bus_address(DATA), b"l");
    serial.write(serial_bus_address(DATA), b"o");

    // Verify the output buffer contains the written bytes
    let buf = serial_out.buf.lock();
    assert_eq!(buf.as_slice(), b"Hello", "Serial output should match written bytes");
}

/// Test 4.2 extended: Serial write with multiple bytes and special characters.
#[test]
fn test_serial_write_extended() {
    let intr_evt = Event::new().expect("Failed to create interrupt event");
    let serial_out = SharedBuffer::new();

    let mut serial = Serial::new(
        ProtectionType::Unprotected,
        intr_evt,
        None,
        Some(Box::new(serial_out.clone())),
        None,
        Default::default(),
        Vec::new(),
    );

    // Write a string with newline
    let test_string = b"Test output\n";
    for byte in test_string {
        serial.write(serial_bus_address(DATA), &[*byte]);
    }

    let buf = serial_out.buf.lock();
    assert_eq!(
        buf.as_slice(),
        test_string,
        "Serial output should include newline character"
    );
}

/// Test 4.3: Serial Read
/// Provide input to serial device, verify it's readable.
#[test]
fn test_serial_read() {
    let intr_evt = Event::new().expect("Failed to create interrupt event");
    let serial_out = SharedBuffer::new();

    let mut serial = Serial::new(
        ProtectionType::Unprotected,
        intr_evt.try_clone().expect("Failed to clone interrupt event"),
        None,
        Some(Box::new(serial_out)),
        None,
        Default::default(),
        Vec::new(),
    );

    // Enable receive interrupt so we can detect when data is available
    serial.write(serial_bus_address(IER), &[IER_RECV_BIT]);

    // Queue input bytes directly (simulating input from host)
    serial
        .queue_input_bytes(b"ABC")
        .expect("Failed to queue input bytes");

    // The interrupt event should be signaled when data is queued
    assert_eq!(
        intr_evt.wait(),
        Ok(()),
        "Interrupt should be signaled when input is queued"
    );

    // Read the bytes back from the DATA register
    let mut data = [0u8; 1];

    serial.read(serial_bus_address(DATA), &mut data[..]);
    assert_eq!(data[0], b'A', "First byte should be 'A'");

    serial.read(serial_bus_address(DATA), &mut data[..]);
    assert_eq!(data[0], b'B', "Second byte should be 'B'");

    serial.read(serial_bus_address(DATA), &mut data[..]);
    assert_eq!(data[0], b'C', "Third byte should be 'C'");
}

/// Test 4.3 extended: Serial read with larger data.
#[test]
fn test_serial_read_extended() {
    let intr_evt = Event::new().expect("Failed to create interrupt event");
    let serial_out = SharedBuffer::new();

    let mut serial = Serial::new(
        ProtectionType::Unprotected,
        intr_evt.try_clone().expect("Failed to clone interrupt event"),
        None,
        Some(Box::new(serial_out)),
        None,
        Default::default(),
        Vec::new(),
    );

    // Enable receive interrupt
    serial.write(serial_bus_address(IER), &[IER_RECV_BIT]);

    // Queue a longer sequence of bytes
    let input_data = b"Hello, macOS Serial!";
    serial
        .queue_input_bytes(input_data)
        .expect("Failed to queue input bytes");

    // Wait for interrupt
    assert_eq!(intr_evt.wait(), Ok(()), "Interrupt should be signaled");

    // Read all bytes and verify
    let mut received = Vec::new();
    for _ in 0..input_data.len() {
        let mut data = [0u8; 1];
        serial.read(serial_bus_address(DATA), &mut data[..]);
        received.push(data[0]);
    }

    assert_eq!(
        received.as_slice(),
        input_data,
        "All input bytes should be readable in order"
    );
}

/// Test that serial device can handle empty write gracefully.
#[test]
fn test_serial_empty_operations() {
    let intr_evt = Event::new().expect("Failed to create interrupt event");
    let serial_out = SharedBuffer::new();

    let mut serial = Serial::new(
        ProtectionType::Unprotected,
        intr_evt,
        None,
        Some(Box::new(serial_out.clone())),
        None,
        Default::default(),
        Vec::new(),
    );

    // Writing empty data should be ignored (per BusDevice implementation)
    serial.write(serial_bus_address(DATA), &[]);

    // Buffer should remain empty
    let buf = serial_out.buf.lock();
    assert!(buf.is_empty(), "Buffer should be empty after empty write");
}

/// Test serial device without output (sink mode).
#[test]
fn test_serial_sink_mode() {
    let intr_evt = Event::new().expect("Failed to create interrupt event");

    // Create serial device without output (sink mode)
    let mut serial = Serial::new(
        ProtectionType::Unprotected,
        intr_evt,
        None,
        None, // No output - sink mode
        None,
        Default::default(),
        Vec::new(),
    );

    // Writing should not panic even without output
    serial.write(serial_bus_address(DATA), b"a");
    serial.write(serial_bus_address(DATA), b"b");
    serial.write(serial_bus_address(DATA), b"c");

    // Device should still be functional
    assert_eq!(serial.debug_label(), "serial");
}

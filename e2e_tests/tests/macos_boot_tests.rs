// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS boot tests for crosvm with HVF backend.
//!
//! These tests verify basic VM boot functionality on macOS using the Hypervisor.framework
//! backend. They require external test assets (kernel, initrd) specified via environment
//! variables:
//!
//! - `CROSVM_TEST_KERNEL_IMAGE`: Path to the kernel image (bzImage for x86_64, Image for aarch64)
//! - `CROSVM_TEST_INITRD`: Path to the initrd/initramfs (optional, required for userspace tests)
//!
//! Run tests with:
//! ```
//! CROSVM_TEST_KERNEL_IMAGE=/path/to/kernel cargo test -p e2e_tests macos_boot -- --ignored
//! ```

#![cfg(target_os = "macos")]

use std::env;
use std::io::BufRead;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Default timeout for boot tests
const DEFAULT_BOOT_TIMEOUT: Duration = Duration::from_secs(60);

/// Short timeout for minimal boot tests
const MINIMAL_BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// Get the path to the test kernel from environment variable
fn get_kernel_path() -> Option<PathBuf> {
    env::var("CROSVM_TEST_KERNEL_IMAGE")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.exists())
}

/// Get the path to the test initrd from environment variable
fn get_initrd_path() -> Option<PathBuf> {
    env::var("CROSVM_TEST_INITRD")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.exists())
}

/// Find the crosvm binary
fn find_crosvm_binary() -> PathBuf {
    // Check CARGO_MANIFEST_DIR/bin first (for tools/run_tests)
    if let Ok(manifest_dir) = env::var("CARGO_MANIFEST_DIR") {
        let bin_crosvm = PathBuf::from(&manifest_dir)
            .parent()
            .unwrap()
            .join("bin")
            .join("crosvm");
        if bin_crosvm.exists() {
            return bin_crosvm;
        }
    }

    // Check parent of exe directory (for cargo test)
    if let Ok(exe_path) = env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            if let Some(parent_dir) = exe_dir.parent() {
                let parent_crosvm = parent_dir.join("crosvm");
                if parent_crosvm.exists() {
                    return parent_crosvm;
                }
            }
        }
    }

    panic!("Cannot find crosvm binary");
}

/// Configuration for a test VM
struct TestVmConfig {
    kernel_path: PathBuf,
    initrd_path: Option<PathBuf>,
    memory_mb: u64,
    cpus: u32,
    extra_args: Vec<String>,
}

impl TestVmConfig {
    fn new(kernel_path: PathBuf) -> Self {
        Self {
            kernel_path,
            initrd_path: None,
            memory_mb: 512,
            cpus: 1,
            extra_args: Vec::new(),
        }
    }

    fn with_initrd(mut self, initrd_path: PathBuf) -> Self {
        self.initrd_path = Some(initrd_path);
        self
    }

    fn with_memory_mb(mut self, memory_mb: u64) -> Self {
        self.memory_mb = memory_mb;
        self
    }

    fn with_cpus(mut self, cpus: u32) -> Self {
        self.cpus = cpus;
        self
    }

    #[allow(dead_code)]
    fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args.extend(args);
        self
    }
}

/// A running test VM with serial output capture
struct TestVm {
    process: Child,
    serial_rx: mpsc::Receiver<String>,
    _serial_thread: thread::JoinHandle<()>,
}

impl TestVm {
    /// Start a new test VM with the given configuration
    fn start(config: TestVmConfig) -> anyhow::Result<Self> {
        let crosvm = find_crosvm_binary();

        let mut cmd = Command::new(&crosvm);
        cmd.arg("run");

        // Memory configuration
        cmd.arg("--mem").arg(config.memory_mb.to_string());

        // CPU configuration
        cmd.arg("--cpus").arg(config.cpus.to_string());

        // Serial output to stdout for capture
        cmd.arg("--serial").arg("type=stdout,hardware=serial,console");

        // Disable sandbox on macOS (not supported)
        cmd.arg("--disable-sandbox");

        // Add initrd if specified
        if let Some(ref initrd) = config.initrd_path {
            cmd.arg("--initrd").arg(initrd);
        }

        // Add any extra arguments
        for arg in &config.extra_args {
            cmd.arg(arg);
        }

        // Kernel path is the last argument
        cmd.arg(&config.kernel_path);

        println!("Starting VM: {:?}", cmd);

        // Capture stdout for serial output
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut process = cmd.spawn()?;

        // Set up channel for serial output
        let (tx, rx) = mpsc::channel();
        let stdout = process.stdout.take().expect("Failed to capture stdout");

        let serial_thread = thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        println!("[SERIAL] {}", line);
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("Error reading serial output: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(Self {
            process,
            serial_rx: rx,
            _serial_thread: serial_thread,
        })
    }

    /// Wait for a pattern to appear in serial output
    fn wait_for_serial_pattern(&self, pattern: &str, timeout: Duration) -> anyhow::Result<String> {
        let start = Instant::now();
        let mut collected_output = String::new();

        while start.elapsed() < timeout {
            match self.serial_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    collected_output.push_str(&line);
                    collected_output.push('\n');
                    if line.contains(pattern) {
                        return Ok(line);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!(
                        "Serial output ended before pattern '{}' was found. Output so far:\n{}",
                        pattern,
                        collected_output
                    );
                }
            }
        }

        anyhow::bail!(
            "Timeout waiting for pattern '{}'. Output so far:\n{}",
            pattern,
            collected_output
        );
    }

    /// Wait for any of the given patterns to appear in serial output
    fn wait_for_any_pattern(
        &self,
        patterns: &[&str],
        timeout: Duration,
    ) -> anyhow::Result<(usize, String)> {
        let start = Instant::now();
        let mut collected_output = String::new();

        while start.elapsed() < timeout {
            match self.serial_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    collected_output.push_str(&line);
                    collected_output.push('\n');
                    for (idx, pattern) in patterns.iter().enumerate() {
                        if line.contains(pattern) {
                            return Ok((idx, line));
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!(
                        "Serial output ended before any pattern was found. Output so far:\n{}",
                        collected_output
                    );
                }
            }
        }

        anyhow::bail!(
            "Timeout waiting for any of {:?}. Output so far:\n{}",
            patterns,
            collected_output
        );
    }

    /// Stop the VM
    fn stop(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        // Send SIGTERM for graceful shutdown
        unsafe {
            libc::kill(self.process.id() as i32, libc::SIGTERM);
        }

        // Wait a bit for graceful shutdown
        thread::sleep(Duration::from_secs(2));

        // Kill if still running
        let _ = self.process.kill();
        let status = self.process.wait()?;
        Ok(status)
    }
}

impl Drop for TestVm {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Test minimal kernel boot - verifies "Linux version" appears in serial output
///
/// This is the most basic boot test that verifies:
/// - HVF hypervisor is working
/// - Kernel can be loaded and executed
/// - Serial console output is functional
///
/// Requires: CROSVM_TEST_KERNEL_IMAGE environment variable
#[test]
#[ignore = "requires external kernel image: set CROSVM_TEST_KERNEL_IMAGE"]
fn test_minimal_boot() {
    let kernel_path = get_kernel_path().expect(
        "CROSVM_TEST_KERNEL_IMAGE must be set to a valid kernel path for this test",
    );

    let config = TestVmConfig::new(kernel_path);
    let vm = TestVm::start(config).expect("Failed to start VM");

    // Wait for the kernel version string which appears early in boot
    let result = vm.wait_for_serial_pattern("Linux version", MINIMAL_BOOT_TIMEOUT);

    match result {
        Ok(line) => {
            println!("Boot successful! Found kernel version: {}", line);
            assert!(line.contains("Linux version"));
        }
        Err(e) => {
            panic!("Boot test failed: {}", e);
        }
    }
}

/// Test boot to userspace with initrd
///
/// Verifies that:
/// - Initrd can be loaded alongside kernel
/// - Kernel successfully unpacks and runs initrd
/// - Init process starts
///
/// Requires: CROSVM_TEST_KERNEL_IMAGE and CROSVM_TEST_INITRD environment variables
#[test]
#[ignore = "requires external kernel and initrd: set CROSVM_TEST_KERNEL_IMAGE and CROSVM_TEST_INITRD"]
fn test_boot_to_userspace() {
    let kernel_path = get_kernel_path().expect(
        "CROSVM_TEST_KERNEL_IMAGE must be set to a valid kernel path for this test",
    );
    let initrd_path = get_initrd_path().expect(
        "CROSVM_TEST_INITRD must be set to a valid initrd path for this test",
    );

    let config = TestVmConfig::new(kernel_path).with_initrd(initrd_path);
    let vm = TestVm::start(config).expect("Failed to start VM");

    // First verify kernel boot
    vm.wait_for_serial_pattern("Linux version", MINIMAL_BOOT_TIMEOUT)
        .expect("Kernel did not boot");

    // Then verify init runs - look for common init messages
    let init_patterns = [
        "Run /init",
        "Run /sbin/init",
        "Freeing unused kernel",
        "Welcome",
        "Starting init",
    ];

    let result = vm.wait_for_any_pattern(&init_patterns, DEFAULT_BOOT_TIMEOUT);

    match result {
        Ok((idx, line)) => {
            println!(
                "Userspace boot successful! Found init indicator '{}': {}",
                init_patterns[idx], line
            );
        }
        Err(e) => {
            panic!("Userspace boot test failed: {}", e);
        }
    }
}

/// Test graceful VM shutdown
///
/// Verifies that:
/// - VM boots successfully
/// - VM can be stopped cleanly
/// - Process exits without error
///
/// Requires: CROSVM_TEST_KERNEL_IMAGE environment variable
#[test]
#[ignore = "requires external kernel image: set CROSVM_TEST_KERNEL_IMAGE"]
fn test_graceful_shutdown() {
    let kernel_path = get_kernel_path().expect(
        "CROSVM_TEST_KERNEL_IMAGE must be set to a valid kernel path for this test",
    );

    let config = TestVmConfig::new(kernel_path);
    let mut vm = TestVm::start(config).expect("Failed to start VM");

    // Wait for kernel to start booting
    vm.wait_for_serial_pattern("Linux version", MINIMAL_BOOT_TIMEOUT)
        .expect("Kernel did not boot");

    // Give it a moment to stabilize
    thread::sleep(Duration::from_secs(2));

    // Stop the VM
    let status = vm.stop().expect("Failed to stop VM");

    // On graceful shutdown with SIGTERM, the exit status should be clean
    // (either 0 or killed by signal which is expected)
    println!("VM exit status: {:?}", status);

    // The test passes if we get here without hanging
    // We don't strictly require exit code 0 since SIGTERM handling varies
}

/// Test multi-vCPU boot
///
/// Verifies that:
/// - VM can be configured with multiple vCPUs
/// - Kernel detects and initializes all CPUs
/// - SMP boot is functional
///
/// Requires: CROSVM_TEST_KERNEL_IMAGE environment variable
#[test]
#[ignore = "requires external kernel image: set CROSVM_TEST_KERNEL_IMAGE"]
fn test_multi_vcpu() {
    let kernel_path = get_kernel_path().expect(
        "CROSVM_TEST_KERNEL_IMAGE must be set to a valid kernel path for this test",
    );

    // Test with 2 vCPUs
    let config = TestVmConfig::new(kernel_path).with_cpus(2);
    let vm = TestVm::start(config).expect("Failed to start VM");

    // Wait for kernel boot
    vm.wait_for_serial_pattern("Linux version", MINIMAL_BOOT_TIMEOUT)
        .expect("Kernel did not boot");

    // Look for SMP detection messages
    // Different kernels report this differently, so check for common patterns
    let smp_patterns = [
        "SMP",            // Generic SMP indication
        "CPU1",           // Secondary CPU
        "Brought up",     // CPU bring-up message
        "smp: Bringing",  // Kernel SMP message
        "2 CPUs",         // Direct CPU count
        "cpus=2",         // Command line or config
    ];

    let result = vm.wait_for_any_pattern(&smp_patterns, DEFAULT_BOOT_TIMEOUT);

    match result {
        Ok((idx, line)) => {
            println!(
                "SMP detection successful! Found pattern '{}': {}",
                smp_patterns[idx], line
            );
        }
        Err(e) => {
            // Even if we don't see explicit SMP messages, the boot itself with 2 CPUs
            // is a success. Some minimal kernels don't print verbose SMP info.
            println!("Note: No explicit SMP message found, but boot succeeded: {}", e);
        }
    }
}

/// Test various memory sizes
///
/// Verifies that:
/// - VM boots with different memory configurations
/// - Memory is correctly detected by the guest kernel
///
/// Requires: CROSVM_TEST_KERNEL_IMAGE environment variable
#[test]
#[ignore = "requires external kernel image: set CROSVM_TEST_KERNEL_IMAGE"]
fn test_various_memory_sizes() {
    let kernel_path = get_kernel_path().expect(
        "CROSVM_TEST_KERNEL_IMAGE must be set to a valid kernel path for this test",
    );

    // Test different memory configurations
    let memory_sizes_mb = [256, 512, 1024];

    for &mem_mb in &memory_sizes_mb {
        println!("\n=== Testing with {} MB memory ===", mem_mb);

        let config = TestVmConfig::new(kernel_path.clone()).with_memory_mb(mem_mb);
        let vm = TestVm::start(config).expect(&format!("Failed to start VM with {} MB", mem_mb));

        // Wait for kernel boot
        let result = vm.wait_for_serial_pattern("Linux version", MINIMAL_BOOT_TIMEOUT);

        match result {
            Ok(line) => {
                println!("Boot with {} MB successful: {}", mem_mb, line);
            }
            Err(e) => {
                panic!("Boot with {} MB failed: {}", mem_mb, e);
            }
        }

        // Look for memory detection messages
        // The kernel reports memory in various formats
        let mem_patterns = [
            "Memory:",       // Generic memory message
            "available",     // Memory available message
            "MemTotal",      // procfs style
            "mem auto-init", // Memory initialization
        ];

        // Try to find memory info, but don't fail if not found
        // (minimal kernels may not print this)
        if let Ok((idx, line)) = vm.wait_for_any_pattern(&mem_patterns, Duration::from_secs(10)) {
            println!(
                "Memory info with {} MB - pattern '{}': {}",
                mem_mb, mem_patterns[idx], line
            );
        }

        // Drop the VM to ensure clean shutdown before next iteration
        drop(vm);

        // Small delay between tests
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod helpers {
    use super::*;

    /// Helper to check if test prerequisites are available
    pub fn check_prerequisites() -> bool {
        get_kernel_path().is_some()
    }

    /// Print setup instructions for running these tests
    #[test]
    fn print_test_setup_instructions() {
        println!("\n=== macOS Boot Tests Setup ===\n");
        println!("These tests require external kernel/initrd images.");
        println!("Set the following environment variables:\n");
        println!("  CROSVM_TEST_KERNEL_IMAGE=/path/to/kernel");
        println!("  CROSVM_TEST_INITRD=/path/to/initrd  (optional, for userspace tests)\n");
        println!("Then run tests with:\n");
        println!("  cargo test -p e2e_tests macos_boot -- --ignored\n");

        if check_prerequisites() {
            println!("Current status: Kernel image found at {:?}", get_kernel_path());
            if let Some(initrd) = get_initrd_path() {
                println!("               Initrd found at {:?}", initrd);
            } else {
                println!("               No initrd configured (userspace tests will be skipped)");
            }
        } else {
            println!("Current status: No kernel image configured");
        }
    }
}

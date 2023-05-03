// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::env;
use std::io::BufRead;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Once;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base::syslog;
use log::Level;
use prebuilts::download_file;

use crate::sys::SerialArgs;
use crate::sys::TestVmSys;
use crate::utils::run_with_timeout;

const PREBUILT_URL: &str = "https://storage.googleapis.com/crosvm/integration_tests";

#[cfg(target_arch = "x86_64")]
const ARCH: &str = "x86_64";
#[cfg(target_arch = "arm")]
const ARCH: &str = "arm";
#[cfg(target_arch = "aarch64")]
const ARCH: &str = "aarch64";
#[cfg(target_arch = "riscv64")]
const ARCH: &str = "riscv64";

/// Timeout when waiting for pipes that are expected to be ready.
const COMMUNICATION_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for the VM to boot and the delegate to report that it's ready.
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default timeout when waiting for guest commands to execute
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

fn prebuilt_version() -> &'static str {
    include_str!("../../guest_under_test/PREBUILT_VERSION").trim()
}

fn kernel_prebuilt_url() -> String {
    format!(
        "{}/guest-bzimage-{}-{}",
        PREBUILT_URL,
        ARCH,
        prebuilt_version()
    )
}

fn rootfs_prebuilt_url() -> String {
    format!(
        "{}/guest-rootfs-{}-{}",
        PREBUILT_URL,
        ARCH,
        prebuilt_version()
    )
}

/// The kernel bzImage is stored next to the test executable, unless overridden by
/// CROSVM_CARGO_TEST_KERNEL_BINARY
pub(super) fn kernel_path() -> PathBuf {
    match env::var("CROSVM_CARGO_TEST_KERNEL_BINARY") {
        Ok(value) => PathBuf::from(value),
        Err(_) => env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("bzImage-{}", prebuilt_version())),
    }
}

/// The rootfs image is stored next to the test executable, unless overridden by
/// CROSVM_CARGO_TEST_ROOTFS_IMAGE
pub(super) fn rootfs_path() -> PathBuf {
    match env::var("CROSVM_CARGO_TEST_ROOTFS_IMAGE") {
        Ok(value) => PathBuf::from(value),
        Err(_) => env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("rootfs-{}", prebuilt_version())),
    }
}

/// Represents a command running in the guest. See `TestVm::exec_in_guest_async()`
#[must_use]
pub struct GuestProcess {
    command: String,
    timeout: Duration,
}

impl GuestProcess {
    pub fn with_timeout(self, duration: Duration) -> Self {
        Self {
            timeout: duration,
            ..self
        }
    }

    /// Waits for the process to finish execution and return the produced stdout.
    /// Will fail on a non-zero exit code.
    pub fn wait(self, vm: &mut TestVm) -> Result<String> {
        let command = self.command.clone();
        let (exit_code, output) = self.wait_unchecked(vm)?;
        if exit_code != 0 {
            bail!(
                "Command `{}` terminated with exit code {}",
                command,
                exit_code
            );
        }
        Ok(output)
    }

    /// Same as `wait` but will return a tuple of (exit code, output) instead of failing
    /// on a non-zero exit code.
    pub fn wait_unchecked(self, vm: &mut TestVm) -> Result<(i32, String)> {
        // First read echo of command
        let echo = vm
            .read_line_from_guest(COMMUNICATION_TIMEOUT)
            .with_context(|| {
                format!(
                    "Command `{}`: Failed to read echo from guest pipe",
                    self.command
                )
            })?;
        assert_eq!(echo.trim(), self.command.trim());

        // Then read stdout and exit code
        let mut output = Vec::<String>::new();
        let exit_code = loop {
            let line = vm.read_line_from_guest(self.timeout).with_context(|| {
                format!(
                    "Command `{}`: Failed to read response from guest",
                    self.command
                )
            })?;
            let trimmed = line.trim();
            if trimmed.starts_with(TestVm::EXIT_CODE_LINE) {
                let exit_code_str = &trimmed[(TestVm::EXIT_CODE_LINE.len() + 1)..];
                break exit_code_str.parse::<i32>().unwrap();
            }
            output.push(trimmed.to_owned());
        };

        // Finally get the VM in a ready state again.
        vm.wait_for_guest(COMMUNICATION_TIMEOUT)
            .with_context(|| format!("Command `{}`: Failed to wait for guest", self.command))?;

        Ok((exit_code, output.join("\n")))
    }
}

/// Configuration to start `TestVm`.
pub struct Config {
    /// Extra arguments for the `run` subcommand.
    pub(super) extra_args: Vec<String>,

    /// Use `O_DIRECT` for the rootfs.
    pub(super) o_direct: bool,

    /// Log level of `TestVm`
    pub(super) log_level: Level,

    /// File to save crosvm log to
    pub(super) log_file: Option<String>,

    /// Wrapper command line for executing `TestVM`
    pub(super) wrapper_cmd: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: Level::Info,
            extra_args: Default::default(),
            o_direct: Default::default(),
            log_file: None,
            wrapper_cmd: None,
        }
    }
}

impl Config {
    /// Creates a new `run` command with `extra_args`.
    pub fn new() -> Self {
        Default::default()
    }

    /// Uses extra arguments for `crosvm run`.
    #[allow(dead_code)]
    pub fn extra_args(mut self, args: Vec<String>) -> Self {
        let mut args = args;
        self.extra_args.append(&mut args);
        self
    }

    /// Uses `O_DIRECT` for the rootfs.
    pub fn o_direct(mut self) -> Self {
        self.o_direct = true;
        self
    }

    /// Uses `disable-sandbox` argument for `crosvm run`.
    pub fn disable_sandbox(mut self) -> Self {
        self.extra_args.push("--disable-sandbox".to_string());
        self
    }

    pub fn from_env() -> Self {
        let mut cfg: Config = Default::default();
        env::var("CROSVM_CARGO_TEST_E2E_WRAPPER_CMD").map_or((), |x| cfg.wrapper_cmd = Some(x));
        env::var("CROSVM_CARGO_TEST_LOG_FILE").map_or((), |x| cfg.log_file = Some(x));
        env::var("CROSVM_CARGO_TEST_LOG_LEVEL_DEBUG").map_or((), |_| cfg.log_level = Level::Debug);
        cfg
    }
}

static PREP_ONCE: Once = Once::new();

/// Test fixture to spin up a VM running a guest that can be communicated with.
///
/// After creation, commands can be sent via exec_in_guest. The VM is stopped
/// when this instance is dropped.
pub struct TestVm {
    // Platform-dependent bits
    sys: TestVmSys,
    // The guest is ready to receive a command.
    ready: bool,
}

impl TestVm {
    /// Line sent by the delegate binary when the guest is ready.
    const READY_LINE: &'static str = "\x05READY";
    /// Line sent by the delegate binary to terminate the stdout and send the exit code.
    const EXIT_CODE_LINE: &'static str = "\x05EXIT_CODE";

    /// Downloads prebuilts if needed.
    fn initialize_once() {
        if let Err(e) = syslog::init() {
            panic!("failed to initiailize syslog: {}", e);
        }

        // It's possible the prebuilts downloaded by crosvm-9999.ebuild differ
        // from the version that crosvm was compiled for.
        if let Ok(value) = env::var("CROSVM_CARGO_TEST_PREBUILT_VERSION") {
            if value != prebuilt_version() {
                panic!(
                    "Environment provided prebuilts are version {}, but crosvm was compiled \
                    for prebuilt version {}. Did you update PREBUILT_VERSION everywhere?",
                    value,
                    prebuilt_version()
                );
            }
        }

        let kernel_path = kernel_path();
        if env::var("CROSVM_CARGO_TEST_KERNEL_BINARY").is_err() {
            if !kernel_path.exists() {
                download_file(&kernel_prebuilt_url(), &kernel_path).unwrap();
            }
        }
        assert!(kernel_path.exists(), "{:?} does not exist", kernel_path);

        let rootfs_path = rootfs_path();
        if env::var("CROSVM_CARGO_TEST_ROOTFS_IMAGE").is_err() {
            if !rootfs_path.exists() {
                download_file(&rootfs_prebuilt_url(), &rootfs_path).unwrap();
            }
        }
        assert!(rootfs_path.exists(), "{:?} does not exist", rootfs_path);

        TestVmSys::check_rootfs_file(&rootfs_path);
    }

    /// Instanciate a new crosvm instance. The first call will trigger the download of prebuilt
    /// files if necessary.
    ///
    /// This generic method takes a `FnOnce` argument which is in charge of completing the `Command`
    /// with all the relevant options needed to boot the VM.
    pub fn new_generic<F>(f: F, cfg: Config) -> Result<TestVm>
    where
        F: FnOnce(&mut Command, &SerialArgs, &Config) -> Result<()>,
    {
        PREP_ONCE.call_once(TestVm::initialize_once);
        let mut vm = TestVm {
            sys: TestVmSys::new_generic(f, cfg).with_context(|| "Could not start crosvm")?,
            ready: false,
        };
        vm.wait_for_guest(BOOT_TIMEOUT)
            .with_context(|| "Guest did not become ready after boot")?;
        Ok(vm)
    }

    pub fn new(cfg: Config) -> Result<TestVm> {
        TestVm::new_generic(TestVmSys::append_config_args, cfg)
    }

    /// Instanciate a new crosvm instance using a configuration file. The first call will trigger
    /// the download of prebuilt files if necessary.
    pub fn new_with_config_file(cfg: Config) -> Result<TestVm> {
        TestVm::new_generic(TestVmSys::append_config_file_arg, cfg)
    }

    /// Executes the provided command in the guest.
    /// Returns the stdout that was produced by the command, or a GuestProcessError::ExitCode if
    /// the program did not exit with 0.
    pub fn exec_in_guest(&mut self, command: &str) -> Result<String> {
        self.exec_in_guest_async(command)?.wait(self)
    }

    /// Same as `exec_in_guest` but will return a tuple of (exit code, output) instead of failing
    /// on a non-zero exit code.
    pub fn exec_in_guest_unchecked(&mut self, command: &str) -> Result<(i32, String)> {
        self.exec_in_guest_async(command)?.wait_unchecked(self)
    }

    /// Executes the provided command in the guest asynchronously.
    /// The command will be run in the guest, but output will not be read until GuestProcess::wait
    /// is called.
    pub fn exec_in_guest_async(&mut self, command: &str) -> Result<GuestProcess> {
        assert!(self.ready);
        self.ready = false;

        // Send command and read echo from the pipe
        self.write_line_to_guest(command, COMMUNICATION_TIMEOUT)
            .with_context(|| format!("Command `{}`: Failed to write to guest pipe", command))?;

        Ok(GuestProcess {
            command: command.to_owned(),
            timeout: DEFAULT_COMMAND_TIMEOUT,
        })
    }

    // Waits for the guest to be ready to receive commands
    fn wait_for_guest(&mut self, timeout: Duration) -> Result<()> {
        assert!(!self.ready);
        let line = self.read_line_from_guest(timeout)?;
        if line.trim() == TestVm::READY_LINE {
            self.ready = true;
            Ok(())
        } else {
            Err(anyhow!(
                "Recevied unexpected data from delegate: {:?}",
                line.trim()
            ))
        }
    }

    /// Reads one line via the `from_guest` pipe from the guest delegate.
    fn read_line_from_guest(&mut self, timeout: Duration) -> Result<String> {
        let reader = self.sys.from_guest_reader.clone();
        run_with_timeout(
            move || {
                let mut data = String::new();
                reader.lock().unwrap().read_line(&mut data)?;
                println!("<- {:?}", data);
                Ok(data)
            },
            timeout,
        )?
    }

    /// Send one line via the `to_guest` pipe to the guest delegate.
    fn write_line_to_guest(&mut self, data: &str, timeout: Duration) -> Result<()> {
        let writer = self.sys.to_guest.clone();
        let data = data.to_owned();
        run_with_timeout(
            move || -> Result<()> {
                println!("-> {:?}", data);
                writeln!(writer.lock().unwrap(), "{}", data)?;
                Ok(())
            },
            timeout,
        )?
    }

    pub fn stop(&mut self) -> Result<()> {
        self.sys.crosvm_command("stop", vec![])
    }

    pub fn suspend(&mut self) -> Result<()> {
        self.sys.crosvm_command("suspend", vec![])
    }

    pub fn resume(&mut self) -> Result<()> {
        self.sys.crosvm_command("resume", vec![])
    }

    pub fn disk(&mut self, args: Vec<String>) -> Result<()> {
        self.sys.crosvm_command("disk", args)
    }

    pub fn snapshot(&mut self, filename: &std::path::Path) -> Result<()> {
        self.sys.crosvm_command(
            "snapshot",
            vec!["take".to_string(), String::from(filename.to_str().unwrap())],
        )
    }

    // No argument is passed in restore as we will always restore snapshot.bkp for testing.
    pub fn restore(&mut self, filename: &std::path::Path) -> Result<()> {
        self.sys.crosvm_command(
            "snapshot",
            vec![
                "restore".to_string(),
                String::from(filename.to_str().unwrap()),
            ],
        )
    }
}

impl Drop for TestVm {
    fn drop(&mut self) {
        self.stop().unwrap();
        let status = self.sys.process.take().unwrap().wait().unwrap();
        if !status.success() {
            panic!("VM exited illegally: {}", status);
        }
    }
}

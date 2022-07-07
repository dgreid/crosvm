// Copyright 2022 The ChromiumOS Authors.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::FromArgs;

use argh_helpers::generate_catchall_args;
#[derive(Debug, FromArgs)]
#[argh(subcommand)]
/// Windows Devices
pub enum DevicesSubcommand {}

#[cfg(feature = "slirp")]
#[generate_catchall_args]
#[argh(subcommand, name = "run-slirp")]
/// Start a new metrics instance
pub struct RunSlirpCommand {}

#[generate_catchall_args]
#[argh(subcommand, name = "run-main")]
/// Start a new broker instance
pub struct RunMainCommand {}

#[generate_catchall_args]
#[argh(subcommand, name = "run-metrics")]
/// Start a new metrics instance
pub struct RunMetricsCommand {}

/// Start a new mp crosvm instance
#[generate_catchall_args]
#[argh(subcommand, name = "run-mp")]
pub struct RunMPCommand {}

#[derive(FromArgs)]
#[argh(subcommand)]
/// Windows Devices
pub enum Commands {
    RunMetrics(RunMetricsCommand),
    RunMP(RunMPCommand),
    #[cfg(feature = "slirp")]
    RunSlirp(RunSlirpCommand),
    RunMain(RunMainCommand),
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::crosvm::cmdline::RunCommand;

    fn get_args() -> Vec<&'static str> {
        vec!["--bios", "C:\\src\\crosvm\\out\\image\\default\\images\\bios.rom",
        "--crash-pipe-name", "\\\\.\\pipe\\crashpad_27812_XGTCCTBYULHHLEJU", "--cpus", "4",
        "--mem", "8192",
        "--log-file", "C:\\tmp\\Emulator.log",
        "--kernel-log-file", "C:\\tmp\\Hypervisor.log",
        "--logs-directory", "C:\\tmp\\emulator_logs",
        "--serial", "hardware=serial,num=1,type=file,path=C:\\tmp\\AndroidSerial.log,earlycon=true",
        "--serial", "hardware=virtio-console,num=1,type=file,path=C:\\tmp\\AndroidSerial.log,console=true",
          "--rwdisk", "C:\\src\\crosvm\\out\\image\\default\\avd\\aggregate.img",
          "--rwdisk", "C:\\src\\crosvm\\out\\image\\default\\avd\\metadata.img",
          "--rwdisk", "C:\\src\\crosvm\\out\\image\\default\\avd\\userdata.img",
          "--rwdisk", "C:\\src\\crosvm\\out\\image\\default\\avd\\misc.img",
          "--process-invariants-handle", "7368", "--process-invariants-size", "568",
           "--gpu", "angle=true,backend=gfxstream,egl=true,gles=false,glx=false,refresh_rate=60,surfaceless=false,vulkan=true,wsi=vk,display_mode=borderless_full_screen,hidden",
          "--host-guid", "09205719-879f-4324-8efc-3e362a4096f4",
          "--ac97", "backend=win_audio",
          "--cid", "3", "--multi-touch", "nil", "--mouse", "nil", "--product-version", "99.9.9.9",
          "--product-channel", "Local", "--product-name", "Play Games",
          "--service-pipe-name", "service-ipc-8244a83a-ae3f-486f-9c50-3fc47b309d27",
           "--pstore", "path=C:\\tmp\\pstore,size=1048576",
          "--pvclock",
          "--params", "fake args"]
    }

    #[test]
    fn parse_run_mp_test() {
        let _ = RunMPCommand::from_args(&[&"run-mp"], &get_args()).unwrap();
    }

    #[test]
    fn parse_run_test() {
        let _ = RunCommand::from_args(&[&"run-main"], &get_args()).unwrap();
    }
}
// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

extern crate rand;

use rand::{thread_rng, Rng};

use std::ffi::OsString;
use std::fs::remove_file;
use std::io::Write;
use std::env::{current_exe, var_os};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

struct RemovePath(PathBuf);
impl Drop for RemovePath {
    fn drop(&mut self) {
        if let Err(e) = remove_file(&self.0) {
            eprintln!("failed to remove path: {:?}", e);
        }
    }
}
fn get_crosvm_path() -> PathBuf {
    let mut crosvm_path = current_exe()
        .ok()
        .map(|mut path| {
            path.pop();
            if path.ends_with("deps") {
                path.pop();
            }
            path
        })
        .expect("failed to get crosvm binary directory");
    crosvm_path.push("crosvm");
    crosvm_path
}

fn build_test(src: &str) -> RemovePath {
    let mut out_bin = PathBuf::from("../target");
    let mut libqcow_utils = get_crosvm_path();
    libqcow_utils.set_file_name("libqcow_utils.so");
    out_bin.push(thread_rng().gen_ascii_chars().take(10).collect::<String>());
    let mut child = Command::new(var_os("CC").unwrap_or(OsString::from("cc")))
        .args(&["-Isrc", "-pthread", "-o"])
        .arg(&out_bin)
        .arg(libqcow_utils)
        .args(&["-xc", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn compiler");
    {
        let stdin = child.stdin.as_mut().expect("failed to open stdin");
        stdin.write_all(src.as_bytes()).expect(
            "failed to write source to stdin",
        );
    }

    let status = child.wait().expect("failed to wait for compiler");
    assert!(status.success(), "failed to build test");

    RemovePath(PathBuf::from(out_bin))
}

fn run_test(bin_path: &Path) {
    let mut child = Command::new(bin_path)
        .spawn()
        .expect("failed to spawn test");
    for _ in 0..12 {
        match child.try_wait().expect("failed to wait for test") {
            Some(status) => {
                assert!(status.success(), "Test returned failure.");
                return;
            }
            None => sleep(Duration::from_millis(100)),
        }
    }
    child.kill().expect("failed to kill test");
    panic!("test subprocess has timed out");
}

pub fn run_c_test(src: &str) {
    let bin_path = build_test(src);
    run_test(&bin_path.0);
}

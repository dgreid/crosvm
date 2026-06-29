// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Display helper process for macOS.
//!
//! This process owns the AppKit main thread (thread 0) and manages NSWindows
//! on behalf of the crosvm GPU device. It communicates with crosvm via a Tube
//! over a unix socket, receiving framebuffer data through shared memory.
//!
//! Invoked as `crosvm display-helper <fd>` where <fd> is the file descriptor
//! of the unix socket to communicate on.

use std::collections::HashMap;

use base::error;
use base::info;
use base::FromRawDescriptor;
use base::MemoryMapping;
use base::MemoryMappingBuilder;
use base::SafeDescriptor;
use base::SharedMemory;
use base::Tube;
use base::UnixSeqpacket;

use crate::gpu_display_macos::protocol::DisplayRequest;
use crate::gpu_display_macos::protocol::DisplayResponse;

struct HelperSurface {
    _id: u32,
    _width: u32,
    _height: u32,
    _shm: SharedMemory,
    _mmap: MemoryMapping,
}

struct DisplayHelper {
    tube: Tube,
    surfaces: HashMap<u32, HelperSurface>,
}

impl DisplayHelper {
    fn new(tube: Tube) -> Self {
        DisplayHelper {
            tube,
            surfaces: HashMap::new(),
        }
    }

    fn handle_request(&mut self, req: DisplayRequest) -> bool {
        match req {
            DisplayRequest::CreateSurface {
                surface_id,
                width,
                height,
                shm,
                shm_size,
            } => {
                let file: std::fs::File = shm.into();
                let raw_fd = std::os::unix::io::IntoRawFd::into_raw_fd(file);
                // SAFETY: the fd is valid, received via SCM_RIGHTS from crosvm.
                let sd = unsafe { SafeDescriptor::from_raw_descriptor(raw_fd) };
                let shm = match SharedMemory::from_safe_descriptor(sd, shm_size) {
                    Ok(s) => s,
                    Err(e) => {
                        error!(
                            "display helper: failed to create SharedMemory for surface {}: {}",
                            surface_id, e
                        );
                        let _ = self.tube.send(&DisplayResponse::Error {
                            message: format!("SharedMemory::from_safe_descriptor failed: {}", e),
                        });
                        return true;
                    }
                };

                match MemoryMappingBuilder::new(shm_size as usize)
                    .from_shared_memory(&shm)
                    .build()
                {
                    Ok(mmap) => {
                        info!(
                            "display helper: created surface {} ({}x{})",
                            surface_id, width, height
                        );
                        self.surfaces.insert(
                            surface_id,
                            HelperSurface {
                                _id: surface_id,
                                _width: width,
                                _height: height,
                                _shm: shm,
                                _mmap: mmap,
                            },
                        );
                        let _ = self.tube.send(&DisplayResponse::SurfaceCreated {
                            surface_id,
                        });
                    }
                    Err(e) => {
                        error!("display helper: failed to mmap surface {}: {}", surface_id, e);
                        let _ = self.tube.send(&DisplayResponse::Error {
                            message: format!("mmap failed: {}", e),
                        });
                    }
                }
            }
            DisplayRequest::DestroySurface { surface_id } => {
                info!("display helper: destroying surface {}", surface_id);
                self.surfaces.remove(&surface_id);
            }
            DisplayRequest::Flip { surface_id } => {
                if self.surfaces.contains_key(&surface_id) {
                    // TODO: blit from shared memory to NSWindow via CALayer.
                    // For now this is a no-op — the surface data is in shared
                    // memory but we haven't created the AppKit window yet.
                }
            }
            DisplayRequest::Shutdown => {
                info!("display helper: shutdown requested");
                return false;
            }
        }
        true
    }

    fn run(&mut self) {
        info!("display helper: starting event loop");
        loop {
            match self.tube.recv::<DisplayRequest>() {
                Ok(req) => {
                    if !self.handle_request(req) {
                        break;
                    }
                }
                Err(e) => {
                    info!("display helper: tube closed ({}), exiting", e);
                    break;
                }
            }
        }
        info!("display helper: exiting");
    }
}

/// Entry point for the display helper process.
/// Called from main.rs when `display-helper` is the first argument.
pub fn run_display_helper(fd_str: &str) -> ! {
    let fd: i32 = fd_str.parse().unwrap_or_else(|e| {
        eprintln!("display-helper: invalid fd '{}': {}", fd_str, e);
        std::process::exit(1);
    });

    // SAFETY: fd was passed by the parent process and is valid.
    let seqpacket = unsafe { UnixSeqpacket::from_raw_descriptor(fd) };
    let tube: Tube = seqpacket.try_into().unwrap_or_else(|e| {
        eprintln!("display-helper: failed to create Tube: {}", e);
        std::process::exit(1);
    });

    let mut helper = DisplayHelper::new(tube);

    // For now, run the Tube event loop on the main thread.
    // When we add AppKit windowing, this will instead:
    // 1. Spawn a background thread for the Tube reader
    // 2. Run [NSApp run] on the main thread
    helper.run();

    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use std::os::unix::io::FromRawFd;

    use base::AsRawDescriptor;
    use base::FileSerdeWrapper;
    use base::FromRawDescriptor;
    use base::MappedRegion;
    use base::SharedMemory;
    use base::Tube;
    use base::UnixSeqpacket;

    use super::*;
    use crate::gpu_display_macos::protocol::*;

    fn make_tube_pair() -> (Tube, Tube) {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd_a = std::os::unix::io::IntoRawFd::into_raw_fd(a);
        let fd_b = std::os::unix::io::IntoRawFd::into_raw_fd(b);
        // SAFETY: fds are valid from UnixStream::pair().
        let tube_a: Tube = unsafe { UnixSeqpacket::from_raw_descriptor(fd_a) }
            .try_into()
            .unwrap();
        let tube_b: Tube = unsafe { UnixSeqpacket::from_raw_descriptor(fd_b) }
            .try_into()
            .unwrap();
        (tube_a, tube_b)
    }

    #[test]
    fn helper_create_and_destroy_surface() {
        let (crosvm_tube, helper_tube) = make_tube_pair();
        let mut helper = DisplayHelper::new(helper_tube);

        let width = 640u32;
        let height = 480u32;
        let fb_size = (width as u64) * (height as u64) * 4;
        let shm = SharedMemory::new("test_surface", fb_size).unwrap();
        let dup_fd = unsafe { libc::dup(shm.as_raw_descriptor()) };
        assert!(dup_fd >= 0);
        let shm_file = unsafe { std::fs::File::from_raw_fd(dup_fd) };

        crosvm_tube
            .send(&DisplayRequest::CreateSurface {
                surface_id: 1,
                width,
                height,
                shm: FileSerdeWrapper(shm_file),
                shm_size: fb_size,
            })
            .unwrap();

        let req: DisplayRequest = helper.tube.recv().unwrap();
        assert!(helper.handle_request(req));
        assert!(helper.surfaces.contains_key(&1));

        let resp: DisplayResponse = crosvm_tube.recv().unwrap();
        match resp {
            DisplayResponse::SurfaceCreated { surface_id } => assert_eq!(surface_id, 1),
            _ => panic!("expected SurfaceCreated"),
        }

        // Destroy
        crosvm_tube
            .send(&DisplayRequest::DestroySurface { surface_id: 1 })
            .unwrap();
        let req: DisplayRequest = helper.tube.recv().unwrap();
        assert!(helper.handle_request(req));
        assert!(!helper.surfaces.contains_key(&1));
    }

    #[test]
    fn helper_shutdown() {
        let (crosvm_tube, helper_tube) = make_tube_pair();
        let mut helper = DisplayHelper::new(helper_tube);

        crosvm_tube.send(&DisplayRequest::Shutdown).unwrap();
        let req: DisplayRequest = helper.tube.recv().unwrap();
        assert!(!helper.handle_request(req));
    }

    #[test]
    fn helper_flip_no_crash() {
        let (crosvm_tube, helper_tube) = make_tube_pair();
        let mut helper = DisplayHelper::new(helper_tube);

        crosvm_tube
            .send(&DisplayRequest::Flip { surface_id: 99 })
            .unwrap();
        let req: DisplayRequest = helper.tube.recv().unwrap();
        assert!(helper.handle_request(req));
    }

    #[test]
    fn helper_tube_eof_exits() {
        let (crosvm_tube, helper_tube) = make_tube_pair();
        let mut helper = DisplayHelper::new(helper_tube);

        drop(crosvm_tube);
        helper.run();
    }

    #[test]
    fn helper_shared_memory_visible() {
        let (crosvm_tube, helper_tube) = make_tube_pair();
        let mut helper = DisplayHelper::new(helper_tube);

        let width = 4u32;
        let height = 4u32;
        let fb_size = (width as u64) * (height as u64) * 4;
        let shm = SharedMemory::new("test_visible", fb_size).unwrap();

        // Write a pattern into the shared memory from the crosvm side.
        let mmap = MemoryMappingBuilder::new(fb_size as usize)
            .from_shared_memory(&shm)
            .build()
            .unwrap();
        // SAFETY: mmap is valid for its entire size.
        let crosvm_slice = unsafe {
            std::slice::from_raw_parts_mut(mmap.as_ptr(), mmap.size())
        };
        for (i, byte) in crosvm_slice.iter_mut().enumerate() {
            *byte = (i & 0xFF) as u8;
        }

        let dup_fd = unsafe { libc::dup(shm.as_raw_descriptor()) };
        assert!(dup_fd >= 0);
        let shm_file = unsafe { std::fs::File::from_raw_fd(dup_fd) };

        crosvm_tube
            .send(&DisplayRequest::CreateSurface {
                surface_id: 1,
                width,
                height,
                shm: FileSerdeWrapper(shm_file),
                shm_size: fb_size,
            })
            .unwrap();

        let req: DisplayRequest = helper.tube.recv().unwrap();
        assert!(helper.handle_request(req));

        // Verify the helper can see the same data through its mmap.
        let helper_surface = helper.surfaces.get(&1).unwrap();
        let helper_slice = unsafe {
            std::slice::from_raw_parts(helper_surface._mmap.as_ptr(), helper_surface._mmap.size())
        };
        for (i, byte) in helper_slice.iter().enumerate() {
            assert_eq!(*byte, (i & 0xFF) as u8, "mismatch at byte {}", i);
        }
    }
}

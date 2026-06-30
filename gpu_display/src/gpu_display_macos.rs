// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS display backend using a separate helper process for AppKit.
//!
//! AppKit requires all UI operations on thread 0. Since crosvm's main thread
//! blocks on VCPU joins, we spawn a helper process (`crosvm display-helper`)
//! that owns the AppKit event loop. Communication uses a Tube (for control
//! messages with SCM_RIGHTS fd passing) and SharedMemory (for framebuffer data).

use std::cell::RefCell;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::os::unix::io::FromRawFd;
use std::os::unix::io::IntoRawFd;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Command;
use std::sync::Arc;

use base::error;
use base::AsRawDescriptor;
use base::FromRawDescriptor;
use base::MappedRegion;
use base::MemoryMapping;
use base::MemoryMappingBuilder;
use base::RawDescriptor;
use base::SharedMemory;
use base::Tube;
use base::UnixSeqpacket;
use base::VolatileSlice;
use linux_input_sys::virtio_input_event;
use vm_control::gpu::DisplayParameters;

use crate::DisplayT;
use crate::EventDeviceKind;
use crate::GpuDisplayError;
use crate::GpuDisplayEvents;
use crate::GpuDisplayFramebuffer;
use crate::GpuDisplayResult;
use crate::GpuDisplaySurface;
use crate::SurfaceType;
use crate::SysDisplayT;

pub(crate) mod protocol {
    use base::FileSerdeWrapper;
    use serde::Deserialize;
    use serde::Serialize;

    #[derive(Serialize, Deserialize, Debug)]
    pub enum DisplayRequest {
        CreateSurface {
            surface_id: u32,
            width: u32,
            height: u32,
            shm: FileSerdeWrapper,
            shm_size: u64,
        },
        DestroySurface {
            surface_id: u32,
        },
        Flip {
            surface_id: u32,
        },
        Shutdown,
    }

    #[derive(Serialize, Deserialize, Debug)]
    pub enum DisplayResponse {
        SurfaceCreated {
            surface_id: u32,
        },
        CloseRequested {
            surface_id: u32,
        },
        InputEvent {
            surface_id: u32,
            type_: u16,
            code: u16,
            value: i32,
        },
        Error {
            message: String,
        },
    }
}

use protocol::DisplayRequest;
use protocol::DisplayResponse;

struct MacosSurface {
    surface_id: u32,
    width: u32,
    _height: u32,
    _shm: SharedMemory,
    mmap: MemoryMapping,
    tube: Arc<Tube>,
    closed_surfaces: Arc<std::sync::Mutex<HashSet<u32>>>,
}

impl GpuDisplaySurface for MacosSurface {
    fn surface_descriptor(&self) -> u64 {
        self.surface_id as u64
    }

    fn framebuffer(&mut self) -> Option<GpuDisplayFramebuffer> {
        let size = self.mmap.size();
        // SAFETY: mmap is valid for its entire size and lives as long as this surface.
        let slice = unsafe {
            VolatileSlice::from_raw_parts(self.mmap.as_ptr(), size)
        };
        let stride = self.width * 4;
        Some(GpuDisplayFramebuffer::new(slice, stride, 4))
    }

    fn close_requested(&self) -> bool {
        self.closed_surfaces.lock().unwrap().contains(&self.surface_id)
    }

    fn flip(&mut self) {
        if let Err(e) = self.tube.send(&DisplayRequest::Flip {
            surface_id: self.surface_id,
        }) {
            error!("display helper flip send failed: {}", e);
        }
    }
}

pub struct DisplayMacos {
    tube: Arc<Tube>,
    child: Child,
    pending_responses: RefCell<VecDeque<DisplayResponse>>,
    current_response: Option<DisplayResponse>,
    closed_surfaces: Arc<std::sync::Mutex<HashSet<u32>>>,
}

// SAFETY: DisplayMacos is used from the GPU worker thread. The Tube handles
// serialized communication with the helper process. Tube::send takes &self
// and uses internal synchronization.
unsafe impl Send for DisplayMacos {}

impl DisplayMacos {
    pub fn new() -> GpuDisplayResult<DisplayMacos> {
        let (sock_crosvm, sock_helper) = std::os::unix::net::UnixStream::pair()
            .map_err(|_| GpuDisplayError::Connect)?;

        let helper_raw_fd = sock_helper.into_raw_fd();

        let exe = std::env::current_exe().map_err(|e| {
            error!("failed to get current exe path: {}", e);
            GpuDisplayError::Connect
        })?;

        // SAFETY: pre_exec runs between fork and exec. We clear CLOEXEC on the
        // helper socket fd so it survives into the child process. fcntl is
        // async-signal-safe.
        let child = unsafe {
            Command::new(&exe)
                .arg("display-helper")
                .arg(helper_raw_fd.to_string())
                .pre_exec(move || {
                    let flags = libc::fcntl(helper_raw_fd, libc::F_GETFD);
                    if flags < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::fcntl(helper_raw_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
        }.map_err(|e| {
            error!("failed to spawn display helper: {}", e);
            // SAFETY: we still own the fd if spawn failed.
            unsafe { libc::close(helper_raw_fd); }
            GpuDisplayError::Connect
        })?;

        // Close the helper's socketpair end in the parent.
        // SAFETY: helper_raw_fd is valid; child got its own copy via fork.
        unsafe { libc::close(helper_raw_fd); }

        let crosvm_raw_fd = sock_crosvm.into_raw_fd();
        // SAFETY: crosvm_raw_fd is a valid fd from UnixStream::pair().
        // On macOS, UnixSeqpacket wraps STREAM sockets.
        let seqpacket = unsafe { UnixSeqpacket::from_raw_descriptor(crosvm_raw_fd) };
        let tube: Tube = seqpacket.try_into().map_err(|e| {
            error!("failed to create Tube: {}", e);
            GpuDisplayError::Connect
        })?;

        Ok(DisplayMacos {
            tube: Arc::new(tube),
            child,
            pending_responses: RefCell::new(VecDeque::new()),
            current_response: None,
            closed_surfaces: Arc::new(std::sync::Mutex::new(HashSet::new())),
        })
    }

    fn send_request(&self, req: DisplayRequest) {
        if let Err(e) = self.tube.send(&req) {
            error!("display helper send failed: {}", e);
        }
    }
}

impl DisplayT for DisplayMacos {
    fn pending_events(&self) -> bool {
        !self.pending_responses.borrow().is_empty()
    }

    fn flush(&self) {
        // Set socket non-blocking to drain all available responses without
        // blocking if the Tube is empty.
        let fd = self.tube.as_raw_descriptor();
        // SAFETY: fd is valid.
        let old_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if old_flags >= 0 {
            // SAFETY: fd is valid, setting O_NONBLOCK.
            unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags | libc::O_NONBLOCK) };
        }

        loop {
            match self.tube.recv::<DisplayResponse>() {
                Ok(resp) => {
                    self.pending_responses.borrow_mut().push_back(resp);
                }
                Err(_) => break,
            }
        }

        // Restore blocking mode.
        if old_flags >= 0 {
            // SAFETY: fd is valid, restoring original flags.
            unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags) };
        }
    }

    fn next_event(&mut self) -> GpuDisplayResult<u64> {
        let resp = self.pending_responses.borrow_mut().pop_front();
        if let Some(resp) = resp {
            let descriptor = match &resp {
                DisplayResponse::CloseRequested { surface_id }
                | DisplayResponse::SurfaceCreated { surface_id }
                | DisplayResponse::InputEvent { surface_id, .. } => *surface_id as u64,
                DisplayResponse::Error { .. } => 0,
            };
            self.current_response = Some(resp);
            Ok(descriptor)
        } else {
            Ok(0)
        }
    }

    fn handle_next_event(
        &mut self,
        _surface: &mut Box<dyn GpuDisplaySurface>,
    ) -> Option<GpuDisplayEvents> {
        let resp = self.current_response.take()?;
        match resp {
            DisplayResponse::CloseRequested { surface_id } => {
                self.closed_surfaces.lock().unwrap().insert(surface_id);
                None
            }
            DisplayResponse::InputEvent {
                surface_id: _,
                type_,
                code,
                value,
            } => {
                let evt = match type_ {
                    1 => virtio_input_event::key(code, value != 0, value == 2),
                    2 => virtio_input_event::relative(code, value),
                    3 => virtio_input_event::absolute(code, value),
                    _ => return None,
                };
                let device_type = if type_ == 1 && code < 0x110 {
                    EventDeviceKind::Keyboard
                } else {
                    EventDeviceKind::Mouse
                };
                Some(GpuDisplayEvents {
                    events: vec![evt],
                    device_type,
                })
            }
            DisplayResponse::SurfaceCreated { .. } => None,
            DisplayResponse::Error { message } => {
                error!("display helper error: {}", message);
                None
            }
        }
    }

    fn create_surface(
        &mut self,
        parent_surface_id: Option<u32>,
        surface_id: u32,
        _scanout_id: Option<u32>,
        display_params: &DisplayParameters,
        surf_type: SurfaceType,
    ) -> GpuDisplayResult<Box<dyn GpuDisplaySurface>> {
        if parent_surface_id.is_some() {
            return Err(GpuDisplayError::Unsupported);
        }

        let (width, height) = display_params.get_virtual_display_size();
        let fb_size = (width as u64) * (height as u64) * 4;

        let shm = SharedMemory::new("gpu_display_surface", fb_size)
            .map_err(|_| GpuDisplayError::Allocate)?;

        let mmap = MemoryMappingBuilder::new(fb_size as usize)
            .from_shared_memory(&shm)
            .build()
            .map_err(|_| GpuDisplayError::Allocate)?;

        if surf_type == SurfaceType::Scanout {
            let shm_fd = shm.as_raw_descriptor();
            // SAFETY: shm_fd is valid.
            let dup_fd = unsafe { libc::dup(shm_fd) };
            if dup_fd < 0 {
                return Err(GpuDisplayError::Allocate);
            }
            // SAFETY: dup_fd is a newly created valid fd.
            let shm_file = unsafe { std::fs::File::from_raw_fd(dup_fd) };

            self.send_request(DisplayRequest::CreateSurface {
                surface_id,
                width,
                height,
                shm: base::FileSerdeWrapper(shm_file),
                shm_size: fb_size,
            });
        }

        Ok(Box::new(MacosSurface {
            surface_id,
            width,
            _height: height,
            _shm: shm,
            mmap,
            tube: Arc::clone(&self.tube),
            closed_surfaces: Arc::clone(&self.closed_surfaces),
        }))
    }

    fn release_surface(&mut self, surface_id: u32) {
        self.closed_surfaces.lock().unwrap().remove(&surface_id);
        self.send_request(DisplayRequest::DestroySurface { surface_id });
    }
}

impl SysDisplayT for DisplayMacos {}

impl AsRawDescriptor for DisplayMacos {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        // Return the Tube's underlying fd so that WaitContext wakes up
        // when the helper sends responses (input events, close requests).
        self.tube.as_raw_descriptor()
    }
}

impl Drop for DisplayMacos {
    fn drop(&mut self) {
        let _ = self.tube.send(&DisplayRequest::Shutdown);
        let _ = self.child.wait();
    }
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

    use super::protocol::*;

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
    fn request_roundtrip_via_tube() {
        let (sender, receiver) = make_tube_pair();

        sender.send(&DisplayRequest::Flip { surface_id: 42 }).unwrap();
        let decoded: DisplayRequest = receiver.recv().unwrap();
        assert!(matches!(decoded, DisplayRequest::Flip { surface_id: 42 }));

        sender.send(&DisplayRequest::Shutdown).unwrap();
        let decoded: DisplayRequest = receiver.recv().unwrap();
        assert!(matches!(decoded, DisplayRequest::Shutdown));
    }

    #[test]
    fn response_roundtrip_via_tube() {
        let (sender, receiver) = make_tube_pair();

        sender
            .send(&DisplayResponse::CloseRequested { surface_id: 5 })
            .unwrap();
        let decoded: DisplayResponse = receiver.recv().unwrap();
        match decoded {
            DisplayResponse::CloseRequested { surface_id } => assert_eq!(surface_id, 5),
            _ => panic!("expected CloseRequested"),
        }
    }

    #[test]
    fn input_event_roundtrip_via_tube() {
        let (sender, receiver) = make_tube_pair();

        sender
            .send(&DisplayResponse::InputEvent {
                surface_id: 1,
                type_: 1,
                code: 30,
                value: 1,
            })
            .unwrap();
        let decoded: DisplayResponse = receiver.recv().unwrap();
        match decoded {
            DisplayResponse::InputEvent {
                surface_id,
                type_,
                code,
                value,
            } => {
                assert_eq!(surface_id, 1);
                assert_eq!(type_, 1); // EV_KEY
                assert_eq!(code, 30); // KEY_A
                assert_eq!(value, 1); // press
            }
            _ => panic!("expected InputEvent"),
        }
    }

    #[test]
    fn create_surface_with_shm_fd_via_tube() {
        let (sender, receiver) = make_tube_pair();

        let shm = SharedMemory::new("test", 4096).unwrap();
        let dup_fd = unsafe { libc::dup(shm.as_raw_descriptor()) };
        assert!(dup_fd >= 0);
        let shm_file = unsafe { std::fs::File::from_raw_fd(dup_fd) };

        sender
            .send(&DisplayRequest::CreateSurface {
                surface_id: 1,
                width: 32,
                height: 32,
                shm: FileSerdeWrapper(shm_file),
                shm_size: 4096,
            })
            .unwrap();

        let decoded: DisplayRequest = receiver.recv().unwrap();
        match decoded {
            DisplayRequest::CreateSurface {
                surface_id, width, height, shm_size, ..
            } => {
                assert_eq!(surface_id, 1);
                assert_eq!(width, 32);
                assert_eq!(height, 32);
                assert_eq!(shm_size, 4096);
            }
            _ => panic!("expected CreateSurface"),
        }
    }

    #[test]
    fn shared_memory_framebuffer_mmap() {
        let width: u32 = 1920;
        let height: u32 = 1080;
        let fb_size = (width as u64) * (height as u64) * 4;
        assert_eq!(fb_size, 8294400);

        let shm = SharedMemory::new("test", fb_size).unwrap();
        let mmap = base::MemoryMappingBuilder::new(fb_size as usize)
            .from_shared_memory(&shm)
            .build()
            .unwrap();
        assert_eq!(mmap.size(), fb_size as usize);
    }

    #[test]
    fn flush_drains_tube_responses() {
        use std::cell::RefCell;
        use std::collections::HashSet;
        use std::collections::VecDeque;
        use std::sync::Arc;

        use super::DisplayMacos;
        use super::DisplayT;

        let (helper_tube, crosvm_tube) = make_tube_pair();

        helper_tube
            .send(&DisplayResponse::InputEvent {
                surface_id: 1,
                type_: 1,
                code: 30,
                value: 1,
            })
            .unwrap();
        helper_tube
            .send(&DisplayResponse::InputEvent {
                surface_id: 1,
                type_: 1,
                code: 30,
                value: 0,
            })
            .unwrap();

        // Construct DisplayMacos directly for testing (no child process).
        let display = DisplayMacos {
            tube: Arc::new(crosvm_tube),
            child: std::process::Command::new("true").spawn().unwrap(),
            pending_responses: RefCell::new(VecDeque::new()),
            current_response: None,
            closed_surfaces: Arc::new(std::sync::Mutex::new(HashSet::new())),
        };

        assert!(!display.pending_events());
        display.flush();
        assert!(display.pending_events());
        assert_eq!(display.pending_responses.borrow().len(), 2);
    }

    #[test]
    fn close_requested_sets_surface_flag() {
        use std::cell::RefCell;
        use std::collections::HashSet;
        use std::collections::VecDeque;
        use std::sync::Arc;

        use super::DisplayMacos;
        use super::DisplayT;

        let (_helper_tube, crosvm_tube) = make_tube_pair();
        let closed_surfaces = Arc::new(std::sync::Mutex::new(HashSet::new()));

        let mut responses = VecDeque::new();
        responses.push_back(DisplayResponse::CloseRequested { surface_id: 42 });

        let mut display = DisplayMacos {
            tube: Arc::new(crosvm_tube),
            child: std::process::Command::new("true").spawn().unwrap(),
            pending_responses: RefCell::new(responses),
            current_response: None,
            closed_surfaces: Arc::clone(&closed_surfaces),
        };

        assert!(!closed_surfaces.lock().unwrap().contains(&42));

        // Create a dummy surface to pass to handle_next_event.
        let shm = SharedMemory::new("test", 4096).unwrap();
        let mmap = base::MemoryMappingBuilder::new(4096)
            .from_shared_memory(&shm)
            .build()
            .unwrap();
        let mut surface: Box<dyn crate::GpuDisplaySurface> = Box::new(super::MacosSurface {
            surface_id: 42,
            width: 32,
            _height: 32,
            _shm: shm,
            mmap,
            tube: Arc::clone(&display.tube),
            closed_surfaces: Arc::clone(&closed_surfaces),
        });

        // next_event pops from pending into current_response.
        let descriptor = display.next_event().unwrap();
        assert_eq!(descriptor, 42);

        let result = display.handle_next_event(&mut surface);
        assert!(result.is_none());
        assert!(closed_surfaces.lock().unwrap().contains(&42));
        assert!(surface.close_requested());
    }
}

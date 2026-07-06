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
        InjectKey {
            surface_id: u32,
            keycode: u16,
            pressed: bool,
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
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::collections::VecDeque;
    use std::os::unix::io::FromRawFd;
    use std::sync::Arc;

    use base::AsRawDescriptor;
    use base::FileSerdeWrapper;
    use base::FromRawDescriptor;
    use base::MappedRegion;
    use base::SharedMemory;
    use base::Tube;
    use base::UnixSeqpacket;

    use super::protocol::DisplayResponse;
    use super::DisplayMacos;
    use super::DisplayT;

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

    fn make_test_display_and_surface(
        responses: VecDeque<DisplayResponse>,
    ) -> (DisplayMacos, Box<dyn crate::GpuDisplaySurface>) {
        let (_helper_tube, crosvm_tube) = make_tube_pair();
        let closed_surfaces = Arc::new(std::sync::Mutex::new(HashSet::new()));

        let display = DisplayMacos {
            tube: Arc::new(crosvm_tube),
            child: std::process::Command::new("true").spawn().unwrap(),
            pending_responses: RefCell::new(responses),
            current_response: None,
            closed_surfaces: Arc::clone(&closed_surfaces),
        };

        let shm = SharedMemory::new("test", 4096).unwrap();
        let mmap = base::MemoryMappingBuilder::new(4096)
            .from_shared_memory(&shm)
            .build()
            .unwrap();
        let surface: Box<dyn crate::GpuDisplaySurface> = Box::new(super::MacosSurface {
            surface_id: 1,
            width: 32,
            _height: 32,
            _shm: shm,
            mmap,
            tube: Arc::clone(&display.tube),
            closed_surfaces,
        });

        (display, surface)
    }

    #[test]
    fn input_key_event_produces_virtio_key_event() {
        let mut responses = VecDeque::new();
        responses.push_back(DisplayResponse::InputEvent {
            surface_id: 1,
            type_: 1, // EV_KEY
            code: 30, // KEY_A
            value: 1, // press
        });

        let (mut display, mut surface) = make_test_display_and_surface(responses);

        let descriptor = display.next_event().unwrap();
        assert_eq!(descriptor, 1);

        let events = display.handle_next_event(&mut surface);
        let events = events.expect("should produce GpuDisplayEvents for key press");
        assert_eq!(events.device_type, crate::EventDeviceKind::Keyboard);
        assert_eq!(events.events.len(), 1);
        let evt = &events.events[0];
        assert_eq!(evt.type_.to_native(), 1); // EV_KEY
        assert_eq!(evt.code.to_native(), 30); // KEY_A
        assert_eq!(evt.value.to_native(), 1); // press
    }

    #[test]
    fn input_mouse_button_classified_as_mouse() {
        let mut responses = VecDeque::new();
        responses.push_back(DisplayResponse::InputEvent {
            surface_id: 1,
            type_: 1,     // EV_KEY
            code: 0x110,  // BTN_LEFT
            value: 1,
        });

        let (mut display, mut surface) = make_test_display_and_surface(responses);
        display.next_event().unwrap();

        let events = display.handle_next_event(&mut surface)
            .expect("should produce events for mouse button");
        assert_eq!(events.device_type, crate::EventDeviceKind::Mouse);
    }

    #[test]
    fn input_relative_motion_produces_rel_event() {
        let mut responses = VecDeque::new();
        responses.push_back(DisplayResponse::InputEvent {
            surface_id: 1,
            type_: 2,   // EV_REL
            code: 8,    // REL_WHEEL
            value: -3,
        });

        let (mut display, mut surface) = make_test_display_and_surface(responses);
        display.next_event().unwrap();

        let events = display.handle_next_event(&mut surface)
            .expect("should produce events for scroll");
        assert_eq!(events.device_type, crate::EventDeviceKind::Mouse);
        assert_eq!(events.events[0].type_.to_native(), 2);
        assert_eq!(events.events[0].code.to_native(), 8);
        assert_eq!(events.events[0].value.to_native() as i32, -3);
    }

    #[test]
    fn input_absolute_motion_produces_abs_event() {
        let mut responses = VecDeque::new();
        responses.push_back(DisplayResponse::InputEvent {
            surface_id: 1,
            type_: 3,    // EV_ABS
            code: 0,     // ABS_X
            value: 500,
        });

        let (mut display, mut surface) = make_test_display_and_surface(responses);
        display.next_event().unwrap();

        let events = display.handle_next_event(&mut surface)
            .expect("should produce events for abs motion");
        assert_eq!(events.device_type, crate::EventDeviceKind::Mouse);
        assert_eq!(events.events[0].type_.to_native(), 3);
        assert_eq!(events.events[0].code.to_native(), 0);
        assert_eq!(events.events[0].value.to_native() as i32, 500);
    }

    extern "C" {
        fn macos_keycode_to_linux(mac_keycode: u16) -> u16;
    }

    #[test]
    fn keycode_table_letters() {
        let expected: &[(u16, u16)] = &[
            (0x00, 30),  // A
            (0x01, 31),  // S
            (0x02, 32),  // D
            (0x03, 33),  // F
            (0x04, 35),  // H
            (0x05, 34),  // G
            (0x06, 44),  // Z
            (0x07, 45),  // X
            (0x08, 46),  // C
            (0x09, 47),  // V
            (0x0B, 48),  // B
            (0x0C, 16),  // Q
            (0x0D, 17),  // W
            (0x0E, 18),  // E
            (0x0F, 19),  // R
            (0x10, 21),  // Y
            (0x11, 20),  // T
            (0x1F, 24),  // O
            (0x20, 22),  // U
            (0x22, 23),  // I
            (0x23, 25),  // P
            (0x25, 38),  // L
            (0x26, 36),  // J
            (0x28, 37),  // K
            (0x2D, 49),  // N
            (0x2E, 50),  // M
        ];
        for &(mac, linux) in expected {
            // SAFETY: macos_keycode_to_linux is a pure lookup function.
            let result = unsafe { macos_keycode_to_linux(mac) };
            assert_eq!(result, linux, "mac keycode {:#x} should map to linux {}", mac, linux);
        }
    }

    #[test]
    fn keycode_table_modifiers_and_special() {
        let expected: &[(u16, u16)] = &[
            (0x24, 28),   // Return -> KEY_ENTER
            (0x30, 15),   // Tab -> KEY_TAB
            (0x31, 57),   // Space -> KEY_SPACE
            (0x33, 14),   // Backspace -> KEY_BACKSPACE
            (0x35, 1),    // Escape -> KEY_ESC
            (0x38, 42),   // Shift -> KEY_LEFTSHIFT
            (0x3C, 54),   // RightShift -> KEY_RIGHTSHIFT
            (0x3A, 56),   // Option -> KEY_LEFTALT
            (0x3D, 100),  // RightOption -> KEY_RIGHTALT
            (0x3B, 29),   // Control -> KEY_LEFTCTRL
            (0x3E, 97),   // RightControl -> KEY_RIGHTCTRL
            (0x37, 125),  // Command -> KEY_LEFTMETA
            (0x36, 126),  // RightCommand -> KEY_RIGHTMETA
            (0x39, 58),   // CapsLock -> KEY_CAPSLOCK
        ];
        for &(mac, linux) in expected {
            let result = unsafe { macos_keycode_to_linux(mac) };
            assert_eq!(result, linux, "mac keycode {:#x} should map to linux {}", mac, linux);
        }
    }

    #[test]
    fn keycode_table_arrows_and_navigation() {
        let expected: &[(u16, u16)] = &[
            (0x7B, 105),  // Left -> KEY_LEFT
            (0x7C, 106),  // Right -> KEY_RIGHT
            (0x7D, 108),  // Down -> KEY_DOWN
            (0x7E, 103),  // Up -> KEY_UP
            (0x73, 102),  // Home -> KEY_HOME
            (0x77, 107),  // End -> KEY_END
            (0x74, 104),  // PageUp -> KEY_PAGEUP
            (0x79, 109),  // PageDown -> KEY_PAGEDOWN
            (0x75, 111),  // ForwardDelete -> KEY_DELETE
        ];
        for &(mac, linux) in expected {
            let result = unsafe { macos_keycode_to_linux(mac) };
            assert_eq!(result, linux, "mac keycode {:#x} should map to linux {}", mac, linux);
        }
    }

    #[test]
    fn keycode_table_function_keys() {
        let expected: &[(u16, u16)] = &[
            (0x7A, 59),   // F1
            (0x78, 60),   // F2
            (0x63, 61),   // F3
            (0x76, 62),   // F4
            (0x60, 63),   // F5
            (0x61, 64),   // F6
            (0x62, 65),   // F7
            (0x64, 66),   // F8
            (0x65, 67),   // F9
            (0x6D, 68),   // F10
            (0x67, 87),   // F11
            (0x6F, 88),   // F12
        ];
        for &(mac, linux) in expected {
            let result = unsafe { macos_keycode_to_linux(mac) };
            assert_eq!(result, linux, "mac keycode {:#x} should map to linux {}", mac, linux);
        }
    }

    #[test]
    fn keycode_unknown_returns_zero() {
        // Keycodes >= 128 and unmapped entries should return 0.
        let result = unsafe { macos_keycode_to_linux(200) };
        assert_eq!(result, 0);
        // 0x0A is not in the table (gap between V and B).
        let result = unsafe { macos_keycode_to_linux(0x0A) };
        assert_eq!(result, 0);
    }

    /// Integration test: spawn the real crosvm display-helper, create a
    /// surface, inject synthetic key events via InjectKey protocol message,
    /// verify the helper sends correct InputEvent back through the tube.
    ///
    /// Tests the full AppKit pipeline: InjectKey → dispatch_to_main →
    /// NSEvent keyDown:/keyUp: → macos_keycode_to_linux → callback → tube.
    ///
    /// Requires: GUI access + signed crosvm binary at target/release/crosvm.
    /// Run with:
    ///   cargo test -p gpu_display --lib -- cgevent_key_injection --ignored
    #[test]
    #[ignore]
    fn cgevent_key_injection() {
        use std::os::unix::io::IntoRawFd;
        use std::os::unix::process::CommandExt;
        use std::process::Command;
        use std::time::Duration;

        use super::protocol::DisplayRequest;

        // Find the crosvm binary (which understands `display-helper`).
        let crosvm_bin = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/release/crosvm");
        if !crosvm_bin.exists() {
            panic!(
                "crosvm binary not found at {:?}. Build with: cargo build --release && codesign ...",
                crosvm_bin
            );
        }

        // Create a socketpair for the tube.
        let (sock_crosvm, sock_helper) =
            std::os::unix::net::UnixStream::pair().expect("socketpair failed");
        let helper_raw_fd = sock_helper.into_raw_fd();

        // Spawn the display helper process.
        // SAFETY: pre_exec clears CLOEXEC on the helper fd.
        let mut child = unsafe {
            Command::new(&crosvm_bin)
                .arg("display-helper")
                .arg(helper_raw_fd.to_string())
                .pre_exec(move || {
                    let flags = libc::fcntl(helper_raw_fd, libc::F_GETFD);
                    if flags >= 0 {
                        libc::fcntl(helper_raw_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                    }
                    Ok(())
                })
                .spawn()
        }
        .expect("failed to spawn crosvm display-helper");

        // Close the helper's fd in our process.
        unsafe { libc::close(helper_raw_fd) };

        let crosvm_raw_fd = sock_crosvm.into_raw_fd();
        let seqpacket =
            unsafe { base::UnixSeqpacket::from_raw_descriptor(crosvm_raw_fd) };
        let tube: base::Tube = seqpacket.try_into().expect("Tube creation failed");
        let tube = Arc::new(tube);

        // Create a surface (which creates the window in the helper).
        let fb_size: u64 = 1280 * 1024 * 4;
        let shm = SharedMemory::new("test_inject", fb_size).unwrap();
        let dup_fd = unsafe { libc::dup(shm.as_raw_descriptor()) };
        assert!(dup_fd >= 0, "dup failed");
        let shm_file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        tube.send(&DisplayRequest::CreateSurface {
            surface_id: 1,
            width: 1280,
            height: 1024,
            shm: base::FileSerdeWrapper(shm_file),
            shm_size: fb_size,
        })
        .expect("failed to send CreateSurface");

        // Wait for the window to be created.
        std::thread::sleep(Duration::from_millis(500));

        // Read the SurfaceCreated response.
        let resp: DisplayResponse = tube.recv().expect("no SurfaceCreated response");
        assert!(
            matches!(resp, DisplayResponse::SurfaceCreated { surface_id: 1 }),
            "expected SurfaceCreated, got {:?}",
            resp
        );

        // Inject key events. macOS keycode 0x00 = 'A' → Linux KEY_A = 30.
        tube.send(&DisplayRequest::InjectKey {
            surface_id: 1,
            keycode: 0x00,
            pressed: true,
        })
        .expect("failed to send InjectKey press");

        tube.send(&DisplayRequest::InjectKey {
            surface_id: 1,
            keycode: 0x00,
            pressed: false,
        })
        .expect("failed to send InjectKey release");

        // Wait for events to propagate.
        std::thread::sleep(Duration::from_millis(500));

        // Set the tube to non-blocking and drain responses.
        let fd = base::AsRawDescriptor::as_raw_descriptor(&*tube);
        let old_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if old_flags >= 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags | libc::O_NONBLOCK) };
        }

        let mut found_press = false;
        let mut found_release = false;
        loop {
            match tube.recv::<DisplayResponse>() {
                Ok(DisplayResponse::InputEvent {
                    type_: 1,
                    code: 30,
                    value,
                    ..
                }) => {
                    if value == 1 {
                        found_press = true;
                    }
                    if value == 0 {
                        found_release = true;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }

        // Restore blocking mode and shut down.
        if old_flags >= 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags) };
        }
        let _ = tube.send(&DisplayRequest::Shutdown);
        let _ = child.wait();

        assert!(found_press, "did not receive KEY_A press event");
        assert!(found_release, "did not receive KEY_A release event");
    }
}

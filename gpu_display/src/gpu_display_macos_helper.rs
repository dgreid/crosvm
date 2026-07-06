// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Display helper process for macOS.
//!
//! This process owns the AppKit main thread (thread 0) and manages NSWindows
//! on behalf of the crosvm GPU device. It communicates with crosvm via a Tube
//! over a unix socket, receiving framebuffer data through shared memory.
//!
//! Architecture:
//! - Main thread: runs `[NSApp run]`, handles all AppKit/UI operations
//! - Background thread: reads from the Tube, dispatches to main thread via
//!   `dispatch_async_f`
//!
//! Invoked as `crosvm display-helper <fd>`.

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::unix::io::IntoRawFd;
use std::sync::Arc;
use std::sync::Mutex;

use base::error;
use base::info;
use base::FromRawDescriptor;
use base::MappedRegion;
use base::MemoryMapping;
use base::MemoryMappingBuilder;
use base::SafeDescriptor;
use base::SharedMemory;
use base::Tube;
use base::UnixSeqpacket;

use crate::gpu_display_macos::protocol::DisplayRequest;
use crate::gpu_display_macos::protocol::DisplayResponse;

// FFI declarations for the ObjC bridge.
extern "C" {
    fn macos_helper_init_app();
    fn macos_helper_run_app();
    fn macos_helper_stop_app();
    fn macos_helper_set_event_callback(
        callback: extern "C" fn(*mut c_void, u32, u16, u16, i32),
        context: *mut c_void,
    );
    fn macos_helper_set_close_callback(
        callback: extern "C" fn(*mut c_void, u32),
        context: *mut c_void,
    );
    fn macos_helper_create_window(
        surface_id: u32,
        width: u32,
        height: u32,
        framebuffer: *mut u8,
    ) -> *mut c_void;
    fn macos_helper_destroy_window(handle: *mut c_void);
    fn macos_helper_flip(handle: *mut c_void);
    fn macos_helper_inject_key(handle: *mut c_void, keycode: u16, key_down: bool);

    static _dispatch_main_q: c_void;

    fn dispatch_async_f(
        queue: *mut c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

fn dispatch_get_main_queue() -> *mut c_void {
    std::ptr::addr_of!(_dispatch_main_q) as *mut c_void
}

struct HelperSurface {
    _id: u32,
    _width: u32,
    _height: u32,
    _shm: SharedMemory,
    mmap: MemoryMapping,
    window_handle: *mut c_void,
}

// SAFETY: window_handle is only accessed on the main thread. The HelperSurface
// is stored in HelperState which is protected by a Mutex.
unsafe impl Send for HelperSurface {}

struct HelperState {
    tube: Arc<Tube>,
    surfaces: HashMap<u32, HelperSurface>,
}

// Global state accessible from dispatch callbacks. Protected by Mutex for
// thread safety between the bg thread (which modifies surfaces map) and the
// main thread (which accesses window handles and sends responses).
static HELPER_STATE: Mutex<Option<HelperState>> = Mutex::new(None);

struct DisplayHelper {
    tube: Arc<Tube>,
}

impl DisplayHelper {
    fn new(tube: Tube) -> Self {
        let tube = Arc::new(tube);
        *HELPER_STATE.lock().unwrap() = Some(HelperState {
            tube: Arc::clone(&tube),
            surfaces: HashMap::new(),
        });
        DisplayHelper { tube }
    }
}

// Operations dispatched from bg thread to main thread.
enum MainThreadOp {
    CreateSurface {
        surface_id: u32,
        width: u32,
        height: u32,
        shm: SharedMemory,
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
    Stop,
}

// SAFETY: SharedMemory is Send.
unsafe impl Send for MainThreadOp {}

extern "C" fn handle_op_on_main(context: *mut c_void) {
    // SAFETY: context was created by Box::into_raw in the bg thread.
    let op = unsafe { Box::from_raw(context as *mut MainThreadOp) };

    let mut guard = HELPER_STATE.lock().unwrap();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return,
    };

    match *op {
        MainThreadOp::CreateSurface {
            surface_id,
            width,
            height,
            shm,
            shm_size,
        } => {
            match MemoryMappingBuilder::new(shm_size as usize)
                .from_shared_memory(&shm)
                .build()
            {
                Ok(mmap) => {
                    let fb_ptr = mmap.as_ptr();
                    // SAFETY: macos_helper_create_window is called on the main
                    // thread and creates an NSWindow backed by the framebuffer.
                    let handle = unsafe {
                        macos_helper_create_window(surface_id, width, height, fb_ptr)
                    };
                    info!(
                        "display helper: created surface {} ({}x{}) handle={:?}",
                        surface_id, width, height, handle
                    );
                    state.surfaces.insert(
                        surface_id,
                        HelperSurface {
                            _id: surface_id,
                            _width: width,
                            _height: height,
                            _shm: shm,
                            mmap,
                            window_handle: handle,
                        },
                    );
                    let _ = state.tube.send(&DisplayResponse::SurfaceCreated {
                        surface_id,
                    });
                }
                Err(e) => {
                    error!("display helper: mmap failed for surface {}: {}", surface_id, e);
                    let _ = state.tube.send(&DisplayResponse::Error {
                        message: format!("mmap failed: {}", e),
                    });
                }
            }
        }
        MainThreadOp::DestroySurface { surface_id } => {
            if let Some(surface) = state.surfaces.remove(&surface_id) {
                info!("display helper: destroying surface {}", surface_id);
                // SAFETY: window_handle was created by macos_helper_create_window.
                unsafe { macos_helper_destroy_window(surface.window_handle) };
            }
        }
        MainThreadOp::Flip { surface_id } => {
            if let Some(surface) = state.surfaces.get(&surface_id) {
                // SAFETY: window_handle is valid, called on main thread.
                unsafe { macos_helper_flip(surface.window_handle) };
            }
        }
        MainThreadOp::InjectKey {
            surface_id,
            keycode,
            pressed,
        } => {
            let handle = state.surfaces.get(&surface_id).map(|s| s.window_handle);
            drop(guard);
            if let Some(handle) = handle {
                // SAFETY: window_handle is valid, called on main thread.
                // Lock is dropped to avoid deadlock: inject_key triggers
                // on_input_event which re-acquires HELPER_STATE.
                unsafe { macos_helper_inject_key(handle, keycode, pressed) };
            }
            return;
        }
        MainThreadOp::Stop => {
            // Destroy all windows first.
            for (_, surface) in state.surfaces.drain() {
                unsafe { macos_helper_destroy_window(surface.window_handle) };
            }
            // SAFETY: called on main thread.
            unsafe { macos_helper_stop_app() };
        }
    }
}

fn dispatch_to_main(op: MainThreadOp) {
    let boxed = Box::new(op);
    let ptr = Box::into_raw(boxed) as *mut c_void;
    // SAFETY: dispatch_get_main_queue and dispatch_async_f are safe to call
    // from any thread.
    unsafe {
        dispatch_async_f(dispatch_get_main_queue(), ptr, handle_op_on_main);
    }
}

// Callback invoked by the ObjC bridge when an input event occurs.
extern "C" fn on_input_event(
    _context: *mut c_void,
    surface_id: u32,
    type_: u16,
    code: u16,
    value: i32,
) {
    let guard = HELPER_STATE.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        let _ = state.tube.send(&DisplayResponse::InputEvent {
            surface_id,
            type_,
            code,
            value,
        });
    }
}

// Callback invoked by the ObjC bridge when window close is requested.
extern "C" fn on_close_requested(_context: *mut c_void, surface_id: u32) {
    let guard = HELPER_STATE.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        let _ = state.tube.send(&DisplayResponse::CloseRequested { surface_id });
    }
}

fn bg_thread_fn(tube: Arc<Tube>) {
    loop {
        match tube.recv::<DisplayRequest>() {
            Ok(req) => {
                let op = match req {
                    DisplayRequest::CreateSurface {
                        surface_id,
                        width,
                        height,
                        shm,
                        shm_size,
                    } => {
                        let file: std::fs::File = shm.into();
                        let raw_fd = IntoRawFd::into_raw_fd(file);
                        // SAFETY: fd is valid, received via SCM_RIGHTS.
                        let sd = unsafe { SafeDescriptor::from_raw_descriptor(raw_fd) };
                        match SharedMemory::from_safe_descriptor(sd, shm_size) {
                            Ok(shm) => MainThreadOp::CreateSurface {
                                surface_id,
                                width,
                                height,
                                shm,
                                shm_size,
                            },
                            Err(e) => {
                                error!("display helper: SharedMemory failed: {}", e);
                                continue;
                            }
                        }
                    }
                    DisplayRequest::DestroySurface { surface_id } => {
                        MainThreadOp::DestroySurface { surface_id }
                    }
                    DisplayRequest::Flip { surface_id } => {
                        MainThreadOp::Flip { surface_id }
                    }
                    DisplayRequest::InjectKey {
                        surface_id,
                        keycode,
                        pressed,
                    } => MainThreadOp::InjectKey {
                        surface_id,
                        keycode,
                        pressed,
                    },
                    DisplayRequest::Shutdown => MainThreadOp::Stop,
                };
                let is_stop = matches!(op, MainThreadOp::Stop);
                dispatch_to_main(op);
                if is_stop {
                    break;
                }
            }
            Err(e) => {
                info!("display helper: tube closed ({}), stopping", e);
                dispatch_to_main(MainThreadOp::Stop);
                break;
            }
        }
    }
}

/// Entry point for the display helper process.
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

    let helper = DisplayHelper::new(tube);
    let tube_for_bg = Arc::clone(&helper.tube);

    // Initialize NSApplication on main thread (thread 0).
    // SAFETY: called on main thread before any other AppKit operations.
    unsafe {
        macos_helper_init_app();
        macos_helper_set_event_callback(on_input_event, std::ptr::null_mut());
        macos_helper_set_close_callback(on_close_requested, std::ptr::null_mut());
    }

    // Spawn background thread for Tube I/O.
    std::thread::spawn(move || {
        bg_thread_fn(tube_for_bg);
    });

    // Run the AppKit event loop on the main thread. This blocks until
    // macos_helper_stop_app() is called (triggered by Shutdown or Tube EOF).
    // SAFETY: called on main thread.
    unsafe { macos_helper_run_app() };

    // Clean up global state.
    *HELPER_STATE.lock().unwrap() = None;

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
    fn dispatch_op_size() {
        // Ensure MainThreadOp is reasonably sized for dispatch_async_f.
        assert!(std::mem::size_of::<MainThreadOp>() < 256);
    }

    #[test]
    fn protocol_request_roundtrip() {
        let (sender, receiver) = make_tube_pair();

        sender.send(&DisplayRequest::Flip { surface_id: 42 }).unwrap();
        let decoded: DisplayRequest = receiver.recv().unwrap();
        assert!(matches!(decoded, DisplayRequest::Flip { surface_id: 42 }));

        sender.send(&DisplayRequest::Shutdown).unwrap();
        let decoded: DisplayRequest = receiver.recv().unwrap();
        assert!(matches!(decoded, DisplayRequest::Shutdown));
    }

    #[test]
    fn protocol_response_roundtrip() {
        let (sender, receiver) = make_tube_pair();

        sender
            .send(&DisplayResponse::CloseRequested { surface_id: 5 })
            .unwrap();
        let decoded: DisplayResponse = receiver.recv().unwrap();
        match decoded {
            DisplayResponse::CloseRequested { surface_id } => assert_eq!(surface_id, 5),
            _ => panic!("expected CloseRequested"),
        }

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
                assert_eq!(type_, 1);
                assert_eq!(code, 30);
                assert_eq!(value, 1);
            }
            _ => panic!("expected InputEvent"),
        }
    }

    #[test]
    fn create_surface_with_shm_fd() {
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
                surface_id,
                width,
                height,
                shm: _,
                shm_size,
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
    fn shared_memory_cross_process_visible() {
        let width = 4u32;
        let height = 4u32;
        let fb_size = (width as u64) * (height as u64) * 4;
        let shm = SharedMemory::new("test_visible", fb_size).unwrap();

        let mmap = MemoryMappingBuilder::new(fb_size as usize)
            .from_shared_memory(&shm)
            .build()
            .unwrap();

        // Write pattern.
        // SAFETY: mmap is valid for its entire size.
        let slice = unsafe {
            std::slice::from_raw_parts_mut(mmap.as_ptr(), mmap.size())
        };
        for (i, byte) in slice.iter_mut().enumerate() {
            *byte = (i & 0xFF) as u8;
        }

        // Create second mapping from the same shm fd.
        let mmap2 = MemoryMappingBuilder::new(fb_size as usize)
            .from_shared_memory(&shm)
            .build()
            .unwrap();

        let slice2 = unsafe {
            std::slice::from_raw_parts(mmap2.as_ptr(), mmap2.size())
        };
        for (i, byte) in slice2.iter().enumerate() {
            assert_eq!(*byte, (i & 0xFF) as u8, "mismatch at byte {}", i);
        }
    }
}

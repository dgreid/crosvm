// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ffi::c_void;

use base::AsRawDescriptor;
use base::Event;
use base::RawDescriptor;
use base::VolatileSlice;
use vm_control::gpu::DisplayParameters;

use crate::DisplayT;
use crate::GpuDisplayError;
use crate::GpuDisplayFramebuffer;
use crate::GpuDisplayResult;
use crate::GpuDisplaySurface;
use crate::SurfaceType;
use crate::SysDisplayT;

extern "C" {
    fn macos_display_create() -> *mut c_void;
    fn macos_display_destroy(display: *mut c_void);
    fn macos_surface_create(width: u32, height: u32) -> *mut c_void;
    fn macos_surface_destroy(surface: *mut c_void);
    fn macos_surface_framebuffer(surface: *mut c_void, out_len: *mut usize) -> *mut u8;
    fn macos_surface_stride(surface: *mut c_void) -> u32;
    fn macos_surface_flip(surface: *mut c_void);
    fn macos_surface_close_requested(surface: *mut c_void) -> bool;
    fn macos_display_poll_events();
}

struct MacosSurface {
    surface: *mut c_void,
    width: u32,
    height: u32,
}

impl GpuDisplaySurface for MacosSurface {
    fn framebuffer(&mut self) -> Option<GpuDisplayFramebuffer> {
        let mut len: usize = 0;
        // SAFETY: surface is a valid pointer created by macos_surface_create.
        let ptr = unsafe { macos_surface_framebuffer(self.surface, &mut len) };
        if ptr.is_null() || len == 0 {
            return None;
        }
        // SAFETY: ptr points to a buffer of len bytes owned by the C surface struct.
        // It is valid for the lifetime of this MacosSurface.
        let slice = unsafe { VolatileSlice::from_raw_parts(ptr as *mut u8, len) };
        let stride = self.width * 4;
        let bytes_per_pixel = 4;
        Some(GpuDisplayFramebuffer::new(slice, stride, bytes_per_pixel))
    }

    fn flip(&mut self) {
        // SAFETY: surface is valid.
        unsafe { macos_surface_flip(self.surface) };
    }

    fn close_requested(&self) -> bool {
        // SAFETY: surface is valid.
        unsafe { macos_surface_close_requested(self.surface) }
    }
}

impl Drop for MacosSurface {
    fn drop(&mut self) {
        if !self.surface.is_null() {
            // SAFETY: surface was created by macos_surface_create.
            unsafe { macos_surface_destroy(self.surface) };
        }
    }
}

pub struct DisplayMacos {
    display: *mut c_void,
    event: Event,
}

// SAFETY: The display pointer is only used via the C bridge functions which
// dispatch UI work to the main thread. The DisplayMacos is used from the
// GPU worker thread.
unsafe impl Send for DisplayMacos {}

impl DisplayMacos {
    pub fn new() -> GpuDisplayResult<DisplayMacos> {
        let event = Event::new().map_err(|_| GpuDisplayError::CreateEvent)?;
        // SAFETY: macos_display_create returns a valid pointer or null.
        let display = unsafe { macos_display_create() };
        if display.is_null() {
            return Err(GpuDisplayError::Allocate);
        }
        Ok(DisplayMacos { display, event })
    }
}

impl DisplayT for DisplayMacos {
    fn pending_events(&self) -> bool {
        false
    }

    fn flush(&self) {
        // Poll AppKit events from whatever thread we're on.
        // The bridge dispatches actual UI work to the main thread.
        unsafe { macos_display_poll_events() };
    }

    fn create_surface(
        &mut self,
        parent_surface_id: Option<u32>,
        _surface_id: u32,
        _scanout_id: Option<u32>,
        display_params: &DisplayParameters,
        _surf_type: SurfaceType,
    ) -> GpuDisplayResult<Box<dyn GpuDisplaySurface>> {
        if parent_surface_id.is_some() {
            return Err(GpuDisplayError::Unsupported);
        }

        let (width, height) = display_params.get_virtual_display_size();
        // SAFETY: macos_surface_create returns a valid pointer or null.
        let surface = unsafe { macos_surface_create(width, height) };
        if surface.is_null() {
            return Err(GpuDisplayError::Allocate);
        }
        Ok(Box::new(MacosSurface {
            surface,
            width,
            height,
        }))
    }
}

impl SysDisplayT for DisplayMacos {}

impl AsRawDescriptor for DisplayMacos {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.event.as_raw_descriptor()
    }
}

impl Drop for DisplayMacos {
    fn drop(&mut self) {
        if !self.display.is_null() {
            // SAFETY: display was created by macos_display_create.
            unsafe { macos_display_destroy(self.display) };
        }
    }
}

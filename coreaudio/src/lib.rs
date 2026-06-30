// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! CoreAudio backend for crosvm's audio_streams interface.
//!
//! Bridges CoreAudio's pull/callback model with audio_streams' push model
//! using a lock-free SPSC ring buffer.

pub mod convert;
pub mod ring_buffer;

#[cfg(target_os = "macos")]
mod coreaudio_sys;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod playback;

use audio_streams::BoxError;
use audio_streams::StreamSource;
use audio_streams::StreamSourceGenerator;

pub struct CoreAudioStreamSourceGenerator;

impl CoreAudioStreamSourceGenerator {
    pub fn new() -> Self {
        CoreAudioStreamSourceGenerator
    }
}

impl StreamSourceGenerator for CoreAudioStreamSourceGenerator {
    fn generate(&self) -> Result<Box<dyn StreamSource>, BoxError> {
        #[cfg(target_os = "macos")]
        {
            Ok(Box::new(playback::CoreAudioStreamSource::new()))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err("CoreAudio is only available on macOS".into())
        }
    }
}

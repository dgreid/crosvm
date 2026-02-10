// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS stubs for virtio-snd.
//!
//! Audio on macOS would require CoreAudio integration, which is not yet implemented.
//! This module provides stub implementations that return errors.

use async_trait::async_trait;
use audio_streams::capture::AsyncCaptureBuffer;
use audio_streams::AsyncPlaybackBufferStream;
use audio_streams::BoxError;
use audio_streams::StreamSource;
use audio_streams::StreamSourceGenerator;
use cros_async::Executor;
use futures::channel::mpsc::UnboundedSender;
use serde::Deserialize;
use serde::Serialize;

use crate::virtio::snd::common_backend::async_funcs::CaptureBufferReader;
use crate::virtio::snd::common_backend::async_funcs::PlaybackBufferWriter;
use crate::virtio::snd::common_backend::stream_info::StreamInfo;
use crate::virtio::snd::common_backend::DirectionalStream;
use crate::virtio::snd::common_backend::Error;
use crate::virtio::snd::common_backend::PcmResponse;
use crate::virtio::snd::common_backend::SndData;
use crate::virtio::snd::parameters::Error as ParametersError;
use crate::virtio::snd::parameters::Parameters;

pub(crate) type SysAudioStreamSourceGenerator = Box<dyn StreamSourceGenerator>;
pub(crate) type SysAudioStreamSource = Box<dyn StreamSource>;
pub(crate) type SysBufferReader = MacosBufferReader;

pub struct SysDirectionOutput {
    pub async_playback_buffer_stream: Box<dyn AsyncPlaybackBufferStream>,
    pub buffer_writer: Box<dyn PlaybackBufferWriter>,
}

pub(crate) struct SysAsyncStreamObjects {
    pub(crate) stream: DirectionalStream,
    pub(crate) pcm_sender: UnboundedSender<PcmResponse>,
}

/// Audio backend types for macOS (none implemented yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum StreamSourceBackend {
    /// Null backend that produces silence and discards output.
    Null,
}

impl From<StreamSourceBackend> for String {
    fn from(backend: StreamSourceBackend) -> Self {
        match backend {
            StreamSourceBackend::Null => "null".to_owned(),
        }
    }
}

impl TryFrom<&str> for StreamSourceBackend {
    type Error = ParametersError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "null" => Ok(StreamSourceBackend::Null),
            _ => Err(ParametersError::InvalidBackend),
        }
    }
}

#[allow(unused_variables)]
pub(crate) fn create_stream_source_generators(
    backend: StreamSourceBackend,
    params: &Parameters,
    snd_data: &SndData,
) -> Vec<Box<dyn StreamSourceGenerator>> {
    // No audio backends implemented for macOS yet
    base::warn!("Sound stream source generation not implemented on macOS, returning no generators");
    Vec::new()
}

pub(crate) fn set_audio_thread_priority() -> Result<(), base::Error> {
    // Thread priority setting not implemented for macOS
    Ok(())
}

impl StreamInfo {
    async fn set_up_async_playback_stream(
        &mut self,
        _frame_size: usize,
        _ex: &Executor,
    ) -> Result<Box<dyn AsyncPlaybackBufferStream>, Error> {
        Err(Error::OperationNotSupported)
    }

    pub(crate) async fn set_up_async_capture_stream(
        &mut self,
        _frame_size: usize,
        _ex: &Executor,
    ) -> Result<SysBufferReader, Error> {
        Err(Error::OperationNotSupported)
    }

    pub(crate) async fn create_directionstream_output(
        &mut self,
        _frame_size: usize,
        _ex: &Executor,
    ) -> Result<DirectionalStream, Error> {
        Err(Error::OperationNotSupported)
    }
}

/// Placeholder buffer reader for macOS.
pub(crate) struct MacosBufferReader {}

#[async_trait(?Send)]
impl CaptureBufferReader for MacosBufferReader {
    async fn get_next_capture_period(
        &mut self,
        _ex: &Executor,
    ) -> Result<AsyncCaptureBuffer, BoxError> {
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "capture not implemented on macOS",
        )))
    }
}

pub(crate) struct MacosBufferWriter {
    guest_period_bytes: usize,
}

#[async_trait(?Send)]
impl PlaybackBufferWriter for MacosBufferWriter {
    fn new(guest_period_bytes: usize) -> Self {
        MacosBufferWriter { guest_period_bytes }
    }

    fn endpoint_period_bytes(&self) -> usize {
        self.guest_period_bytes
    }
}

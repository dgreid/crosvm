// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use audio_streams::capture::AsyncCaptureBuffer;
use audio_streams::capture::AsyncCaptureBufferStream;
use audio_streams::AsyncPlaybackBufferStream;
use audio_streams::BoxError;
use audio_streams::StreamSource;
use audio_streams::StreamSourceGenerator;
use coreaudio::CoreAudioStreamSourceGenerator;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum StreamSourceBackend {
    Null,
    COREAUDIO,
}

impl From<StreamSourceBackend> for String {
    fn from(backend: StreamSourceBackend) -> Self {
        match backend {
            StreamSourceBackend::Null => "null".to_owned(),
            StreamSourceBackend::COREAUDIO => "coreaudio".to_owned(),
        }
    }
}

impl TryFrom<&str> for StreamSourceBackend {
    type Error = ParametersError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "null" => Ok(StreamSourceBackend::Null),
            "coreaudio" => Ok(StreamSourceBackend::COREAUDIO),
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
    match backend {
        StreamSourceBackend::Null => {
            base::warn!("Using null sound backend, audio will not be functional");
            let mut generators: Vec<Box<dyn StreamSourceGenerator>> =
                Vec::with_capacity(snd_data.pcm_info_len());
            for _ in snd_data.pcm_info_iter() {
                generators.push(Box::new(audio_streams::NoopStreamSourceGenerator::new()));
            }
            generators
        }
        StreamSourceBackend::COREAUDIO => {
            let mut generators: Vec<Box<dyn StreamSourceGenerator>> =
                Vec::with_capacity(snd_data.pcm_info_len());
            for _ in snd_data.pcm_info_iter() {
                generators.push(Box::new(CoreAudioStreamSourceGenerator::new()));
            }
            generators
        }
    }
}

pub(crate) fn set_audio_thread_priority() -> Result<(), base::Error> {
    // macOS handles audio thread priority via CoreAudio's real-time thread.
    Ok(())
}

impl StreamInfo {
    async fn set_up_async_playback_stream(
        &mut self,
        frame_size: usize,
        ex: &Executor,
    ) -> Result<Box<dyn AsyncPlaybackBufferStream>, Error> {
        Ok(self
            .stream_source
            .as_mut()
            .ok_or(Error::EmptyStreamSource)?
            .async_new_async_playback_stream(
                self.channels as usize,
                self.format,
                self.frame_rate,
                self.period_bytes / frame_size,
                ex,
            )
            .await
            .map_err(Error::CreateStream)?
            .1)
    }

    pub(crate) async fn set_up_async_capture_stream(
        &mut self,
        frame_size: usize,
        ex: &Executor,
    ) -> Result<SysBufferReader, Error> {
        let async_capture_buffer_stream = self
            .stream_source
            .as_mut()
            .ok_or(Error::EmptyStreamSource)?
            .async_new_async_capture_stream(
                self.channels as usize,
                self.format,
                self.frame_rate,
                self.period_bytes / frame_size,
                &self.effects,
                ex,
            )
            .await
            .map_err(Error::CreateStream)?
            .1;
        Ok(SysBufferReader::new(async_capture_buffer_stream))
    }

    pub(crate) async fn create_directionstream_output(
        &mut self,
        frame_size: usize,
        ex: &Executor,
    ) -> Result<DirectionalStream, Error> {
        let async_playback_buffer_stream =
            self.set_up_async_playback_stream(frame_size, ex).await?;

        let buffer_writer = MacosBufferWriter::new(self.period_bytes);

        Ok(DirectionalStream::Output(SysDirectionOutput {
            async_playback_buffer_stream,
            buffer_writer: Box::new(buffer_writer),
        }))
    }
}

pub(crate) struct MacosBufferReader {
    async_stream: Box<dyn AsyncCaptureBufferStream>,
}

impl MacosBufferReader {
    fn new(async_stream: Box<dyn AsyncCaptureBufferStream>) -> Self {
        MacosBufferReader { async_stream }
    }
}

#[async_trait(?Send)]
impl CaptureBufferReader for MacosBufferReader {
    async fn get_next_capture_period(
        &mut self,
        ex: &Executor,
    ) -> Result<AsyncCaptureBuffer, BoxError> {
        Ok(self
            .async_stream
            .next_capture_buffer(ex)
            .await
            .map_err(Error::FetchBuffer)?)
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

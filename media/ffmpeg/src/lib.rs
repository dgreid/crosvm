// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(any(target_os = "android", target_os = "linux"))]

pub mod avcodec;
mod avutil;
pub use avutil::*;
mod error;
pub use error::*;
mod ffi {
    #![allow(clippy::missing_safety_doc)]
    #![allow(clippy::undocumented_unsafe_blocks)]
    #![allow(clippy::upper_case_acronyms)]
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

    /// SAFETY: `AVCodec` instances are all static and thus safe to share.
    unsafe impl Sync for AVCodec {}
}
pub mod swscale;

pub use ffi::AVPictureType_AV_PICTURE_TYPE_I;
pub use ffi::AVPixelFormat_AV_PIX_FMT_NV12;
pub use ffi::AVPixelFormat_AV_PIX_FMT_YUV420P;
pub use ffi::AVRational;
pub use ffi::AV_CODEC_CAP_DR1;
pub use ffi::AV_PKT_FLAG_KEY;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_H264_BASELINE as FF_PROFILE_H264_BASELINE;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_H264_EXTENDED as FF_PROFILE_H264_EXTENDED;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_H264_HIGH as FF_PROFILE_H264_HIGH;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_H264_HIGH_10 as FF_PROFILE_H264_HIGH_10;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_H264_HIGH_422 as FF_PROFILE_H264_HIGH_422;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_H264_HIGH_444_PREDICTIVE as FF_PROFILE_H264_HIGH_444_PREDICTIVE;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_H264_MAIN as FF_PROFILE_H264_MAIN;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_H264_MULTIVIEW_HIGH as FF_PROFILE_H264_MULTIVIEW_HIGH;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_H264_STEREO_HIGH as FF_PROFILE_H264_STEREO_HIGH;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_HEVC_MAIN as FF_PROFILE_HEVC_MAIN;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_HEVC_MAIN_10 as FF_PROFILE_HEVC_MAIN_10;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_HEVC_MAIN_STILL_PICTURE as FF_PROFILE_HEVC_MAIN_STILL_PICTURE;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_VP9_0 as FF_PROFILE_VP9_0;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_VP9_1 as FF_PROFILE_VP9_1;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_VP9_2 as FF_PROFILE_VP9_2;
#[cfg(ffmpeg_profile_prefix_av)]
pub use ffi::AV_PROFILE_VP9_3 as FF_PROFILE_VP9_3;
// FFmpeg 7.x renamed FF_PROFILE_* to AV_PROFILE_*. Re-export under the old names for compat.
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_H264_BASELINE;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_H264_EXTENDED;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_H264_HIGH;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_H264_HIGH_10;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_H264_HIGH_422;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_H264_HIGH_444_PREDICTIVE;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_H264_MAIN;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_H264_MULTIVIEW_HIGH;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_H264_STEREO_HIGH;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_HEVC_MAIN;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_HEVC_MAIN_10;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_HEVC_MAIN_STILL_PICTURE;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_VP9_0;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_VP9_1;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_VP9_2;
#[cfg(not(ffmpeg_profile_prefix_av))]
pub use ffi::FF_PROFILE_VP9_3;

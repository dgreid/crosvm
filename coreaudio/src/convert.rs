// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Sample format conversion between virtio-snd formats and CoreAudio Float32.

use audio_streams::SampleFormat;

pub fn guest_format_to_float32(src: &[u8], dst: &mut [f32], format: SampleFormat) {
    match format {
        SampleFormat::U8 => {
            for (i, &sample) in src.iter().enumerate() {
                if i < dst.len() {
                    dst[i] = (sample as f32 - 128.0) / 128.0;
                }
            }
        }
        SampleFormat::S16LE => {
            let samples = src.len() / 2;
            for i in 0..samples.min(dst.len()) {
                let s = i16::from_le_bytes([src[i * 2], src[i * 2 + 1]]);
                dst[i] = s as f32 / 32768.0;
            }
        }
        SampleFormat::S24LE => {
            let samples = src.len() / 4;
            for i in 0..samples.min(dst.len()) {
                let raw = i32::from_le_bytes([
                    src[i * 4],
                    src[i * 4 + 1],
                    src[i * 4 + 2],
                    src[i * 4 + 3],
                ]);
                // 24-bit sample stored in 32-bit container, sign-extend from 24 bits
                let s = (raw << 8) >> 8;
                dst[i] = s as f32 / 8388608.0;
            }
        }
        SampleFormat::S32LE => {
            let samples = src.len() / 4;
            for i in 0..samples.min(dst.len()) {
                let s = i32::from_le_bytes([
                    src[i * 4],
                    src[i * 4 + 1],
                    src[i * 4 + 2],
                    src[i * 4 + 3],
                ]);
                dst[i] = s as f32 / 2147483648.0;
            }
        }
    }
}

pub fn float32_to_guest_format(src: &[f32], dst: &mut [u8], format: SampleFormat) {
    match format {
        SampleFormat::U8 => {
            for (i, &sample) in src.iter().enumerate() {
                if i < dst.len() {
                    let clamped = sample.clamp(-1.0, 1.0);
                    dst[i] = (clamped * 128.0 + 128.0) as u8;
                }
            }
        }
        SampleFormat::S16LE => {
            for (i, &sample) in src.iter().enumerate() {
                let offset = i * 2;
                if offset + 1 < dst.len() {
                    let clamped = sample.clamp(-1.0, 1.0);
                    let s = (clamped * 32767.0) as i16;
                    let bytes = s.to_le_bytes();
                    dst[offset] = bytes[0];
                    dst[offset + 1] = bytes[1];
                }
            }
        }
        SampleFormat::S24LE => {
            for (i, &sample) in src.iter().enumerate() {
                let offset = i * 4;
                if offset + 3 < dst.len() {
                    let clamped = sample.clamp(-1.0, 1.0);
                    let s = (clamped * 8388607.0) as i32;
                    let bytes = s.to_le_bytes();
                    dst[offset] = bytes[0];
                    dst[offset + 1] = bytes[1];
                    dst[offset + 2] = bytes[2];
                    dst[offset + 3] = 0; // upper byte unused
                }
            }
        }
        SampleFormat::S32LE => {
            for (i, &sample) in src.iter().enumerate() {
                let offset = i * 4;
                if offset + 3 < dst.len() {
                    let clamped = sample.clamp(-1.0, 1.0);
                    let s = (clamped * 2147483647.0) as i32;
                    let bytes = s.to_le_bytes();
                    dst[offset] = bytes[0];
                    dst[offset + 1] = bytes[1];
                    dst[offset + 2] = bytes[2];
                    dst[offset + 3] = bytes[3];
                }
            }
        }
    }
}

pub fn sample_bytes_per_frame(format: SampleFormat, num_channels: usize) -> usize {
    format.sample_bytes() * num_channels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s16le_to_float32_roundtrip() {
        let s16_data: Vec<u8> = vec![
            0x00, 0x00, // 0 -> 0.0
            0xFF, 0x7F, // 32767 -> ~1.0
            0x00, 0x80, // -32768 -> -1.0
            0x00, 0x40, // 16384 -> 0.5
        ];
        let mut float_buf = vec![0.0f32; 4];
        guest_format_to_float32(&s16_data, &mut float_buf, SampleFormat::S16LE);

        assert!((float_buf[0] - 0.0).abs() < 0.001);
        assert!((float_buf[1] - 1.0).abs() < 0.001);
        assert!((float_buf[2] - (-1.0)).abs() < 0.001);
        assert!((float_buf[3] - 0.5).abs() < 0.001);

        let mut s16_out = vec![0u8; 8];
        float32_to_guest_format(&float_buf, &mut s16_out, SampleFormat::S16LE);

        assert_eq!(s16_out[0..2], [0x00, 0x00]); // 0
        let max_val = i16::from_le_bytes([s16_out[2], s16_out[3]]);
        assert!(max_val >= 32766, "expected ~32767, got {}", max_val);
        assert_eq!(
            i16::from_le_bytes([s16_out[4], s16_out[5]]),
            -32767 // slight precision loss
        );
    }

    #[test]
    fn u8_to_float32_roundtrip() {
        let u8_data = vec![128u8, 255, 0, 192];
        let mut float_buf = vec![0.0f32; 4];
        guest_format_to_float32(&u8_data, &mut float_buf, SampleFormat::U8);

        assert!((float_buf[0] - 0.0).abs() < 0.01);
        assert!((float_buf[1] - 1.0).abs() < 0.01);
        assert!((float_buf[2] - (-1.0)).abs() < 0.01);
        assert!((float_buf[3] - 0.5).abs() < 0.01);
    }

    #[test]
    fn s32le_to_float32() {
        let s32_data: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x00, // 0
            0xFF, 0xFF, 0xFF, 0x7F, // INT32_MAX
            0x00, 0x00, 0x00, 0x80, // INT32_MIN
        ];
        let mut float_buf = vec![0.0f32; 3];
        guest_format_to_float32(&s32_data, &mut float_buf, SampleFormat::S32LE);

        assert!((float_buf[0] - 0.0).abs() < 0.001);
        assert!((float_buf[1] - 1.0).abs() < 0.001);
        assert!((float_buf[2] - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn clamp_prevents_overflow() {
        let src = [1.5f32, -1.5];
        let mut dst = vec![0u8; 4];
        float32_to_guest_format(&src, &mut dst, SampleFormat::S16LE);

        assert_eq!(i16::from_le_bytes([dst[0], dst[1]]), 32767);
        assert_eq!(i16::from_le_bytes([dst[2], dst[3]]), -32767);
    }
}

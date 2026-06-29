// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Minimal FFI bindings for Apple's AudioToolbox framework.

#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(dead_code)]

use std::ffi::c_void;

pub type OSStatus = i32;
pub type AudioComponentInstance = *mut c_void;
pub type AudioUnit = AudioComponentInstance;
pub type AudioComponent = *mut c_void;

pub const noErr: OSStatus = 0;

// AudioUnit types and subtypes
pub const kAudioUnitType_Output: u32 = u32::from_be_bytes(*b"auou");
pub const kAudioUnitSubType_DefaultOutput: u32 = u32::from_be_bytes(*b"def ");
pub const kAudioUnitSubType_HALOutput: u32 = u32::from_be_bytes(*b"ahal");

// AudioUnit properties
pub const kAudioUnitProperty_StreamFormat: u32 = 8;
pub const kAudioUnitProperty_SetRenderCallback: u32 = 23;
pub const kAudioUnitProperty_EnableIO: u32 = 2003;

// AudioUnit scopes
pub const kAudioUnitScope_Input: u32 = 1;
pub const kAudioUnitScope_Output: u32 = 2;
pub const kAudioUnitScope_Global: u32 = 0;

// AudioStreamBasicDescription format flags
pub const kAudioFormatLinearPCM: u32 = u32::from_be_bytes(*b"lpcm");
pub const kAudioFormatFlagIsFloat: u32 = 1 << 0;
pub const kAudioFormatFlagIsBigEndian: u32 = 1 << 1;
pub const kAudioFormatFlagIsPacked: u32 = 1 << 3;
pub const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;

// AudioComponent manufacturer
pub const kAudioUnitManufacturer_Apple: u32 = u32::from_be_bytes(*b"appl");

#[repr(C)]
#[derive(Debug, Clone)]
pub struct AudioComponentDescription {
    pub component_type: u32,
    pub component_sub_type: u32,
    pub component_manufacturer: u32,
    pub component_flags: u32,
    pub component_flags_mask: u32,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct AudioStreamBasicDescription {
    pub sample_rate: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels_per_frame: u32,
    pub bits_per_channel: u32,
    pub reserved: u32,
}

impl AudioStreamBasicDescription {
    pub fn float32_interleaved(sample_rate: f64, channels: u32) -> Self {
        AudioStreamBasicDescription {
            sample_rate,
            format_id: kAudioFormatLinearPCM,
            format_flags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            bytes_per_packet: 4 * channels,
            frames_per_packet: 1,
            bytes_per_frame: 4 * channels,
            channels_per_frame: channels,
            bits_per_channel: 32,
            reserved: 0,
        }
    }
}

pub type AURenderCallback = unsafe extern "C" fn(
    in_ref_con: *mut c_void,
    io_action_flags: *mut u32,
    in_time_stamp: *const AudioTimeStamp,
    in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus;

#[repr(C)]
pub struct AURenderCallbackStruct {
    pub input_proc: AURenderCallback,
    pub input_proc_ref_con: *mut c_void,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct AudioTimeStamp {
    pub sample_time: f64,
    pub host_time: u64,
    pub rate_scalar: f64,
    pub word_clock_time: u64,
    pub smpte_time: SMPTETime,
    pub flags: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct SMPTETime {
    pub subframes: i16,
    pub subframe_divisor: i16,
    pub counter: u32,
    pub smpte_type: u32,
    pub flags: u32,
    pub hours: i16,
    pub minutes: i16,
    pub seconds: i16,
    pub frames: i16,
}

#[repr(C)]
pub struct AudioBufferList {
    pub number_buffers: u32,
    pub buffers: [AudioBuffer; 1], // Variable-length array
}

#[repr(C)]
pub struct AudioBuffer {
    pub number_channels: u32,
    pub data_byte_size: u32,
    pub data: *mut c_void,
}

extern "C" {
    pub fn AudioComponentFindNext(
        in_component: AudioComponent,
        in_desc: *const AudioComponentDescription,
    ) -> AudioComponent;

    pub fn AudioComponentInstanceNew(
        in_component: AudioComponent,
        out_instance: *mut AudioComponentInstance,
    ) -> OSStatus;

    pub fn AudioComponentInstanceDispose(in_instance: AudioComponentInstance) -> OSStatus;

    pub fn AudioUnitInitialize(in_unit: AudioUnit) -> OSStatus;

    pub fn AudioUnitUninitialize(in_unit: AudioUnit) -> OSStatus;

    pub fn AudioUnitSetProperty(
        in_unit: AudioUnit,
        in_id: u32,
        in_scope: u32,
        in_element: u32,
        in_data: *const c_void,
        in_data_size: u32,
    ) -> OSStatus;

    pub fn AudioUnitGetProperty(
        in_unit: AudioUnit,
        in_id: u32,
        in_scope: u32,
        in_element: u32,
        out_data: *mut c_void,
        io_data_size: *mut u32,
    ) -> OSStatus;

    pub fn AudioOutputUnitStart(ci: AudioUnit) -> OSStatus;

    pub fn AudioOutputUnitStop(ci: AudioUnit) -> OSStatus;
}

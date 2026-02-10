// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS implementation of VirtIO net platform-specific functions.
//!
//! macOS doesn't have Linux-style TAP devices. Networking on macOS should use
//! slirp (user-space networking) or vmnet.framework. This module provides
//! stub implementations that work with the TapT trait abstraction.

use std::io;
use std::io::Write;
use std::result;

use base::error;
use base::warn;
use base::EventType;
use base::ReadNotifier;
use base::WaitContext;
use net_util::TapT;
use virtio_sys::virtio_net;
use virtio_sys::virtio_net::virtio_net_hdr;
use zerocopy::IntoBytes;

use super::super::super::net::NetError;
use super::super::super::net::Token;
use super::super::super::net::Worker;
use super::super::super::Queue;
use super::PendingBuffer;

/// Validates and configures the tap interface.
///
/// On macOS, traditional TAP devices don't exist. This function is a stub
/// that accepts any TapT implementation (which on macOS would typically be
/// slirp or vmnet).
pub fn validate_and_configure_tap<T: TapT>(_tap: &T, _vq_pairs: u16) -> Result<(), NetError> {
    // On macOS, we don't have Linux-style TAP flags to validate.
    // The TapT trait implementation (slirp/vmnet) handles configuration.
    warn!("TAP validation not implemented on macOS, skipping");
    Ok(())
}

/// Converts virtio-net feature bits to tap's offload bits.
///
/// On macOS, TAP offload flags don't exist. Returns 0 as offloading
/// is handled differently (or not at all) on macOS.
pub fn virtio_features_to_tap_offload(_features: u64) -> u32 {
    // macOS doesn't have TUN_F_* offload flags like Linux.
    // Offloading is handled by the network backend (slirp/vmnet).
    0
}

/// Process RX with mergeable receive buffers.
///
/// This implementation is the same as Linux - the platform differences
/// are abstracted by the TapT trait.
pub fn process_mrg_rx<T: TapT>(
    rx_queue: &mut Queue,
    tap: &mut T,
    pending: &mut PendingBuffer,
) -> result::Result<(), NetError> {
    let mut needs_interrupt = false;
    let mut exhausted_queue = false;

    loop {
        // Refill `pending` if it is empty.
        if pending.length == 0 {
            match tap.read(&mut *pending.buffer) {
                Ok(length) => {
                    pending.length = length as u32;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // No more to read from the tap.
                    break;
                }
                Err(e) => {
                    warn!("net: rx: failed to read from tap: {}", e);
                    return Err(NetError::WriteBuffer(e));
                }
            }
        }
        if pending.length == 0 {
            break;
        }
        let packet_len = pending.length;
        let Some(mut desc_list) = rx_queue.try_pop_length(packet_len as usize) else {
            // If vq is exhausted, pending buffer should be used firstly
            // instead of reading from tap in next loop.
            exhausted_queue = true;
            break;
        };
        let num_buffers = desc_list.len() as u16;

        // Copy the num_buffers value to specified address
        let num_buffers_offset = std::mem::size_of::<virtio_net_hdr>();
        pending.buffer[num_buffers_offset..num_buffers_offset + 2]
            .copy_from_slice(num_buffers.as_bytes());
        let mut offset = 0;
        let end = packet_len as usize;
        for desc in desc_list.iter_mut() {
            let writer = &mut desc.writer;
            let bytes_written = match writer.write(&pending.buffer[offset..end]) {
                Ok(n) => n,
                Err(e) => {
                    warn!(
                        "net: mrg_rx: failed to write slice from pending buffer: {}",
                        e
                    );
                    return Err(NetError::WriteBuffer(e));
                }
            };
            offset += bytes_written;
        }
        rx_queue.add_used_batch(desc_list);

        needs_interrupt = true;
        pending.length = 0;
    }

    if needs_interrupt {
        rx_queue.trigger_interrupt();
    }

    if exhausted_queue {
        Err(NetError::RxDescriptorsExhausted)
    } else {
        Ok(())
    }
}

/// Process RX without mergeable receive buffers.
pub fn process_rx<T: TapT>(rx_queue: &mut Queue, mut tap: &mut T) -> result::Result<(), NetError> {
    let mut needs_interrupt = false;
    let mut exhausted_queue = false;

    // Read as many frames as possible.
    loop {
        let mut desc_chain = match rx_queue.peek() {
            Some(desc) => desc,
            None => {
                exhausted_queue = true;
                break;
            }
        };

        let writer = &mut desc_chain.writer;

        match writer.write_from(&mut tap, writer.available_bytes()) {
            Ok(_) => {}
            Err(ref e) if e.kind() == io::ErrorKind::WriteZero => {
                warn!("net: rx: buffer is too small to hold frame");
                break;
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // No more to read from the tap.
                break;
            }
            Err(e) => {
                warn!("net: rx: failed to write slice: {}", e);
                return Err(NetError::WriteBuffer(e));
            }
        };

        let bytes_written = writer.bytes_written() as u32;
        cros_tracing::trace_simple_print!("{bytes_written} bytes read from tap");

        if bytes_written > 0 {
            let desc_chain = desc_chain.pop();
            rx_queue.add_used(desc_chain);
            needs_interrupt = true;
        }
    }

    if needs_interrupt {
        rx_queue.trigger_interrupt();
    }

    if exhausted_queue {
        Err(NetError::RxDescriptorsExhausted)
    } else {
        Ok(())
    }
}

/// Process TX - send packets from guest to network.
pub fn process_tx<T: TapT>(tx_queue: &mut Queue, mut tap: &mut T) {
    while let Some(mut desc_chain) = tx_queue.pop() {
        let reader = &mut desc_chain.reader;
        let expected_count = reader.available_bytes();
        match reader.read_to(&mut tap, expected_count) {
            Ok(count) => {
                // Tap writes must be done in one call. If the entire frame was not
                // written, it's an error.
                if count != expected_count {
                    error!(
                        "net: tx: wrote only {} bytes of {} byte frame",
                        count, expected_count
                    );
                }
                cros_tracing::trace_simple_print!("{count} bytes write to tap");
            }
            Err(e) => error!("net: tx: failed to write frame to tap: {}", e),
        }

        tx_queue.add_used(desc_chain);
    }

    tx_queue.trigger_interrupt();
}

impl<T> Worker<T>
where
    T: TapT + ReadNotifier,
{
    pub(in crate::virtio) fn handle_rx_token(
        &mut self,
        wait_ctx: &WaitContext<Token>,
        pending_buffer: &mut PendingBuffer,
    ) -> result::Result<(), NetError> {
        match self.process_rx(pending_buffer) {
            Ok(()) => Ok(()),
            Err(NetError::RxDescriptorsExhausted) => {
                wait_ctx
                    .modify(&self.tap, EventType::None, Token::RxTap)
                    .map_err(NetError::WaitContextDisableTap)?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub(in crate::virtio) fn handle_rx_queue(
        &mut self,
        wait_ctx: &WaitContext<Token>,
        tap_polling_enabled: bool,
    ) -> result::Result<(), NetError> {
        if !tap_polling_enabled {
            wait_ctx
                .modify(&self.tap, EventType::Read, Token::RxTap)
                .map_err(NetError::WaitContextEnableTap)?;
        }
        Ok(())
    }

    pub(super) fn process_rx(
        &mut self,
        pending_buffer: &mut PendingBuffer,
    ) -> result::Result<(), NetError> {
        if self.acked_features & 1 << virtio_net::VIRTIO_NET_F_MRG_RXBUF == 0 {
            process_rx(&mut self.rx_queue, &mut self.tap)
        } else {
            process_mrg_rx(&mut self.rx_queue, &mut self.tap, pending_buffer)
        }
    }
}

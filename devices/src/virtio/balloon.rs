// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std;
use std::cell::RefCell;
use std::convert::TryFrom;
use std::fmt::{self, Display};
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::task::{Context, Poll};
use std::future::Future;
use std::pin::Pin;

use futures::StreamExt;

use async_core::{EventFd as AsyncEventFd, MsgReceiver as AsyncReceiver};
use cros_async::{SelectResult,select5};
use data_model::{DataInit, Le32};
use sys_util::{self, error, info, warn, EventFd, GuestAddress, GuestMemory};
use vm_control::{BalloonControlCommand, BalloonControlResponseSocket};

use super::{
    copy_config, DescriptorChain, Interrupt, Queue, Reader, VirtioDevice, TYPE_BALLOON,
    VIRTIO_F_VERSION_1,
};

#[derive(Debug)]
pub enum BalloonError {
    /// Request to adjust memory size can't provide the number of pages requested.
    NotEnoughPages,
    /// Failure wriitng the config notification event.
    WritingConfigEvent(sys_util::Error),
}
pub type Result<T> = std::result::Result<T, BalloonError>;

impl Display for BalloonError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::BalloonError::*;

        match self {
            NotEnoughPages => write!(f, "not enough pages"),
            WritingConfigEvent(e) => write!(f, "failed to write config event: {}", e),
        }
    }
}

// Balloon has three virt IO queues: Inflate, Deflate, and Stats.
// Stats is currently not used.
const QUEUE_SIZE: u16 = 128;
const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE, QUEUE_SIZE];

const VIRTIO_BALLOON_PFN_SHIFT: u32 = 12;

// The feature bitmap for virtio balloon
const VIRTIO_BALLOON_F_MUST_TELL_HOST: u32 = 0; // Tell before reclaiming pages
const VIRTIO_BALLOON_F_DEFLATE_ON_OOM: u32 = 2; // Deflate balloon on OOM

// virtio_balloon_config is the ballon device configuration space defined by the virtio spec.
#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtio_balloon_config {
    num_pages: Le32,
    actual: Le32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for virtio_balloon_config {}

// BalloonConfig is modified by the worker and read from the device thread.
#[derive(Default)]
struct BalloonConfig {
    num_pages: AtomicUsize,
    actual_pages: AtomicUsize,
}

async fn handle_inflate(
    mut mem: GuestMemory,
    mut queue: Queue,
    queue_evt: EventFd,
    interrupt: Rc<RefCell<Interrupt>>,
) {
    let mut desc_mem = mem.clone();
    let mut inflate_queue_evt = AsyncEventFd::try_from(queue_evt).unwrap();
    let mut desc_stream = queue.stream(&mut desc_mem, &mut inflate_queue_evt);
    while let Some(avail_desc) = desc_stream.next().await {
        let index = avail_desc.index;
        if process_inflate(avail_desc, &mut mem) {
            desc_stream.queue_mut().add_used(&mut mem, index, 0);
            interrupt
                .borrow_mut()
                .signal_used_queue(desc_stream.queue_mut().vector);
        }
    }
}

async fn handle_deflate(
    mut mem: GuestMemory,
    mut queue: Queue,
    queue_evt: EventFd,
    interrupt: Rc<RefCell<Interrupt>>,
) {
    let mut desc_mem = mem.clone();
    let mut deflate_queue_evt = AsyncEventFd::try_from(queue_evt).unwrap();
    let mut desc_stream = queue.stream(&mut desc_mem, &mut deflate_queue_evt);
    while let Some(avail_desc) = desc_stream.next().await {
        let index = avail_desc.index;
        // Do nothing with deflate
        desc_stream.queue_mut().add_used(&mut mem, index, 0);
        interrupt
            .borrow_mut()
            .signal_used_queue(desc_stream.queue_mut().vector);
    }
}

async fn handle_command_socket(
    command_socket: &BalloonControlResponseSocket,
    interrupt: Rc<RefCell<Interrupt>>,
    config: Arc<BalloonConfig>,
) {
    let mut command_socket = AsyncReceiver::from_receiver(command_socket).unwrap();
    while let Some(req) = command_socket.next().await {
        match req {
            BalloonControlCommand::Adjust { num_bytes } => {
                let num_pages = (num_bytes >> VIRTIO_BALLOON_PFN_SHIFT) as usize;
                info!("ballon config changed to consume {} pages", num_pages);

                config.num_pages.store(num_pages, Ordering::Relaxed);
                interrupt.borrow_mut().signal_config_changed();
            }
        }
    }
}

// Because of the requirement to pull the command socket back from an uncompleted future,
// `CommandSocketFuture` wraps what would otherwise be an async fn, but adds the ability to extract
// the socket from the future without completing it first.
struct CommandSocketFuture {
    command_socket: Option<AsyncReceiver<BalloonControlCommand, BalloonControlResponseSocket>>,
    interrupt: Rc<RefCell<Interrupt>>,
    config: Arc<BalloonConfig>,
}

impl CommandSocketFuture {
    fn new(
    command_socket: BalloonControlResponseSocket,
    interrupt: Rc<RefCell<Interrupt>>,
    config: Arc<BalloonConfig>,) -> Self {
        CommandSocketFuture {
            command_socket: Some(AsyncReceiver::from_receiver(command_socket).unwrap()),
            interrupt,
            config,
        }
    }
}

impl Future for CommandSocketFuture {
    type Output = BalloonControlResponseSocket;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            let req = if let Some(command_socket) = self.command_socket.as_mut() {
                command_socket.poll_next_unpin(cx)
            } else {
                panic!("polled without a command_socket");
            };
            match req {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(req) => {
                    match req {
                        Some(BalloonControlCommand::Adjust { num_bytes }) => {
                            let num_pages = (num_bytes >> VIRTIO_BALLOON_PFN_SHIFT) as usize;
                            info!("ballon config changed to consume {} pages", num_pages);

                            self.config.num_pages.store(num_pages, Ordering::Relaxed);
                            self.interrupt.borrow_mut().signal_config_changed();
                        }
                        None => {
                            break
                        }
                    }
                },
            }
        }

        Poll::Ready(self.command_socket.take().unwrap().into_inner())
    }
}

async fn handle_irq_resample(interrupt: Rc<RefCell<Interrupt>>) {
    let resample_evt = interrupt
        .borrow_mut()
        .get_resample_evt()
        .try_clone()
        .unwrap();
    let mut resample_evt = AsyncEventFd::try_from(resample_evt).unwrap();
    loop {
        if let Some(_) = resample_evt.next().await {
            interrupt.borrow_mut().interrupt_resample();
        } else {
            break;
        }
    }
}

async fn wait_kill(kill_evt: EventFd) {
    let mut kill_evt = AsyncEventFd::try_from(kill_evt).unwrap();
    // Once this event is readable, exit. Exiting this future will cause the main loop to
    // break and the device process to exit.
    kill_evt.next().await;
}

fn run_worker(
    mut queue_evts: Vec<EventFd>,
    mut queues: Vec<Queue>,
    command_socket: BalloonControlResponseSocket,
    interrupt: Interrupt,
    kill_evt: EventFd,
    mem: GuestMemory,
    config: Arc<BalloonConfig>,
) -> BalloonControlResponseSocket {
    // Wrap the interupt in a `RefCell` so it can be shared between async functions.
    let interrupt = Rc::new(RefCell::new(interrupt));

    // The first queue is used for inflate messages
    let inflate = handle_inflate(
        mem.clone(),
        queues.remove(0),
        queue_evts.remove(0),
        interrupt.clone(),
    );
    // The second queue is used for deflate messages
    let deflate = handle_deflate(
        mem.clone(),
        queues.remove(0),
        queue_evts.remove(0),
        interrupt.clone(),
    );
    // Future to handle command messages that resize the balloon.
    let socket = CommandSocketFuture::new(command_socket, interrupt.clone(), config);
    // Process any requests to resample the irq value.
    let resample = handle_irq_resample(interrupt.clone());
    // Exit if the kill event is triggered.
    let kill = wait_kill(kill_evt);

    // And return once any future exits.
    let (_, _, socket, _, _) = select5(inflate, deflate, socket, resample, kill);
    match socket {
        SelectResult::Finished(s) => s,
        SelectResult::Pending(f) => f.command_socket.unwrap().into_inner(),
    }
}

/// Virtio device for memory balloon inflation/deflation.
pub struct Balloon {
    command_socket: Option<BalloonControlResponseSocket>,
    config: Arc<BalloonConfig>,
    features: u64,
    kill_evt: Option<EventFd>,
    worker_thread: Option<thread::JoinHandle<BalloonControlResponseSocket>>,
}

impl Balloon {
    /// Create a new virtio balloon device.
    pub fn new(command_socket: BalloonControlResponseSocket) -> Result<Balloon> {
        Ok(Balloon {
            command_socket: Some(command_socket),
            config: Arc::new(BalloonConfig {
                num_pages: AtomicUsize::new(0),
                actual_pages: AtomicUsize::new(0),
            }),
            kill_evt: None,
            worker_thread: None,
            // TODO(dgreid) - Add stats queue feature.
            features: 1 << VIRTIO_BALLOON_F_MUST_TELL_HOST | 1 << VIRTIO_BALLOON_F_DEFLATE_ON_OOM,
        })
    }

    fn get_config(&self) -> virtio_balloon_config {
        let num_pages = self.config.num_pages.load(Ordering::Relaxed) as u32;
        let actual_pages = self.config.actual_pages.load(Ordering::Relaxed) as u32;
        virtio_balloon_config {
            num_pages: num_pages.into(),
            actual: actual_pages.into(),
        }
    }
}

impl Drop for Balloon {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do with a failure.
            let _ = kill_evt.write(1);
        }

        if let Some(worker_thread) = self.worker_thread.take() {
            let _ = worker_thread.join();
        }
    }
}

impl VirtioDevice for Balloon {
    fn keep_fds(&self) -> Vec<RawFd> {
        vec![self.command_socket.as_ref().unwrap().as_raw_fd()]
    }

    fn device_type(&self) -> u32 {
        TYPE_BALLOON
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        copy_config(data, 0, self.get_config().as_slice(), offset);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let mut config = self.get_config();
        copy_config(config.as_mut_slice(), offset, data, 0);
        self.config
            .actual_pages
            .store(config.actual.to_native() as usize, Ordering::Relaxed);
    }

    fn features(&self) -> u64 {
        1 << VIRTIO_BALLOON_F_MUST_TELL_HOST
            | 1 << VIRTIO_BALLOON_F_DEFLATE_ON_OOM
            | 1 << VIRTIO_F_VERSION_1
    }

    fn ack_features(&mut self, value: u64) {
        self.features &= value;
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Interrupt,
        queues: Vec<Queue>,
        queue_evts: Vec<EventFd>,
    ) {
        if queues.len() != QUEUE_SIZES.len() || queue_evts.len() != QUEUE_SIZES.len() {
            return;
        }

        let (self_kill_evt, kill_evt) = match EventFd::new().and_then(|e| Ok((e.try_clone()?, e))) {
            Ok(v) => v,
            Err(e) => {
                error!("failed to create kill EventFd pair: {}", e);
                return;
            }
        };
        self.kill_evt = Some(self_kill_evt);

        let config = self.config.clone();
        let command_socket = self.command_socket.take().unwrap();
        let worker_result = thread::Builder::new()
            .name("virtio_balloon".to_string())
            .spawn(move || {
                run_worker(
                    queue_evts,
                    queues,
                    command_socket,
                    interrupt,
                    kill_evt,
                    mem,
                    config,
                )});

        match worker_result {
            Err(e) => {
                error!("failed to spawn virtio_balloon worker: {}", e);
            }
            Ok(join_handle) => {
                self.worker_thread = Some(join_handle);
            }
        }
    }

    fn reset(&mut self) -> bool {
        if let Some(kill_evt) = self.kill_evt.take() {
            if kill_evt.write(1).is_err() {
                error!("{}: failed to notify the kill event", self.debug_label());
                return false;
            }
        }

        if let Some(worker_thread) = self.worker_thread.take() {
            match worker_thread.join() {
                Err(_) => {
                    error!("{}: failed to get back resources", self.debug_label());
                    return false;
                }
                Ok(command_socket) => {
                    self.command_socket = Some(command_socket);
                    return true;
                }
            }
        }
        return false;
    }
}

fn process_inflate(avail_desc: DescriptorChain, mem: &mut GuestMemory) -> bool {
    let mut reader = match Reader::new(mem, avail_desc) {
        Ok(r) => r,
        Err(e) => {
            error!("balloon: failed to create reader: {}", e);
            return true;
        }
    };
    let data_length = reader.available_bytes();

    if data_length % 4 != 0 {
        error!("invalid inflate buffer size: {}", data_length);
        return true;
    }

    let num_addrs = data_length / 4;
    for _ in 0..num_addrs as usize {
        let guest_input = match reader.read_obj::<Le32>() {
            Ok(a) => a.to_native(),
            Err(err) => {
                error!("error while reading unused pages: {}", err);
                return false;
            }
        };
        let guest_address = GuestAddress((u64::from(guest_input)) << VIRTIO_BALLOON_PFN_SHIFT);

        if mem
            .remove_range(guest_address, 1 << VIRTIO_BALLOON_PFN_SHIFT)
            .is_err()
        {
            warn!("Marking pages unused failed; addr={}", guest_address);
            return false;
        }
    }
    true
}

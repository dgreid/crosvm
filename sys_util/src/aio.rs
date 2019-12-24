// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::os::unix::io::AsRawFd;

use crate::aio_abi_bindings::{
    aio_context_t, io_destroy, io_event, io_getevents, io_setup, io_submit, iocb, poll_iocb,
};
use crate::Result;
use crate::{PollToken, WatchingEvents};

pub struct Aio {
    context: aio_context_t,
    events_ret: Vec<io_event>,
}

impl Aio {
    pub fn new(nr_events: usize) -> Result<Aio> {
        let context = io_setup(nr_events)?;
        Ok(Aio {
            context,
            events_ret: vec![Default::default(); nr_events],
        })
    }

    pub fn submit_cb<T: PollToken>(&mut self, cb: AioCb) -> Result<()> {
        let cbs = [cb.cb];
        unsafe {
            // Safe because of the checks when creating an AioCb.
            io_submit(self.context, &cbs)?;
        }
        Ok(())
    }

    pub fn events(&mut self) -> Result<impl Iterator<Item = &io_event>> {
        let num_events = io_getevents(self.context, &mut self.events_ret)?;
        Ok(self.events_ret[..num_events].iter())
    }
}

impl Drop for Aio {
    fn drop(&mut self) {
        io_destroy(self.context);
    }
}

pub struct AioCb {
    cb: iocb,
}

impl AioCb {
    pub fn new<T: PollToken>(fd: &dyn AsRawFd, events: WatchingEvents, token: T) -> AioCb {
        AioCb {
            cb: poll_iocb(
                token.as_raw_token(),
                fd.as_raw_fd(),
                events.get_raw().into(),
            ),
        }
    }
}

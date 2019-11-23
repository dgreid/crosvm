// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::future::Future;
use std::io::{stdin, Read, StdinLock};
use std::pin::Pin;
use std::task::{Context, Poll};

use cros_async::{add_read_waker, FdExecutor};

struct VectorProducer<'a> {
    stdin_lock: StdinLock<'a>,
    started: bool, // hack because first poll can't check stdin for readable.
}

impl<'a> VectorProducer<'a> {
    pub fn new(stdin_lock: StdinLock<'a>) -> Self {
        VectorProducer {
            stdin_lock,
            started: false,
        }
    }
}

impl<'a> Future for VectorProducer<'a> {
    type Output = Vec<u8>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        println!("poll");
        if self.started {
            let mut b = [0u8; 2];
            self.stdin_lock.read(&mut b).unwrap();
            if b[0] >= b'0' && b[0] <= b'9' {
                return Poll::Ready((0..(b[0] - b'0')).collect());
            }
        }
        self.started = true;
        add_read_waker(&self.stdin_lock, cx.waker().clone());
        Poll::Pending
    }
}

fn main() {
    let mut ex = FdExecutor::new();

    async fn get_vec() {
        let stdin = stdin();
        let stdin_lock = stdin.lock();

        let vec_future = VectorProducer::new(stdin_lock);
        println!("Hello from async closure.");
        let buf = vec_future.await;
        println!("Hello from async closure again {}.", buf.len());
    }
    println!("pre add future");

    ex.add_future(Box::pin(get_vec()));
    println!("after adding, before runninc");

    ex.run();
}

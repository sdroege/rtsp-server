// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::channel::oneshot;
use futures::prelude::*;

use std::error;
use std::fmt;

use std::pin::Pin;
use std::task::{Context, Poll};

/// Oneshot receiver.
#[derive(Debug)]
pub struct Receiver<T>(oneshot::Receiver<T>);

/// Oneshot sender.
#[derive(Debug)]
pub struct Sender<T>(oneshot::Sender<T>);

pub fn channel<T: Send + 'static>() -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = oneshot::channel();
    (Sender(sender), Receiver(receiver))
}

impl<T: Send + 'static> Sender<T> {
    /// Send the item.
    pub fn send(self, item: T) -> Result<(), Disconnected> {
        self.0.send(item).map_err(|_| Disconnected)
    }
}

/// Send error.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Disconnected;

impl error::Error for Disconnected {}
impl fmt::Display for Disconnected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Channel is disconnected")
    }
}

impl<T: Send + 'static> Future for Receiver<T> {
    type Output = Result<T, Disconnected>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().0)
            .poll(cx)
            .map_err(|_| Disconnected)
    }
}

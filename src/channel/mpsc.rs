// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::channel::mpsc;
use futures::prelude::*;

use std::error;
use std::fmt;

use std::marker::{PhantomData, Unpin};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// MPSC Receiver.
#[derive(Debug)]
pub struct Receiver<T>(ReceiverInner<T>);

enum ReceiverInner<T> {
    Plain(mpsc::Receiver<T>),
    Mapped(Box<dyn ReceiveMap<T> + Send + Sync>),
}

impl<T> fmt::Debug for ReceiverInner<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReceiverInner::Plain(_) => f.debug_struct("Plain").finish(),
            ReceiverInner::Mapped(_) => f.debug_struct("Mapped").finish(),
        }
    }
}

/// MPSC Sender.
#[derive(Debug)]
pub struct Sender<T>(SenderInner<T>);

enum SenderInner<T> {
    Plain(mpsc::Sender<T>),
    Mapped(Box<dyn SendMap<T> + Send + Sync>),
}

impl<T: Send + 'static> Clone for Sender<T> {
    fn clone(&self) -> Self {
        match self.0 {
            SenderInner::Plain(ref sender) => Sender(SenderInner::Plain(sender.clone())),
            SenderInner::Mapped(ref sender) => Sender(SenderInner::Mapped((&**sender).clone())),
        }
    }
}

impl<T> fmt::Debug for SenderInner<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SenderInner::Plain(_) => f.debug_struct("Plain").finish(),
            SenderInner::Mapped(_) => f.debug_struct("Mapped").finish(),
        }
    }
}

pub fn channel<T: Send + 'static>() -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = mpsc::channel(100);

    (
        Sender(SenderInner::Plain(sender)),
        Receiver(ReceiverInner::Plain(receiver)),
    )
}

impl<T: Send + 'static> Sender<T> {
    /// Close the channel from the receiver.
    pub fn close_channel(&mut self) {
        match self.0 {
            SenderInner::Plain(ref mut sender) => sender.close_channel(),
            SenderInner::Mapped(ref mut sender) => sender.close_channel(),
        }
    }

    /// Disconnect this sender.
    pub fn disconnect(&mut self) {
        match self.0 {
            SenderInner::Plain(ref mut sender) => sender.disconnect(),
            SenderInner::Mapped(ref mut sender) => sender.disconnect(),
        }
    }

    /// Try sending an item.
    pub fn try_send(&mut self, msg: T) -> Result<(), SendError> {
        match self.0 {
            SenderInner::Plain(ref mut sender) => {
                sender.try_send(msg).map_err(SendError::from_try_send_error)
            }
            SenderInner::Mapped(ref mut sender) => sender.try_send(msg),
        }
    }

    /// Map a closure over all items.
    pub fn map<U: Send + Unpin + 'static, F: Fn(U) -> T + Unpin + Send + Sync + 'static>(
        self,
        func: F,
    ) -> Sender<U> {
        Sender(SenderInner::Mapped(Box::new(SenderMap {
            sender: self,
            func: Arc::new(func),
            phantom: PhantomData,
        })))
    }
}

impl<T: Send + 'static> Receiver<T> {
    /// Close the channel.
    pub fn close(&mut self) {
        match self.0 {
            ReceiverInner::Plain(ref mut receiver) => receiver.close(),
            ReceiverInner::Mapped(ref mut receiver) => receiver.close(),
        }
    }

    /// Map a closure over all items.
    pub fn map<U: Send + 'static, F: Fn(T) -> U + Unpin + Send + Sync + 'static>(
        self,
        func: F,
    ) -> Receiver<U> {
        Receiver(ReceiverInner::Mapped(Box::new(ReceiverMap {
            receiver: self,
            func,
        })))
    }
}

/// Send error.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SendError {
    /// Channel is full.
    Full,
    /// Channel is disconnected.
    Disconnected,
}

impl SendError {
    pub fn is_full(self) -> bool {
        matches!(self, SendError::Full)
    }

    pub fn is_disconnected(self) -> bool {
        matches!(self, SendError::Disconnected)
    }

    fn from_try_send_error<T>(err: mpsc::TrySendError<T>) -> Self {
        if err.is_full() {
            SendError::Full
        } else if err.is_disconnected() {
            SendError::Disconnected
        } else {
            unimplemented!()
        }
    }

    fn from_send_error(err: mpsc::SendError) -> Self {
        if err.is_full() {
            SendError::Full
        } else if err.is_disconnected() {
            SendError::Disconnected
        } else {
            unimplemented!()
        }
    }
}

impl error::Error for SendError {}
impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Full => write!(f, "Channel is full"),
            SendError::Disconnected => write!(f, "Channel is disconnected"),
        }
    }
}

trait SendMap<U: Send + 'static>: Send + Unpin + Sink<U, Error = SendError> {
    fn close_channel(&mut self);
    fn disconnect(&mut self);
    fn try_send(&mut self, msg: U) -> Result<(), SendError>;

    fn clone(&self) -> Box<dyn SendMap<U> + Send + Sync>;
}

struct SenderMap<T, U: Unpin, F: Fn(U) -> T + Unpin + Send + Sync + 'static> {
    sender: Sender<T>,
    func: Arc<F>,
    phantom: PhantomData<Sender<U>>,
}

impl<
        T: Send + 'static,
        U: Send + Unpin + 'static,
        F: Fn(U) -> T + Unpin + Send + Sync + 'static,
    > SendMap<U> for SenderMap<T, U, F>
{
    fn close_channel(&mut self) {
        self.sender.close_channel();
    }

    fn disconnect(&mut self) {
        self.sender.disconnect();
    }

    fn try_send(&mut self, msg: U) -> Result<(), SendError> {
        self.sender.try_send((self.func)(msg))
    }

    fn clone(&self) -> Box<dyn SendMap<U> + Send + Sync> {
        Box::new(SenderMap {
            sender: self.sender.clone(),
            func: self.func.clone(),
            phantom: PhantomData,
        })
    }
}

impl<
        T: Send + 'static,
        U: Send + Unpin + 'static,
        F: Fn(U) -> T + Unpin + Send + Sync + 'static,
    > Sink<U> for SenderMap<T, U, F>
{
    type Error = SendError;

    fn start_send(self: Pin<&mut Self>, item: U) -> Result<(), Self::Error> {
        let SenderMap {
            ref mut sender,
            ref func,
            ..
        } = self.get_mut();

        Pin::new(sender).start_send(func(item))
    }

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().sender).poll_ready(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().sender).poll_close(cx)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().sender).poll_flush(cx)
    }
}

impl<T: Send + 'static> Sink<T> for Sender<T> {
    type Error = SendError;

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let inner = &mut self.get_mut().0;

        match inner {
            SenderInner::Plain(ref mut sender) => Pin::new(sender)
                .start_send(item)
                .map_err(SendError::from_send_error),
            SenderInner::Mapped(ref mut sender) => Pin::new(sender).start_send(item),
        }
    }

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let inner = &mut self.get_mut().0;

        match inner {
            SenderInner::Plain(ref mut sender) => Pin::new(sender)
                .poll_ready(cx)
                .map_err(SendError::from_send_error),
            SenderInner::Mapped(ref mut sender) => Pin::new(sender).poll_ready(cx),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let inner = &mut self.get_mut().0;

        match inner {
            SenderInner::Plain(ref mut sender) => Pin::new(sender)
                .poll_close(cx)
                .map_err(SendError::from_send_error),
            SenderInner::Mapped(ref mut sender) => Pin::new(sender).poll_close(cx),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let inner = &mut self.get_mut().0;

        match inner {
            SenderInner::Plain(ref mut sender) => Pin::new(sender)
                .poll_flush(cx)
                .map_err(SendError::from_send_error),
            SenderInner::Mapped(ref mut sender) => Pin::new(sender).poll_flush(cx),
        }
    }
}

trait ReceiveMap<U: Send + 'static>: Send + Unpin + Stream<Item = U> {
    fn close(&mut self);
}

struct ReceiverMap<T, U, F: Fn(T) -> U + Unpin + Send + Sync + 'static> {
    receiver: Receiver<T>,
    func: F,
}

impl<T: Send + 'static> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let inner = &mut self.get_mut().0;
        match inner {
            ReceiverInner::Plain(ref mut receiver) => Pin::new(receiver).poll_next(cx),
            ReceiverInner::Mapped(ref mut receiver) => Pin::new(receiver).poll_next(cx),
        }
    }
}

impl<T: Send + 'static, U: Send + 'static, F: Fn(T) -> U + Unpin + Send + Sync + 'static>
    ReceiveMap<U> for ReceiverMap<T, U, F>
{
    fn close(&mut self) {
        self.receiver.close();
    }
}

impl<T: Send + 'static, U: Send + 'static, F: Fn(T) -> U + Unpin + Send + Sync + 'static> Stream
    for ReceiverMap<T, U, F>
{
    type Item = U;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let ReceiverMap {
            ref mut receiver,
            ref func,
        } = self.get_mut();

        Pin::new(receiver).poll_next(cx).map(|item| item.map(func))
    }
}

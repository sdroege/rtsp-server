use futures::prelude::*;
use std::task::{Context, Poll};

use std::collections::HashMap;
use std::pin::Pin;

use log::trace;

use crate::channel::mpsc;

type StreamId = u128;
type StreamFunc<H, Ctx> =
    Box<dyn FnMut(&mut H, &mut Ctx, &mut Context<'_>) -> Poll<Option<()>> + Send>;

/// Collection of `Stream`s and corresponding `MessageHandler`s.
///
/// Allows polling them and gets new streams registered with the corresponding
/// `StreamRegistration`.
pub struct StreamHandler<H: ?Sized + 'static, Ctx: ?Sized + 'static> {
    streams: HashMap<StreamId, StreamFunc<H, Ctx>>,
    receiver: mpsc::Receiver<(StreamId, StreamFunc<H, Ctx>)>,
}

pub struct StreamRegistration<H: ?Sized + 'static, Ctx: ?Sized + 'static> {
    next_stream_id: StreamId,
    sender: mpsc::Sender<(StreamId, StreamFunc<H, Ctx>)>,
}

impl<H: ?Sized + 'static, Ctx: ?Sized + 'static> StreamRegistration<H, Ctx> {
    /// Register `stream` and its corresponding `token` with this instance.
    pub fn register<Msg, Token: Send + 'static, S: Stream<Item = Msg> + Unpin + Send + 'static>(
        &mut self,
        token: Token,
        mut stream: S,
    ) -> Result<(), mpsc::SendError>
    where
        H: MessageHandler<Msg, Token, Context = Ctx>,
    {
        let stream_id = self.next_stream_id;
        self.next_stream_id += 1;

        trace!("Registering new stream with id {}", stream_id);

        let func = move |h: &mut H, ctx: &mut Ctx, cx: &mut Context<'_>| match Pin::new(&mut stream)
            .poll_next(cx)
        {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(item)) => {
                <H as MessageHandler<Msg, Token>>::handle_message(h, ctx, &token, item);
                Poll::Ready(Some(()))
            }
            Poll::Ready(None) => {
                <H as MessageHandler<Msg, Token>>::finished(h, ctx, &token);
                Poll::Ready(None)
            }
        };

        self.sender.try_send((stream_id, Box::new(func)))
    }

    /// Close the stream registration.
    ///
    /// Any further `register()` calls will return `SendError::Disconnect`.
    pub fn close(&mut self) {
        self.sender.close_channel();
    }
}

impl<H: ?Sized + 'static, Ctx: ?Sized + 'static> StreamHandler<H, Ctx> {
    /// Create a new `StreamHandler` and its corresponding `StreamRegistration`.
    pub fn new() -> (StreamHandler<H, Ctx>, StreamRegistration<H, Ctx>) {
        let (sender, receiver) = mpsc::channel();

        (
            StreamHandler {
                receiver,
                streams: HashMap::new(),
            },
            StreamRegistration {
                sender,
                next_stream_id: 0,
            },
        )
    }

    /// Poll all currently registered streams.
    ///
    /// This always returns `Poll::Pending` after polling all available streams and registering the
    /// waker.
    pub fn poll<'a>(&'a mut self, h: &'a mut H, ctx: &'a mut Ctx) -> impl Future<Output = ()> + 'a {
        // TODO: Instead of always polling all streams, provide a custom waker to each that when
        // woken up remembers that this stream needs re-polling.
        future::poll_fn(move |cx| {
            trace!("Polling");

            let receiver_closed = loop {
                match self.receiver.poll_next_unpin(cx) {
                    Poll::Ready(None) => {
                        trace!("Stream registration closed");
                        break true;
                    }
                    Poll::Ready(Some((stream_id, stream))) => {
                        trace!("Inserting new stream with id {}", stream_id);
                        self.streams.insert(stream_id, stream);
                    }
                    Poll::Pending => break false,
                }
            };

            let mut to_remove = Vec::new();
            for (stream_id, func) in &mut self.streams {
                trace!("Polling stream with id {}", stream_id);

                // Poll until not ready or finished
                loop {
                    match func(h, ctx, cx) {
                        Poll::Ready(None) => {
                            to_remove.push(*stream_id);
                            break;
                        }
                        Poll::Pending => {
                            break;
                        }
                        Poll::Ready(Some(_)) => continue,
                    }
                }
            }

            for stream_id in to_remove {
                trace!("Removing old stream with id {}", stream_id);
                self.streams.remove(&stream_id);
            }

            trace!("Polled");

            if receiver_closed && self.streams.is_empty() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }
}

/// Trait for handling specific kinds of messages.
pub trait MessageHandler<Msg, Token> {
    /// Context type passed into `handle_message`.
    type Context: ?Sized;

    /// Handle a message for a registered stream.
    fn handle_message(&mut self, ctx: &mut Self::Context, token: &Token, msg: Msg);

    /// Called when the registered stream is finished.
    fn finished(&mut self, _ctx: &mut Self::Context, _token: &Token) {}
}

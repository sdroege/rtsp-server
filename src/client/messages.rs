use std::pin::Pin;

use futures::prelude::*;

use crate::channel::{mpsc, oneshot};
use crate::media;
use crate::server;
use crate::typemap::TypeMap;

use super::context::Context;
use super::interleaved::{DataReceiver, DataSender};

/// Messages sent from the server to the client task
#[derive(Debug)]
pub(super) enum ServerMessage {
    Quit,
    TimedOut(server::SessionId),
    ReplacedClient(server::SessionId),
}

impl From<ServerMessage> for ControllerMessage {
    fn from(msg: ServerMessage) -> ControllerMessage {
        ControllerMessage::Server(msg)
    }
}

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum MediaMessage {
    RegisterInterleavedChannel {
        media_id: media::Id,
        session_id: server::SessionId,
        channel_id: Option<u8>,
        #[derivative(Debug = "ignore")]
        receivers: Vec<Box<dyn DataReceiver>>,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(u8, Vec<DataSender>, TypeMap), crate::error::Error>>,
    },
    PlayNotify(
        media::Id,
        server::SessionId,
        crate::media::PlayNotifyMessage,
    ),
    Error(media::Id),
    Finished(media::Id),
}

impl From<MediaMessage> for ControllerMessage {
    fn from(msg: MediaMessage) -> ControllerMessage {
        ControllerMessage::Media(msg)
    }
}

#[derive(Debug)]
pub(super) enum AppMessage {}

impl From<AppMessage> for ControllerMessage {
    fn from(msg: AppMessage) -> ControllerMessage {
        ControllerMessage::App(msg)
    }
}

#[derive(Debug)]
pub(super) enum ControllerMessage {
    Server(ServerMessage),
    Media(MediaMessage),
    App(AppMessage),
}

/// Messages handled by the client context task
#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum ClientMessage<C: super::Client + ?Sized> {
    Controller(ControllerMessage),
    ControllerClosed,
    InterleavedChannelClosed(u8),
    ClientFuture(
        #[derivative(Debug = "ignore")]
        Box<
            dyn FnOnce(&mut C, &mut Context<C>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send,
        >,
    ),
    ClientClosure(
        #[derivative(Debug = "ignore")] Box<dyn FnOnce(&mut C, &mut Context<C>) + Send + 'static>,
    ),
    RequestFinished(u32),
    ResponseFinished(u32),
    SenderError(std::io::Error),
    SenderFull,
}

/// Messages sent to the RTSP send task.
#[derive(Debug)]
pub(super) struct RtspSendMessage(
    pub(super) rtsp_types::Message<crate::body::Body>,
    // Using an mpsc sender so it's not require to allocate a new channel per message
    pub(super) Option<mpsc::Sender<()>>,
);

use std::pin::Pin;

use futures::prelude::*;

use crate::channel::oneshot;
use crate::client;
use crate::media;
use crate::typemap::TypeMap;

use super::context::Context;

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum ClientMessage {
    Options {
        client_id: client::Id,
        stream_id: Option<media::StreamId>,
        supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<
                (
                    rtsp_types::headers::Allow,
                    rtsp_types::headers::Supported,
                    rtsp_types::headers::Unsupported,
                    TypeMap,
                ),
                crate::error::Error,
            >,
        >,
    },
    Describe {
        client_id: client::Id,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(sdp_types::Session, TypeMap), crate::error::Error>>,
    },
    CreateMedia {
        client_id: client::Id,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<(media::Controller<media::controller::Client>, TypeMap), crate::error::Error>,
        >,
    },
}

impl From<ClientMessage> for ControllerMessage {
    fn from(msg: ClientMessage) -> ControllerMessage {
        ControllerMessage::Client(msg)
    }
}

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum MediaMessage {
    Idle {
        media_id: media::Id,
        idle: bool,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<TypeMap, crate::error::Error>>,
    },
    Error(media::Id),
    Finished(media::Id),
}

impl From<MediaMessage> for ControllerMessage {
    fn from(msg: MediaMessage) -> ControllerMessage {
        ControllerMessage::Media(msg)
    }
}

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum ServerMessage {
    Quit,
}

impl From<ServerMessage> for ControllerMessage {
    fn from(msg: ServerMessage) -> ControllerMessage {
        ControllerMessage::Server(msg)
    }
}

#[derive(Debug)]
pub(super) enum ControllerMessage {
    Client(ClientMessage),
    Media(MediaMessage),
    Server(ServerMessage),
}

/// Messages handled by the media factory context task
#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum MediaFactoryMessage<MF: super::MediaFactory + ?Sized> {
    Controller(ControllerMessage),
    ControllerClosed,
    CheckMediaTimeout(media::Id),
    MediaFactoryFuture(
        #[derivative(Debug = "ignore")]
        Box<
            dyn FnOnce(&mut MF, &mut Context<MF>) -> Pin<Box<dyn Future<Output = ()> + Send>>
                + Send,
        >,
    ),
    MediaFactoryClosure(
        #[derivative(Debug = "ignore")] Box<dyn FnOnce(&mut MF, &mut Context<MF>) + Send + 'static>,
    ),
}

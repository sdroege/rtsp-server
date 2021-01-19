use std::pin::Pin;

use futures::prelude::*;

use crate::channel::oneshot;
use crate::client;
use crate::media_factory;
use crate::server;
use crate::typemap::TypeMap;

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum ServerMessage {
    ClientChanged(
        server::SessionId,
        #[derivative(Debug = "ignore")] server::Controller<server::controller::Media>,
        #[derivative(Debug = "ignore")] Option<client::Controller<client::controller::Media>>,
    ),
    TimedOut(server::SessionId),
}

impl From<ServerMessage> for ControllerMessage {
    fn from(msg: ServerMessage) -> ControllerMessage {
        ControllerMessage::Server(msg)
    }
}

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum ClientMessage {
    FindMediaFactory {
        client_id: client::Id,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<
                media_factory::Controller<media_factory::controller::Client>,
                crate::error::Error,
            >,
        >,
    },
    Options {
        client_id: client::Id,
        supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<
                (
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
    AddTransport {
        client_id: client::Id,
        session_id: server::SessionId,
        stream_id: super::StreamId,
        transports: rtsp_types::headers::Transports,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<super::ConfiguredTransport, crate::error::Error>>,
    },
    RemoveTransport {
        client_id: client::Id,
        session_id: server::SessionId,
        stream_id: super::StreamId,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(), crate::error::Error>>,
    },
    ShutdownSession {
        client_id: client::Id,
        session_id: server::SessionId,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(), crate::error::Error>>,
    },
    Play {
        client_id: client::Id,
        session_id: server::SessionId,
        range: rtsp_types::headers::Range,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<
                (
                    rtsp_types::headers::Range,
                    rtsp_types::headers::RtpInfos,
                    TypeMap,
                ),
                crate::error::Error,
            >,
        >,
    },
    Pause {
        client_id: client::Id,
        session_id: server::SessionId,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(rtsp_types::headers::Range, TypeMap), crate::error::Error>>,
    },
}

impl From<ClientMessage> for ControllerMessage {
    fn from(msg: ClientMessage) -> ControllerMessage {
        ControllerMessage::Client(msg)
    }
}

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum MediaFactoryMessage {
    Options {
        media_factory_id: media_factory::Id,
        supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<
                (
                    rtsp_types::headers::Supported,
                    rtsp_types::headers::Unsupported,
                    TypeMap,
                ),
                crate::error::Error,
            >,
        >,
    },
    Describe {
        media_factory_id: media_factory::Id,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(sdp_types::Session, TypeMap), crate::error::Error>>,
    },
    Quit(media_factory::Id),
}

impl From<MediaFactoryMessage> for ControllerMessage {
    fn from(msg: MediaFactoryMessage) -> ControllerMessage {
        ControllerMessage::MediaFactory(msg)
    }
}

#[derive(Debug)]
pub(super) enum ControllerMessage {
    Server(ServerMessage),
    Client(ClientMessage),
    MediaFactory(MediaFactoryMessage),
}

#[derive(derivative::Derivative)]
#[derivative(Debug)]
/// Messages handled by the media context task
pub(super) enum MediaMessage<M: super::Media + ?Sized> {
    Controller(ControllerMessage),
    ControllerClosed,
    MediaFuture(
        #[derivative(Debug = "ignore")]
        Box<
            dyn FnOnce(&mut M, &mut super::Context<M>) -> Pin<Box<dyn Future<Output = ()> + Send>>
                + Send,
        >,
    ),
    MediaClosure(
        #[derivative(Debug = "ignore")]
        Box<dyn FnOnce(&mut M, &mut super::Context<M>) + Send + 'static>,
    ),
}

#[derive(Debug, Clone)]
pub enum PlayNotifyMessage {
    EndOfStream {
        range: rtsp_types::headers::Range,
        rtp_info: rtsp_types::headers::RtpInfos,
        extra_data: TypeMap,
    },
}

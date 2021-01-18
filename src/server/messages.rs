use crate::channel::oneshot;
use crate::client;
use crate::listener;
use crate::media;
use crate::media_factory;
use crate::typemap::TypeMap;

use super::session;

/// Messages sent from the app to the server
#[derive(Debug)]
pub(super) enum AppMessage {
    Quit,
}

impl From<AppMessage> for ControllerMessage {
    fn from(msg: AppMessage) -> ControllerMessage {
        ControllerMessage::App(msg)
    }
}

/// Messages sent from the listeners to the server
#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum ListenerMessage {
    NewConnection(listener::Id, listener::IncomingConnection),
    Finished(listener::Id),
    Error(listener::Id, std::io::Error),
}

impl From<ListenerMessage> for ControllerMessage {
    fn from(msg: ListenerMessage) -> ControllerMessage {
        ControllerMessage::Listener(msg)
    }
}

/// Messages sent from the clients to the server
#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum ClientMessage {
    FindMediaFactoryForUri {
        client_id: client::Id,
        uri: url::Url,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<
                media_factory::Controller<media_factory::controller::Client>,
                crate::error::Error,
            >,
        >,
    },
    /// Create a new session.
    CreateSession {
        client_id: client::Id,
        presentation_uri: url::Url,
        #[derivative(Debug = "ignore")]
        media: media::Controller<media::controller::Server>,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(), crate::error::Error>>,
    },
    /// Get an existing session media with a given id.
    FindSessionMedia {
        client_id: client::Id,
        session_id: session::Id,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<media::Controller<media::controller::Client>, crate::error::Error>,
        >,
    },
    KeepAliveSession {
        client_id: client::Id,
        session_id: session::Id,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(), crate::error::Error>>,
    },
    ShutdownSession {
        client_id: client::Id,
        session_id: session::Id,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(), crate::error::Error>>,
    },
    Error(client::Id),
    Finished(client::Id),
}

impl From<ClientMessage> for ControllerMessage {
    fn from(msg: ClientMessage) -> ControllerMessage {
        ControllerMessage::Client(msg)
    }
}

/// Messages sent from the medias to the server
#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum MediaMessage {
    KeepAliveSession {
        media_id: media::Id,
        session_id: session::Id,
        #[derivative(Debug = "ignore")]
        ret: Option<oneshot::Sender<Result<(), crate::error::Error>>>,
    },
    Finished(media::Id, session::Id),
    Error(media::Id, session::Id),
}

impl From<MediaMessage> for ControllerMessage {
    fn from(msg: MediaMessage) -> ControllerMessage {
        ControllerMessage::Media(msg)
    }
}

/// Messages sent from the medias to the server
#[derive(Debug)]
pub(super) enum MediaFactoryMessage {
    Error(media_factory::Id),
    Finished(media_factory::Id),
}

impl From<MediaFactoryMessage> for ControllerMessage {
    fn from(msg: MediaFactoryMessage) -> ControllerMessage {
        ControllerMessage::MediaFactory(msg)
    }
}

#[derive(Debug)]
pub(super) enum ControllerMessage {
    App(AppMessage),
    Listener(ListenerMessage),
    Client(ClientMessage),
    Media(MediaMessage),
    MediaFactory(MediaFactoryMessage),
}

/// Messages sent to the server task
#[derive(Debug)]
pub(super) enum ServerMessage {
    Controller(ControllerMessage),
    ControllerClosed,
    CheckSessionTimeout(session::Id),
}

impl From<ControllerMessage> for ServerMessage {
    fn from(msg: ControllerMessage) -> ServerMessage {
        ServerMessage::Controller(msg)
    }
}

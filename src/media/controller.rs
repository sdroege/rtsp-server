use std::sync::Arc;

use futures::lock::Mutex;
use futures::prelude::*;

use async_std::task;

use crate::channel::{mpsc, oneshot};
use crate::client;
use crate::media_factory;
use crate::server;
use crate::typemap::TypeMap;

use super::messages::*;
use super::Id;

#[derive(Clone)]
pub struct Controller<T> {
    id: Id,
    sender: mpsc::Sender<ControllerMessage>,
    context: T,
}

impl<T> Controller<T> {
    pub fn media_id(&self) -> Id {
        self.id
    }
}

#[derive(Clone)]
pub struct Server {
    session_id: server::SessionId,
}

impl Controller<Server> {
    pub(crate) fn from_client_controller(
        controller: &Controller<Client>,
        session_id: server::SessionId,
    ) -> Self {
        Controller {
            id: controller.id,
            sender: controller.sender.clone(),
            context: Server { session_id },
        }
    }

    pub fn session_id(&self) -> server::SessionId {
        self.context.session_id.clone()
    }

    pub async fn timed_out(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(ServerMessage::TimedOut(self.context.session_id.clone()).into())
            .await
    }

    pub async fn client_changed(
        &mut self,
        server_controller: server::Controller<server::controller::Media>,
        client_controller: Option<client::Controller<client::controller::Media>>,
    ) -> Result<(), mpsc::SendError> {
        self.sender
            .send(
                ServerMessage::ClientChanged(
                    self.context.session_id.clone(),
                    server_controller,
                    client_controller,
                )
                .into(),
            )
            .await
    }
}

#[derive(Clone)]
pub struct Client {
    id: client::Id,
}

impl Controller<Client> {
    pub(crate) fn from_server_controller(
        controller: &Controller<Server>,
        client_id: client::Id,
    ) -> Self {
        Controller {
            id: controller.id,
            sender: controller.sender.clone(),
            context: Client { id: client_id },
        }
    }

    pub(crate) fn from_media_factory_controller(
        controller: &Controller<MediaFactory>,
        client_id: client::Id,
    ) -> Self {
        Controller {
            id: controller.id,
            sender: controller.sender.clone(),
            context: Client { id: client_id },
        }
    }
}

impl Controller<Client> {
    pub fn client_id(&self) -> client::Id {
        self.context.id.clone()
    }

    pub async fn options(
        &mut self,
        supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        extra_data: TypeMap,
    ) -> Result<
        (
            rtsp_types::headers::Supported,
            rtsp_types::headers::Unsupported,
            TypeMap,
        ),
        crate::error::Error,
    > {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::Options {
                    client_id: self.context.id,
                    supported,
                    require,
                    extra_data,
                    ret: sender,
                }
                .into(),
            )
            .await
        {
            return Err(crate::error::InternalServerError.into());
        }

        receiver
            .await
            .map_err(|_| crate::error::InternalServerError)?
    }

    pub async fn describe(
        &mut self,
        extra_data: TypeMap,
    ) -> Result<(sdp_types::Session, TypeMap), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::Describe {
                    client_id: self.context.id,
                    extra_data,
                    ret: sender,
                }
                .into(),
            )
            .await
        {
            return Err(crate::error::InternalServerError.into());
        }

        receiver
            .await
            .map_err(|_| crate::error::InternalServerError)?
    }

    pub async fn add_transport(
        &mut self,
        session_id: server::SessionId,
        stream_id: super::StreamId,
        transports: rtsp_types::headers::Transports,
        extra_data: TypeMap,
    ) -> Result<super::ConfiguredTransport, crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::AddTransport {
                    client_id: self.context.id,
                    session_id,
                    stream_id,
                    transports,
                    extra_data,
                    ret: sender,
                }
                .into(),
            )
            .await
        {
            return Err(crate::error::InternalServerError.into());
        }

        receiver
            .await
            .map_err(|_| crate::error::InternalServerError)?
    }

    pub async fn remove_transport(
        &mut self,
        session_id: server::SessionId,
        stream_id: super::StreamId,
        extra_data: TypeMap,
    ) -> Result<(), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::RemoveTransport {
                    client_id: self.context.id,
                    session_id,
                    stream_id,
                    extra_data,
                    ret: sender,
                }
                .into(),
            )
            .await
        {
            return Err(crate::error::InternalServerError.into());
        }

        receiver
            .await
            .map_err(|_| crate::error::InternalServerError)?
    }

    pub async fn shutdown_session(
        &mut self,
        session_id: server::SessionId,
        extra_data: TypeMap,
    ) -> Result<(), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::ShutdownSession {
                    client_id: self.context.id,
                    session_id,
                    extra_data,
                    ret: sender,
                }
                .into(),
            )
            .await
        {
            return Err(crate::error::InternalServerError.into());
        }

        receiver
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))?
    }

    pub async fn play(
        &mut self,
        session_id: server::SessionId,
        range: rtsp_types::headers::Range,
        extra_data: TypeMap,
    ) -> Result<
        (
            rtsp_types::headers::Range,
            rtsp_types::headers::RtpInfos,
            TypeMap,
        ),
        crate::error::Error,
    > {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::Play {
                    client_id: self.context.id,
                    session_id,
                    range,
                    extra_data,
                    ret: sender,
                }
                .into(),
            )
            .await
        {
            return Err(crate::error::InternalServerError.into());
        }

        receiver
            .await
            .map_err(|_| crate::error::InternalServerError)?
    }

    pub async fn pause(
        &mut self,
        session_id: server::SessionId,
        extra_data: TypeMap,
    ) -> Result<(rtsp_types::headers::Range, TypeMap), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::Pause {
                    client_id: self.context.id,
                    session_id,
                    extra_data,
                    ret: sender,
                }
                .into(),
            )
            .await
        {
            return Err(crate::error::InternalServerError.into());
        }

        receiver
            .await
            .map_err(|_| crate::error::InternalServerError)?
    }
}

#[derive(Clone)]
pub struct MediaFactory(Arc<MediaFactoryInner>);

struct MediaFactoryInner {
    id: media_factory::Id,
    join_handle: Mutex<Option<task::JoinHandle<()>>>,
    sender: mpsc::Sender<ControllerMessage>,
}

impl Drop for MediaFactoryInner {
    fn drop(&mut self) {
        // Close channel once the media factory has no reference left
        self.sender.close_channel();
    }
}

impl Controller<MediaFactory> {
    pub(super) fn new(
        id: Id,
        media_factory_id: media_factory::Id,
        join_handle: task::JoinHandle<()>,
        sender: mpsc::Sender<ControllerMessage>,
    ) -> Self {
        Controller {
            id,
            sender: sender.clone(),
            context: MediaFactory(Arc::new(MediaFactoryInner {
                id: media_factory_id,
                join_handle: Mutex::new(Some(join_handle)),
                sender,
            })),
        }
    }

    pub fn media_factory_id(&self) -> media_factory::Id {
        self.context.0.id.clone()
    }

    pub async fn options(
        &mut self,

        supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        extra_data: TypeMap,
    ) -> Result<
        (
            rtsp_types::headers::Supported,
            rtsp_types::headers::Unsupported,
            TypeMap,
        ),
        crate::error::Error,
    > {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                MediaFactoryMessage::Options {
                    media_factory_id: self.context.0.id,
                    supported,
                    require,
                    extra_data,
                    ret: sender,
                }
                .into(),
            )
            .await
        {
            return Err(crate::error::InternalServerError.into());
        }

        receiver
            .await
            .map_err(|_| crate::error::InternalServerError)?
    }

    pub async fn describe(
        &mut self,
        extra_data: TypeMap,
    ) -> Result<(sdp_types::Session, TypeMap), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                MediaFactoryMessage::Describe {
                    media_factory_id: self.context.0.id,
                    extra_data,
                    ret: sender,
                }
                .into(),
            )
            .await
        {
            return Err(crate::error::InternalServerError.into());
        }

        receiver
            .await
            .map_err(|_| crate::error::InternalServerError)?
    }

    pub async fn shutdown(mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(MediaFactoryMessage::Quit(self.context.0.id).into())
            .await?;

        if let Some(join_handle) = self.context.0.join_handle.lock().await.take() {
            join_handle.await;
        }

        Ok(())
    }
}

use std::sync::Arc;

use futures::lock::Mutex;
use futures::prelude::*;

use async_std::task;

use log::error;

use crate::channel::{mpsc, oneshot};
use crate::client;
use crate::media;
use crate::typemap::TypeMap;
use crate::utils::RunOnDrop;

use super::messages::*;
use super::Id;

#[derive(Clone)]
pub struct Controller<T> {
    id: Id,
    sender: mpsc::Sender<ControllerMessage>,
    context: T,
}

impl<T> Controller<T> {
    pub fn media_factory_id(&self) -> Id {
        self.id
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

    pub(crate) fn from_media_controller(
        controller: &Controller<Media>,
        client_id: client::Id,
    ) -> Self {
        Controller {
            id: controller.id,
            sender: controller.sender.clone(),
            context: Client { id: client_id },
        }
    }

    pub fn client_id(&self) -> client::Id {
        self.context.id
    }

    pub async fn options(
        &mut self,
        uri: url::Url,
        supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        extra_data: TypeMap,
    ) -> Result<
        (
            rtsp_types::headers::Allow,
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
                    uri,
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
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))?
    }

    pub async fn describe(
        &mut self,
        uri: url::Url,
        extra_data: TypeMap,
    ) -> Result<(sdp_types::Session, TypeMap), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::Describe {
                    client_id: self.context.id,
                    uri,
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

    pub async fn find_presentation_uri(
        &mut self,
        uri: url::Url,
        extra_data: TypeMap,
    ) -> Result<(url::Url, Option<media::StreamId>, TypeMap), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::FindPresentationURI {
                    client_id: self.context.id,
                    uri,
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

    pub async fn create_media(
        &mut self,
        uri: url::Url,
        extra_data: TypeMap,
    ) -> Result<(media::Controller<media::controller::Client>, TypeMap), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::CreateMedia {
                    client_id: self.context.id,
                    uri,
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
}

#[derive(Clone)]
pub struct Media {
    id: media::Id,
}

impl Controller<Media> {
    pub(super) fn new(
        id: Id,
        media_id: media::Id,
        sender: mpsc::Sender<ControllerMessage>,
    ) -> Self {
        Controller {
            id,
            sender: sender.clone(),
            context: Media { id: media_id },
        }
    }

    pub fn media_id(&self) -> media::Id {
        self.context.id
    }

    pub async fn idle(
        &mut self,
        idle: bool,
        extra_data: TypeMap,
    ) -> Result<TypeMap, crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                MediaMessage::Idle {
                    media_id: self.context.id,
                    idle,
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

    pub async fn error(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(MediaMessage::Error(self.context.id).into())
            .await
    }

    pub(crate) async fn finished(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(MediaMessage::Finished(self.context.id).into())
            .await
    }

    pub(crate) fn finish_on_drop(&self) -> RunOnDrop {
        let mut sender = self.sender.clone();
        let id = self.context.id;
        RunOnDrop::new(move || {
            if let Err(err) = sender.try_send(MediaMessage::Finished(id).into()) {
                if err.is_full() {
                    error!("Can't finish controller on drop");
                }
            }
        })
    }
}

#[derive(Clone)]
pub struct Server(Arc<ServerInner>);

struct ServerInner {
    join_handle: Mutex<Option<task::JoinHandle<()>>>,
    sender: mpsc::Sender<ControllerMessage>,
}

impl Drop for ServerInner {
    fn drop(&mut self) {
        // Close the channel once the server has no reference to the factory anymore
        self.sender.close_channel();
    }
}

impl Controller<Server> {
    pub(super) fn new(
        id: Id,
        join_handle: task::JoinHandle<()>,
        sender: mpsc::Sender<ControllerMessage>,
    ) -> Self {
        Controller {
            id,
            sender: sender.clone(),
            context: Server(Arc::new(ServerInner {
                join_handle: Mutex::new(Some(join_handle)),
                sender,
            })),
        }
    }

    pub async fn shutdown(mut self) -> Result<(), mpsc::SendError> {
        self.sender.send(ServerMessage::Quit.into()).await?;

        if let Some(join_handle) = self.context.0.join_handle.lock().await.take() {
            join_handle.await;
        }

        Ok(())
    }
}

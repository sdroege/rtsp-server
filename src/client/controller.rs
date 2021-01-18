use std::sync::Arc;

use futures::lock::Mutex;
use futures::prelude::*;

use async_std::task;

use crate::channel::{mpsc, oneshot};
use crate::media;
use crate::server;
use crate::typemap::TypeMap;

use super::interleaved::{DataReceiver, DataSender};
use super::messages::*;
use super::Id;

#[derive(Clone)]
pub struct Controller<T> {
    id: Id,
    sender: mpsc::Sender<ControllerMessage>,
    context: T,
}

impl<T> Controller<T> {
    pub fn client_id(&self) -> Id {
        self.id
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
        // Close the controller channel once the server has no reference to the client anymore
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

    pub async fn timed_out(
        &mut self,
        session_id: server::SessionId,
    ) -> Result<(), mpsc::SendError> {
        self.sender
            .send(ServerMessage::TimedOut(session_id).into())
            .await
    }

    pub async fn replaced_client(
        &mut self,
        session_id: server::SessionId,
    ) -> Result<(), mpsc::SendError> {
        self.sender
            .send(ServerMessage::ReplacedClient(session_id).into())
            .await
    }
}

#[derive(Clone)]
pub struct Media {
    id: media::Id,
}

impl Controller<Media> {
    pub(crate) fn from_server_controller(
        controller: &Controller<Server>,
        media_id: media::Id,
    ) -> Self {
        Controller {
            id: controller.id,
            sender: controller.sender.clone(),
            context: Media { id: media_id },
        }
    }

    pub fn media_id(&self) -> media::Id {
        self.context.id.clone()
    }

    pub async fn error(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(MediaMessage::Error(self.context.id).into())
            .await
    }

    pub async fn register_interleaved_channel<D: DataReceiver + 'static>(
        &mut self,
        session_id: server::SessionId,
        channel_id: Option<u8>,
        receivers: Vec<D>,
        extra_data: TypeMap,
    ) -> Result<(u8, Vec<DataSender>, TypeMap), crate::error::Error> {
        let receivers = receivers
            .into_iter()
            .map(|r| {
                let r: Box<dyn DataReceiver + 'static> = Box::new(r);
                r
            })
            .collect();

        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                MediaMessage::RegisterInterleavedChannel {
                    media_id: self.context.id,
                    session_id,
                    channel_id,
                    receivers,
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

    pub async fn play_notify(
        &mut self,
        session_id: server::SessionId,
        notify: crate::media::PlayNotifyMessage,
    ) -> Result<(), mpsc::SendError> {
        self.sender
            .send(MediaMessage::PlayNotify(self.context.id, session_id, notify).into())
            .await
    }

    pub(crate) async fn finished(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(MediaMessage::Finished(self.context.id.clone()).into())
            .await
    }
}

#[derive(Clone)]
pub enum App {}
impl Controller<App> {
    // TODO shutdown etc
}

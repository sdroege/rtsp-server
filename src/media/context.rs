use std::collections::HashMap;
use std::pin::Pin;

use futures::prelude::*;

use async_std::task;

use log::{error, info, trace, warn};

use crate::channel::{mpsc, oneshot};
use crate::client;
use crate::media_factory;
use crate::server;
use crate::stream_handler;
use crate::typemap::TypeMap;

use super::messages::*;
use super::{Id, Media};

pub struct Handle<M: Media + ?Sized> {
    id: Id,
    sender: mpsc::Sender<MediaMessage<M>>,
}

impl<M: Media + ?Sized> Clone for Handle<M> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            id: self.id,
        }
    }
}

pub struct ClientHandle<M: Media + ?Sized> {
    pub(super) controller: client::Controller<client::controller::Media>,
    pub(super) handle: Handle<M>,
}

impl<M: Media + ?Sized> Clone for ClientHandle<M> {
    fn clone(&self) -> Self {
        ClientHandle {
            controller: self.controller.clone(),
            handle: self.handle.clone(),
        }
    }
}

impl<M: Media + ?Sized> ClientHandle<M> {
    pub fn client_id(&self) -> client::Id {
        self.controller.client_id()
    }

    pub async fn register_interleaved_channel<D: client::DataReceiver + 'static>(
        &mut self,
        session_id: server::SessionId,
        channel_id: Option<u8>,
        receivers: Vec<D>,
        extra_data: TypeMap,
    ) -> Result<(u8, Vec<client::DataSender>, TypeMap), crate::error::Error> {
        trace!(
            "Media {}: Registering {} interleaved channels {:?} for session {} with client {}",
            self.controller.media_id(),
            receivers.len(),
            channel_id,
            session_id,
            self.client_id()
        );

        let mut controller = self.controller.clone();

        self.handle
            .spawn(move |_factory, ctx| {
                let found = ctx.sessions.contains_key(&session_id);

                async move {
                    if found {
                        let res = controller
                            .register_interleaved_channel(
                                session_id.clone(), channel_id, receivers, extra_data,
                            )
                            .await;

                        trace!("Media {}: Registered interleaved channels for session {} with client {}: {:?}", controller.media_id(), session_id, controller.client_id(), res);

                        res
                    } else {
                        trace!("Media {}: Session {} not found", controller.media_id(), session_id);
                        Err(crate::error::Error::from(crate::error::ErrorStatus::from(
                            rtsp_types::StatusCode::NotFound,
                        )))
                    }
                }
            })
            .await
            .map_err(|_| crate::error::InternalServerError)?
    }

    pub async fn play_notify(
        &mut self,
        session_id: server::SessionId,
        notify: crate::media::PlayNotifyMessage,
    ) -> Result<(), crate::error::Error> {
        trace!(
            "Media {}: Play notify {:?} for session {} with client {}",
            self.controller.media_id(),
            notify,
            session_id,
            self.client_id()
        );

        let mut controller = self.controller.clone();

        self.handle
            .spawn(move |_factory, ctx| {
                let found = ctx.sessions.contains_key(&session_id);

                async move {
                    if found {
                        controller
                            .play_notify(session_id, notify)
                            .await
                            .map_err(|_| {
                                crate::error::Error::from(crate::error::InternalServerError)
                            })
                    } else {
                        trace!(
                            "Media {}: Session {} not found",
                            controller.media_id(),
                            session_id
                        );
                        Err(crate::error::Error::from(crate::error::ErrorStatus::from(
                            rtsp_types::StatusCode::NotFound,
                        )))
                    }
                }
            })
            .await
            .map_err(|_| crate::error::InternalServerError)?
    }
}

pub struct Context<M: Media + ?Sized> {
    pub(super) id: Id,

    /// Sender for sending internal messages to this media's task
    pub(super) media_sender: mpsc::Sender<MediaMessage<M>>,
    pub(super) controller_sender: mpsc::Sender<ControllerMessage>,

    /// Registration for custom media streams
    pub(super) stream_registration: stream_handler::StreamRegistration<M, Self>,

    /// Controller for the media factory that spawned this media.
    pub(super) media_factory_controller:
        media_factory::Controller<media_factory::controller::Media>,

    /// All currently active sessions
    pub(super) sessions: HashMap<
        server::SessionId,
        (
            Option<client::Id>,
            server::Controller<server::controller::Media>,
        ),
    >,
    /// Clients that are participanting in a session.
    pub(super) session_clients: HashMap<client::Id, client::Controller<client::controller::Media>>,

    /// Current idle status
    pub(super) idle: bool,
}

#[derive(Clone)]
pub struct KeepAliveHandle(server::Controller<server::controller::Media>);

impl KeepAliveHandle {
    pub fn keep_alive(&mut self) {
        let _ = self.0.keep_alive_session_non_async();
    }
}

impl<M: Media + ?Sized> Context<M> {
    /// Returns this media's id.
    pub fn id(&self) -> Id {
        self.id
    }

    pub fn handle(&self) -> Handle<M> {
        Handle {
            sender: self.media_sender.clone(),
            id: self.id,
        }
    }

    pub fn find_session_client(&self, session_id: &server::SessionId) -> Option<ClientHandle<M>> {
        if let Some((client_id, _)) = self.sessions.get(session_id) {
            if let Some(client_id) = client_id {
                trace!(
                    "Media {}: Found session {} client {}",
                    self.id,
                    session_id,
                    client_id
                );
                self.session_clients.get(&client_id).map(|c| ClientHandle {
                    handle: self.handle(),
                    controller: c.clone(),
                })
            } else {
                trace!(
                    "Media {}: Found session {} without client",
                    self.id,
                    session_id
                );
                None
            }
        } else {
            trace!("Media {}: Did not find session {}", self.id, session_id);
            None
        }
    }

    pub fn session_keep_alive_handle(
        &self,
        session_id: &server::SessionId,
    ) -> Option<KeepAliveHandle> {
        if let Some((_, server_controller)) = self.sessions.get(session_id) {
            trace!("Media {}: Found session {}", self.id, session_id);
            Some(KeepAliveHandle(server_controller.clone()))
        } else {
            trace!("Media {}: Did not find session {}", self.id, session_id);
            None
        }
    }

    pub fn session_keep_alive(
        &self,
        session_id: &server::SessionId,
    ) -> impl Future<Output = Result<(), crate::error::Error>> + Send {
        use future::Either;

        let session_id = session_id.clone();
        if let Some((_, server_controller)) = self.sessions.get(&session_id) {
            trace!("Media {}: Found session {}", self.id, session_id);
            let id = self.id;
            let mut server_controller = server_controller.clone();
            Either::Right(async move {
                trace!("Media {}: Keeping alive session {}", id, session_id);
                server_controller.keep_alive_session().await
            })
        } else {
            trace!("Media {}: Did not find session {}", self.id, session_id);
            Either::Left(async move {
                Err(crate::error::Error::from(crate::error::ErrorStatus::from(
                    rtsp_types::StatusCode::SessionNotFound,
                )))
            })
        }
    }

    pub fn idle(&mut self, idle: bool, extra_data: TypeMap) {
        assert!(!M::AUTOMATIC_IDLE);

        if self.idle == idle {
            return;
        }
        trace!("Media {}: Idle {}", self.id, idle);
        self.idle = idle;

        let mut media_factory_controller = self.media_factory_controller.clone();
        task::spawn(async move {
            let _ = media_factory_controller.idle(idle, extra_data).await;
        });
    }

    pub fn is_idle(&self) -> bool {
        self.idle
    }

    /// Signals to the media factory or client that this media had a fatal error.
    ///
    /// This shuts down the media and all currently active sessions.
    pub fn error(&mut self) {
        error!("Media {}: Errored", self.id);
        let mut media_factory_controller = self.media_factory_controller.clone();

        let mut server_controllers = Vec::new();
        let mut client_controllers = Vec::new();

        for (_, (client_id, server_controller)) in &self.sessions {
            server_controllers.push(server_controller.clone());

            if let Some(client_id) = client_id {
                if let Some(client_controller) = self.session_clients.get(client_id) {
                    client_controllers.push(client_controller.clone());
                }
            }
        }

        let mut controller_sender = self.controller_sender.clone();

        task::spawn(async move {
            for mut client_controller in client_controllers {
                let _ = client_controller.error().await;
            }

            for mut server_controller in server_controllers {
                let _ = server_controller.error().await;
            }

            let _ = media_factory_controller.error().await;

            controller_sender.close_channel();
        });
    }

    /// Shuts down this media.
    pub fn shutdown(&mut self) {
        info!("Media {}: Shutting down", self.id);
        self.controller_sender.close_channel();
    }

    pub fn register_stream<
        Msg,
        Token: Clone + Send + 'static,
        S: Stream<Item = Msg> + Unpin + Send + 'static,
    >(
        &mut self,
        token: Token,
        stream: S,
    ) -> Result<(), mpsc::SendError>
    where
        M: stream_handler::MessageHandler<Msg, Token, Context = Self>,
    {
        self.stream_registration.register(token, stream)
    }

    pub(super) fn is_client_in_session(
        &self,
        client_id: client::Id,
        session_id: &server::SessionId,
    ) -> Result<(), crate::error::Error> {
        if let Some((stored_client_id, _)) = self.sessions.get(session_id) {
            if stored_client_id == &Some(client_id) {
                Ok(())
            } else {
                warn!(
                    "Media {}: Client {} is not in session {}",
                    self.id, client_id, session_id
                );
                Err(crate::error::InternalServerError.into())
            }
        } else {
            info!(
                "Media {}: Client {} asked for session {} but session not found",
                self.id, client_id, session_id
            );
            Err(crate::error::ErrorStatus::from(rtsp_types::StatusCode::SessionNotFound).into())
        }
    }
}

impl<M: Media + ?Sized> Handle<M> {
    /// Returns this media's id.
    pub fn id(&self) -> Id {
        self.id
    }

    pub async fn find_session_client(
        &mut self,
        session_id: &server::SessionId,
    ) -> Option<ClientHandle<M>> {
        let session_id = session_id.clone();
        self.run(move |_, ctx| ctx.find_session_client(&session_id))
            .await
            .ok()
            .flatten()
    }

    pub async fn session_keep_alive_handle(
        &mut self,
        session_id: &server::SessionId,
    ) -> Option<KeepAliveHandle> {
        let session_id = session_id.clone();
        self.run(move |_, ctx| ctx.session_keep_alive_handle(&session_id))
            .await
            .ok()
            .flatten()
    }

    pub async fn session_keep_alive(
        &mut self,
        session_id: &server::SessionId,
    ) -> Result<(), crate::error::Error> {
        let session_id = session_id.clone();
        self.spawn(move |_, ctx| ctx.session_keep_alive(&session_id))
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))?
    }

    pub async fn idle(
        &mut self,
        idle: bool,
        extra_data: TypeMap,
    ) -> Result<(), crate::error::Error> {
        assert!(!M::AUTOMATIC_IDLE);

        self.run(move |_, ctx| ctx.idle(idle, extra_data))
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))
    }

    pub async fn is_idle(&mut self) -> bool {
        self.run(move |_, ctx| ctx.is_idle()).await.unwrap_or(true)
    }

    /// Signals to the media factory or client that this media had a fatal error.
    ///
    /// This shuts down the media and all currently active sessions.
    pub async fn error(&mut self) -> Result<(), crate::error::Error> {
        self.run(move |_, ctx| ctx.error())
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))
    }

    /// Shuts down this media.
    pub async fn shutdown(&mut self) -> Result<(), crate::error::Error> {
        self.run(move |_, ctx| ctx.shutdown())
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))
    }

    /// Spawn a new future to be executed on the media's task.
    pub fn spawn<
        Fut,
        T: Send + 'static,
        F: FnOnce(&mut M, &mut Context<M>) -> Fut + Send + 'static,
    >(
        &mut self,
        func: F,
    ) -> impl Future<Output = Result<T, mpsc::SendError>> + Send
    where
        Fut: Future<Output = T> + Send + 'static,
    {
        use future::Either;

        let (sender, receiver) = oneshot::channel();

        trace!("Media {}: Spawning media future", self.id);
        let func: Box<
            dyn FnOnce(&mut M, &mut Context<M>) -> Pin<Box<dyn Future<Output = ()> + Send>>
                + Send
                + 'static,
        > = Box::new(move |client, context| {
            let fut = func(client, context);
            let fut: Pin<Box<dyn Future<Output = ()> + Send + 'static>> = Box::pin(async move {
                let res = fut.await;
                let _ = sender.send(res);
            });
            fut
        });

        if let Err(err) = self.sender.try_send(MediaMessage::MediaFuture(func)) {
            warn!("Media {}: Failed to spawn future", self.id);
            Either::Left(async move { Err(err) })
        } else {
            Either::Right(async move { receiver.await.map_err(|_| mpsc::SendError::Disconnected) })
        }
    }

    /// Run a closure on the media's task.
    pub fn run<T: Send + 'static, F: FnOnce(&mut M, &mut Context<M>) -> T + Send + 'static>(
        &mut self,
        func: F,
    ) -> impl Future<Output = Result<T, mpsc::SendError>> + Send {
        use future::Either;

        let (sender, receiver) = oneshot::channel();

        trace!("Media {}: Running media closure", self.id);
        let func: Box<dyn FnOnce(&mut M, &mut Context<M>) + Send + 'static> =
            Box::new(move |media, context| {
                let res = func(media, context);
                let _ = sender.send(res);
            });

        if let Err(err) = self.sender.try_send(MediaMessage::MediaClosure(func)) {
            warn!("Media {}: Failed to media closure", self.id);
            Either::Left(async move { Err(err) })
        } else {
            Either::Right(async move { receiver.await.map_err(|_| mpsc::SendError::Disconnected) })
        }
    }
}

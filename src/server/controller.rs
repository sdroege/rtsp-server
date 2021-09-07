// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use futures::lock::Mutex;
use futures::prelude::*;

use async_std::task;

use log::{debug, error, warn};

use crate::channel::{mpsc, oneshot};
use crate::client;
use crate::listener;
use crate::media;
use crate::media_factory;
use crate::typemap::TypeMap;
use crate::utils::RunOnDrop;

use super::context;
use super::messages::*;
use super::session;

/// Server handle.
#[derive(Clone)]
pub struct Server(Controller<App>);

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Server")
    }
}

impl Server {
    pub fn builder<
        C: client::Client,
        F: FnMut(client::Id, &listener::ConnectionInformation) -> Option<C> + Send + 'static,
    >(
        mut factory_fn: F,
    ) -> Builder {
        Builder {
            factory_fn: Box::new(move |controller, incoming_connection| {
                let client =
                    factory_fn(controller.client_id(), &incoming_connection.connection_info)?;
                Some(client::spawn(controller, incoming_connection, client))
            }),
            listeners: Vec::new(),
            mounts: None,
        }
    }

    // TODO: get clients, sessions

    pub async fn shutdown(mut self) {
        debug!("Shutting down server");

        if let Err(err) = self.0.sender.send(AppMessage::Quit.into()).await {
            warn!("Server task can't be shut down: {}", err);
            return;
        }

        if let Some(join_handle) = self.0.context.0.join_handle.lock().await.take() {
            join_handle.await;
        }
    }
}

#[derive(Clone)]
pub struct Controller<T> {
    sender: mpsc::Sender<ControllerMessage>,
    context: T,
}

#[derive(Clone)]
pub struct App(Arc<AppInner>);

struct AppInner {
    join_handle: Mutex<Option<task::JoinHandle<()>>>,
    sender: mpsc::Sender<ControllerMessage>,
}

impl Drop for AppInner {
    fn drop(&mut self) {
        // Close the channel once the last app reference is gone
        self.sender.close_channel();
    }
}

impl Controller<App> {
    pub(super) fn new(
        join_handle: task::JoinHandle<()>,
        sender: mpsc::Sender<ControllerMessage>,
    ) -> Self {
        Controller {
            sender: sender.clone(),
            context: App(Arc::new(AppInner {
                join_handle: Mutex::new(Some(join_handle)),
                sender,
            })),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Listener {
    id: listener::Id,
}

impl Controller<Listener> {
    pub(super) fn new(id: listener::Id, sender: mpsc::Sender<ControllerMessage>) -> Self {
        Controller {
            sender,
            context: Listener { id },
        }
    }

    pub fn listener_id(&self) -> listener::Id {
        self.context.id.clone()
    }

    pub async fn new_connection(
        &mut self,
        connection: listener::IncomingConnection,
    ) -> Result<(), mpsc::SendError> {
        self.sender
            .send(ListenerMessage::NewConnection(self.context.id.clone(), connection).into())
            .await
    }

    pub(crate) async fn finished(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(ListenerMessage::Finished(self.context.id.clone()).into())
            .await
    }

    pub(crate) async fn error(&mut self, err: std::io::Error) -> Result<(), mpsc::SendError> {
        self.sender
            .send(ListenerMessage::Error(self.context.id.clone(), err).into())
            .await
    }

    pub fn finish_on_drop(&self) -> RunOnDrop {
        let mut sender = self.sender.clone();
        let id = self.context.id.clone();
        RunOnDrop::new(move || {
            if let Err(err) = sender.try_send(ListenerMessage::Finished(id).into()) {
                if err.is_full() {
                    error!("Can't finish controller on drop");
                }
            }
        })
    }
}

#[derive(Clone)]
pub struct Client {
    id: client::Id,
}

impl Controller<Client> {
    pub(super) fn new(id: client::Id, sender: mpsc::Sender<ControllerMessage>) -> Self {
        Controller {
            sender,
            context: Client { id },
        }
    }

    pub fn client_id(&self) -> client::Id {
        self.context.id
    }

    pub async fn find_media_factory_for_uri(
        &mut self,
        uri: url::Url,
        extra_data: TypeMap,
    ) -> Result<
        (
            media_factory::Controller<media_factory::controller::Client>,
            super::PresentationURI,
            TypeMap,
        ),
        crate::error::Error,
    > {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::FindMediaFactoryForUri {
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

    pub async fn create_session(
        &mut self,
        presentation_uri: super::PresentationURI,
        media: media::Controller<media::controller::Server>,
    ) -> Result<(), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::CreateSession {
                    client_id: self.context.id,
                    presentation_uri,
                    media,
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

    pub async fn find_session(
        &mut self,
        session_id: session::Id,
    ) -> Result<
        (
            media::Controller<media::controller::Client>,
            super::PresentationURI,
        ),
        crate::error::Error,
    > {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::FindSession {
                    client_id: self.context.id,
                    session_id,
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

    pub async fn keep_alive_session(
        &mut self,
        session_id: session::Id,
    ) -> Result<(), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::KeepAliveSession {
                    client_id: self.context.id,
                    session_id,
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

    pub async fn shutdown_session(
        &mut self,
        session_id: session::Id,
    ) -> Result<(), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                ClientMessage::ShutdownSession {
                    client_id: self.context.id,
                    session_id,
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

    pub(crate) async fn finished(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(ClientMessage::Finished(self.context.id).into())
            .await
    }

    pub async fn error(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(ClientMessage::Error(self.context.id).into())
            .await
    }

    pub(crate) fn finish_on_drop(&self) -> RunOnDrop {
        let mut sender = self.sender.clone();
        let id = self.context.id;
        RunOnDrop::new(move || {
            if let Err(err) = sender.try_send(ClientMessage::Finished(id).into()) {
                if err.is_full() {
                    error!("Can't finish controller on drop");
                }
            }
        })
    }
}

#[derive(Clone)]
pub struct Media {
    id: media::Id,
    session_id: session::Id,
}

impl Controller<Media> {
    pub(super) fn new(
        id: media::Id,
        session_id: session::Id,
        sender: mpsc::Sender<ControllerMessage>,
    ) -> Self {
        Controller {
            sender,
            context: Media { id, session_id },
        }
    }

    pub async fn keep_alive_session(&mut self) -> Result<(), crate::error::Error> {
        let (sender, receiver) = oneshot::channel();

        if let Err(_) = self
            .sender
            .send(
                MediaMessage::KeepAliveSession {
                    media_id: self.context.id,
                    session_id: self.context.session_id.clone(),
                    ret: Some(sender),
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

    pub(crate) fn keep_alive_session_non_async(&mut self) -> Result<(), mpsc::SendError> {
        self.sender.try_send(
            MediaMessage::KeepAliveSession {
                media_id: self.context.id,
                session_id: self.context.session_id.clone(),
                ret: None,
            }
            .into(),
        )
    }

    pub(crate) async fn finished(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(
                MediaMessage::Finished(self.context.id.clone(), self.context.session_id.clone())
                    .into(),
            )
            .await
    }

    pub async fn error(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(
                MediaMessage::Error(self.context.id.clone(), self.context.session_id.clone())
                    .into(),
            )
            .await
    }
}

#[derive(Clone)]
pub struct MediaFactory {
    id: media_factory::Id,
}

impl Controller<MediaFactory> {
    pub(super) fn new(id: media_factory::Id, sender: mpsc::Sender<ControllerMessage>) -> Self {
        Controller {
            sender,
            context: MediaFactory { id },
        }
    }

    pub fn media_factory_id(&self) -> media_factory::Id {
        self.context.id
    }

    pub(crate) async fn error(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(MediaFactoryMessage::Error(self.context.id).into())
            .await
    }

    pub(crate) async fn finished(&mut self) -> Result<(), mpsc::SendError> {
        self.sender
            .send(MediaFactoryMessage::Finished(self.context.id).into())
            .await
    }

    pub(crate) fn finish_on_drop(&self) -> RunOnDrop {
        let mut sender = self.sender.clone();
        let id = self.context.id;
        RunOnDrop::new(move || {
            if let Err(err) = sender.try_send(MediaFactoryMessage::Finished(id).into()) {
                if err.is_full() {
                    error!("Can't finish controller on drop");
                }
            }
        })
    }
}

/// Server builder
pub struct Builder {
    factory_fn: Box<
        dyn FnMut(
                Controller<Client>,
                listener::IncomingConnection,
            ) -> Option<client::Controller<client::controller::Server>>
            + Send,
    >,
    listeners: Vec<listener::Listener>,
    mounts: Option<super::mounts::Mounts>,
}

impl fmt::Debug for Builder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Builder")
    }
}

impl Builder {
    pub fn bind_tcp(self, addr: std::net::SocketAddr) -> Self {
        self.listener(crate::listener::tcp_listener::tcp_listener(
            addr,
            super::MAX_MESSAGE_SIZE,
        ))
    }

    pub fn listener(mut self, listener: listener::Listener) -> Self {
        self.listeners.push(listener);

        self
    }

    pub fn mounts(mut self, mounts: super::mounts::Mounts) -> Self {
        self.mounts = Some(mounts);

        self
    }

    pub fn run(self) -> Server {
        let Builder {
            listeners: configured_listeners,
            mut mounts,
            factory_fn,
        } = self;

        // Channel to send commands to the main server task
        let (server_sender, server_receiver) = mpsc::channel();

        let (controller_sender, controller_receiver) = mpsc::channel();

        // Then start all listeners and have them report back to the server task whenever a next
        // connection is being established or something goes wrong
        let mut listeners = HashMap::new();
        for listener in configured_listeners {
            let id = listener.id();

            let server_controller =
                Controller::<Listener>::new(listener.id(), controller_sender.clone());
            let listener = listener.spawn(server_controller);

            listeners.insert(id, listener);
        }

        if let Some(ref mut mounts) = mounts {
            mounts.spawn(controller_sender.clone());
        }

        let ctx = context::Context {
            factory_fn,
            server_sender,
            controller_sender,
            listeners,
            clients: HashMap::new(),
            sessions: HashMap::new(),
            mounts,
        };

        // Start the server task from which we will handle everything and pass all state needed
        // from above into the task
        Server(super::task::spawn(
            ctx,
            server_receiver,
            controller_receiver,
        ))
    }
}

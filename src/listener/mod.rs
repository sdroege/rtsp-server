// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::lock::Mutex;
use futures::prelude::*;

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

use log::{trace, warn};

use crate::body::Body;
use crate::channel::mpsc;
use crate::server;

use async_std::task;

pub(crate) mod message_socket;
pub(crate) mod tcp_listener;
pub use message_socket::ReadError;

use crate::typemap::TypeMap;

/// Unique identifier for a specific listener.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(Arc<url::Url>);

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<url::Url> for Id {
    fn from(uri: url::Url) -> Id {
        Id(Arc::new(uri))
    }
}

pub struct Listener {
    id: Id,
    setup: ListenerSetup,
}

pub type ListenerSetup =
    Pin<Box<dyn Future<Output = Result<IncomingConnectionStream, std::io::Error>> + Send>>;

pub type IncomingConnectionStream =
    Pin<Box<dyn Stream<Item = Result<IncomingConnection, std::io::Error>> + Send>>;

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub struct IncomingConnection {
    #[derivative(Debug = "ignore")]
    pub stream: MessageStream,
    #[derivative(Debug = "ignore")]
    pub sink: MessageSink,
    pub connection_info: ConnectionInformation,
}

/// Client connection information.
///
/// This is passed through the extra data in various places.
#[derive(Clone, Debug)]
pub struct ConnectionInformation(Arc<ConnectionInformationInner>);

impl ConnectionInformation {
    pub fn new(
        local_addr: Option<std::net::SocketAddr>,
        peer_addr: Option<std::net::SocketAddr>,
        extra_data: TypeMap,
    ) -> Self {
        ConnectionInformation(Arc::new(ConnectionInformationInner {
            local_addr,
            peer_addr,
            extra_data,
        }))
    }
}

#[derive(Debug)]
struct ConnectionInformationInner {
    local_addr: Option<std::net::SocketAddr>,
    peer_addr: Option<std::net::SocketAddr>,
    extra_data: TypeMap,
}

impl ConnectionInformation {
    /// Return the local address the client client connected to, if known.
    pub fn local_addr(&self) -> Option<&std::net::SocketAddr> {
        self.0.local_addr.as_ref()
    }

    /// Return the peer address of the client, if known.
    pub fn peer_addr(&self) -> Option<&std::net::SocketAddr> {
        self.0.peer_addr.as_ref()
    }

    /// Return the extra data for this connection.
    pub fn extra_data(&self) -> &TypeMap {
        &self.0.extra_data
    }
}

pub type MessageStream =
    Pin<Box<dyn Stream<Item = Result<rtsp_types::Message<Body>, ReadError>> + Send>>;

pub type MessageSink = Pin<Box<dyn Sink<rtsp_types::Message<Body>, Error = std::io::Error> + Send>>;

impl Listener {
    pub fn new<
        S: Future<Output = Result<IncomingConnectionStream, std::io::Error>> + Send + 'static,
    >(
        id: Id,
        setup: S,
    ) -> Self {
        Listener {
            id,
            setup: Box::pin(setup),
        }
    }

    pub fn id(&self) -> Id {
        self.id.clone()
    }

    pub(crate) fn spawn(
        self,
        mut server_controller: server::Controller<server::controller::Listener>,
    ) -> Controller {
        let guard = server_controller.finish_on_drop();

        assert_eq!(server_controller.listener_id(), self.id);

        let Listener { id, setup } = self;
        let (sender, mut receiver) = mpsc::channel();

        let id_clone = id.clone();

        let join_handle = task::spawn(async move {
            let id = id_clone;
            trace!("Listener {}: Starting", id);

            // Make sure to signal that the task is finished on drop
            let _guard = guard;

            use futures::future::Either::{Left, Right};

            let mut incoming = match future::select(setup, receiver.next()).await {
                Left((Ok(incoming), _)) => incoming,
                Left((Err(err), _)) => {
                    let _ = server_controller.error(err).await;
                    return;
                }
                Right((Some(ServerMessage::Quit), _)) | Right((None, _)) => return,
            };

            loop {
                match future::select(incoming.next(), receiver.next()).await {
                    Left((Some(Ok(connection)), _)) => {
                        trace!("Listener {}: New connection {:?}", id, connection);
                        if let Err(_) = server_controller.new_connection(connection).await {
                            break;
                        }
                    }
                    Left((Some(Err(err)), _)) => {
                        warn!("Listener {}: Error {}", id, err);
                        let _ = server_controller.error(err).await;
                        return;
                    }
                    Right((Some(ServerMessage::Quit), _)) => {
                        return;
                    }
                    Left((None, _)) | Right((None, _)) => {
                        return;
                    }
                }
            }

            trace!("Listener {}: Finished", id);
            let _ = server_controller.finished().await;
        });

        Controller {
            id,
            sender,
            join_handle: Arc::new(Mutex::new(Some(join_handle))),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Controller {
    id: Id,
    sender: mpsc::Sender<ServerMessage>,
    join_handle: Arc<Mutex<Option<task::JoinHandle<()>>>>,
}

impl Controller {
    pub async fn shutdown(mut self) -> Result<(), mpsc::SendError> {
        self.sender.send(ServerMessage::Quit).await?;

        if let Some(join_handle) = self.join_handle.lock().await.take() {
            join_handle.await;
        }

        Ok(())
    }
}

/// Messages sent from the server to the listener task
#[derive(Debug)]
enum ServerMessage {
    Quit,
}

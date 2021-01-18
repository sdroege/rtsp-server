use futures::prelude::*;

use std::collections::HashSet;

use log::{debug, error, info, trace, warn};

use async_std::task;

use crate::channel::mpsc;
use crate::client;
use crate::media;

use super::context;
use super::controller::{self, Controller};
use super::messages::*;
use super::session::Session;

async fn task_fn(
    mut ctx: context::Context,
    receiver: mpsc::Receiver<ServerMessage>,
    controller_receiver: mpsc::Receiver<ControllerMessage>,
) {
    let mut merged_streams = stream::select(
        receiver,
        controller_receiver
            .map(ServerMessage::Controller)
            .chain(stream::once(future::ready(ServerMessage::ControllerClosed))),
    );

    while let Some(msg) = merged_streams.next().await {
        trace!("Received server message {:?}", msg);
        match msg {
            ServerMessage::ControllerClosed => {
                info!("Controller closed, shutting down");
                break;
            }
            ServerMessage::CheckSessionTimeout(session_id) => {
                trace!("Checking timeout for session {}", session_id);

                if let Some(session) = ctx.sessions.get(&session_id) {
                    let elapsed = session.last_active.elapsed();

                    if elapsed >= std::time::Duration::from_secs(60) {
                        debug!("Session {} timed out", session_id);

                        let mut session = ctx.sessions.remove(&session_id).unwrap();
                        let mut client_controller = None;
                        if let Some(client_id) = session.client_id {
                            if let Some(client) = ctx.clients.get(&client_id) {
                                client_controller = Some(client.controller.clone());
                            }
                        }

                        task::spawn(async move {
                            let _ = session.media.timed_out().await;
                            if let Some(mut client_controller) = client_controller {
                                let _ = client_controller.timed_out(session_id).await;
                            }
                        });
                    } else {
                        let mut server_sender = ctx.server_sender.clone();
                        task::spawn(async move {
                            let remaining = std::time::Duration::from_secs(60) - elapsed;
                            async_std::task::sleep(remaining).await;
                            let _ = server_sender
                                .send(ServerMessage::CheckSessionTimeout(session_id))
                                .await;
                        });
                    }
                }
            }
            ServerMessage::Controller(ControllerMessage::App(msg)) => match msg {
                AppMessage::Quit => {
                    info!("Application wants to shut down server");
                    break;
                }
            },
            ServerMessage::Controller(ControllerMessage::Listener(msg)) => {
                match msg {
                    ListenerMessage::NewConnection(listener_id, connection) => {
                        let client_id = client::Id::new();

                        // TODO: client limit

                        debug!(
                            "Got new connection {:?} on listener {}, assigning client listener_id {}",
                            connection, listener_id, client_id,
                        );

                        let server_controller = Controller::<controller::Client>::new(
                            client_id,
                            ctx.controller_sender.clone(),
                        );

                        if let Some(client_controller) =
                            (ctx.factory_fn)(server_controller, connection)
                        {
                            debug!("Created new client {}", client_id);
                            ctx.clients.insert(
                                client_id,
                                context::Client {
                                    controller: client_controller,
                                    sessions: HashSet::new(),
                                },
                            );
                        } else {
                            debug!("No client created");
                        }
                    }
                    ListenerMessage::Error(listener_id, err) => {
                        warn!("Error on listener {}: {}", listener_id, err);

                        ctx.listeners.remove(&listener_id);
                    }
                    ListenerMessage::Finished(listener_id) => {
                        debug!("Listener {} finished", listener_id);
                        ctx.listeners.remove(&listener_id);
                    }
                }
            }
            ServerMessage::Controller(ControllerMessage::Client(msg)) => {
                match msg {
                    ClientMessage::FindMediaFactoryForUri {
                        client_id,
                        uri,
                        extra_data,
                        ret,
                    } => {
                        if ctx.clients.contains_key(&client_id) {
                            debug!("Client {} looking for media factory at {}", client_id, uri);

                            if let Some(ref mounts) = ctx.mounts {
                                if let Some(media_factory_controller) =
                                    mounts.match_uri(client_id, &uri, extra_data)
                                {
                                    debug!(
                                        "Client {} found media factory {} for {}",
                                        client_id,
                                        media_factory_controller.media_factory_id(),
                                        uri
                                    );
                                    let _ = ret.send(Ok(media_factory_controller));
                                } else {
                                    debug!(
                                        "Client {} no media factory for {} available",
                                        client_id, uri
                                    );
                                    let _ = ret.send(Err(crate::error::ErrorStatus::from(
                                        rtsp_types::StatusCode::NotFound,
                                    )
                                    .into()));
                                }
                            } else {
                                error!("Client {} asked for a media factory for {} but no mounts available", client_id, uri);
                                let _ = ret.send(Err(crate::error::ErrorStatus::from(
                                    rtsp_types::StatusCode::NotFound,
                                )
                                .into()));
                            }
                        } else {
                            warn!(
                                "Unknown client {} looking for media factory at {}",
                                client_id, uri
                            );
                            let _ = ret.send(Err(crate::error::InternalServerError.into()));
                        }
                    }
                    ClientMessage::CreateSession {
                        client_id,
                        presentation_uri,
                        media,
                        ret,
                    } => {
                        let session_id = media.session_id();

                        debug!(
                            "Client {} creating session {} for presentation URI {} and media {}",
                            client_id,
                            session_id,
                            presentation_uri,
                            media.media_id()
                        );

                        if ctx.sessions.contains_key(&session_id) {
                            warn!(
                                "Client {} tried to create session {} but it exists already",
                                client_id,
                                media.session_id()
                            );
                            let _ = ret.send(Err(crate::error::InternalServerError.into()));
                        } else if let Some(client) = ctx.clients.get_mut(&client_id) {
                            let mut server_sender = ctx.server_sender.clone();

                            let client_controller = client::Controller::<client::controller::Media>::from_server_controller(&client.controller, media.media_id());
                            client.sessions.insert(session_id.clone());
                            let mut media_clone = media.clone();

                            ctx.sessions.insert(
                                media.session_id(),
                                Session {
                                    media,
                                    client_id: Some(client_id),
                                    presentation_uri,
                                    last_active: std::time::Instant::now(),
                                },
                            );

                            if ret.send(Ok(())).is_err() {
                                warn!("Client {} disconnected in the meantime", client_id);
                                ctx.sessions.remove(&session_id);
                            } else {
                                let server_controller = Controller::<controller::Media>::new(
                                    media_clone.media_id(),
                                    media_clone.session_id(),
                                    ctx.controller_sender.clone(),
                                );
                                // Tell media of the new client and its session
                                task::spawn(async move {
                                    let _ = media_clone
                                        .client_changed(server_controller, Some(client_controller))
                                        .await;
                                });

                                // Add timeout for the session
                                task::spawn(async move {
                                    async_std::task::sleep(std::time::Duration::from_secs(60))
                                        .await;
                                    let _ = server_sender
                                        .send(ServerMessage::CheckSessionTimeout(session_id))
                                        .await;
                                });
                            }
                        } else {
                            error!(
                                "Client {} tried to create session {} but the client is not known",
                                client_id, session_id
                            );
                            let _ = ret.send(Err(crate::error::InternalServerError.into()));
                        }
                    }
                    ClientMessage::FindSessionMedia {
                        client_id,
                        session_id,
                        ret,
                    } => {
                        debug!(
                            "Client {} searching for session media with session id {}",
                            client_id, session_id
                        );

                        if let Some(session) = ctx.sessions.get_mut(&session_id) {
                            if let Some(client) = ctx.clients.get_mut(&client_id) {
                                session.last_active = std::time::Instant::now();

                                client.sessions.insert(session_id.clone());
                                let client_controller = client::Controller::<
                                    client::controller::Media,
                                >::from_server_controller(
                                    &client.controller,
                                    session.media.media_id(),
                                );

                                let mut old_client_controller = None;
                                if let Some(old_client_id) = session.client_id {
                                    if let Some(old_client) = ctx.clients.get_mut(&old_client_id) {
                                        old_client.sessions.remove(&session_id);
                                        old_client_controller = Some(old_client.controller.clone());
                                    }
                                }
                                session.client_id = Some(client_id);

                                // Notify old client and media asynchronously and then reply to the new
                                // client
                                let mut media = session.media.clone();
                                let server_controller = Controller::<controller::Media>::new(
                                    media.media_id(),
                                    media.session_id(),
                                    ctx.controller_sender.clone(),
                                );
                                task::spawn(async move {
                                    if let Some(mut old_client_controller) = old_client_controller {
                                        let _ =
                                            old_client_controller.replaced_client(session_id).await;
                                    }
                                    let _ = media
                                        .client_changed(server_controller, Some(client_controller))
                                        .await;

                                    let _ = ret.send(Ok(media::Controller::<
                                        media::controller::Client,
                                    >::from_server_controller(
                                        &media, client_id
                                    )));
                                });
                            } else {
                                error!(
                                    "Client {} tried to create session {} but the client is not known",
                                    client_id, session_id
                                );
                                let _ = ret.send(Err(crate::error::InternalServerError.into()));
                            }
                        } else {
                            info!(
                                "Client {} tried to create session {} but it does not exist",
                                client_id, session_id
                            );
                            let _ = ret.send(Err(crate::error::ErrorStatus::from(
                                rtsp_types::StatusCode::SessionNotFound,
                            )
                            .into()));
                        }
                    }
                    ClientMessage::KeepAliveSession {
                        client_id,
                        session_id,
                        ret,
                    } => {
                        debug!(
                            "Client {} keeping alive session id {}",
                            client_id, session_id
                        );

                        if let Some(session) = ctx.sessions.get_mut(&session_id) {
                            if Some(client_id) == session.client_id {
                                session.last_active = std::time::Instant::now();
                                let _ = ret.send(Ok(()));
                            } else {
                                warn!("Client {} not in session {}", client_id, session_id);
                                let _ = ret.send(Err(crate::error::InternalServerError.into()));
                            }
                        } else {
                            info!(
                                "Client {} tried to keep alive session {} but it does not exist",
                                client_id, session_id
                            );
                            let _ = ret.send(Err(crate::error::ErrorStatus::from(
                                rtsp_types::StatusCode::SessionNotFound,
                            )
                            .into()));
                        }
                    }
                    ClientMessage::ShutdownSession {
                        client_id,
                        session_id,
                        ret,
                    } => {
                        debug!(
                            "Client {} shutting down session id {}",
                            client_id, session_id
                        );

                        if let Some(mut session) = ctx.sessions.remove(&session_id) {
                            if Some(client_id) == session.client_id {
                                task::spawn(async move {
                                    // TODO: timed out seems not correct here but will do for now
                                    let _ = session.media.timed_out().await;
                                });
                            }
                            let _ = ret.send(Ok(()));
                        } else {
                            info!(
                                "Client {} tried to remove session {} but it does not exist",
                                client_id, session_id
                            );
                            let _ = ret.send(Err(crate::error::ErrorStatus::from(
                                rtsp_types::StatusCode::SessionNotFound,
                            )
                            .into()));
                        }
                    }
                    ClientMessage::Error(client_id) => {
                        // FIXME: Needs some error type
                        if let Some(client) = ctx.clients.remove(&client_id) {
                            warn!("Error on client {}", client_id);
                            for session_id in client.sessions {
                                if let Some(mut session) = ctx.sessions.get_mut(&session_id) {
                                    assert_eq!(
                                        session.client_id,
                                        Some(client.controller.client_id())
                                    );
                                    session.client_id = None;

                                    let mut media_controller = session.media.clone();
                                    let server_controller = Controller::<controller::Media>::new(
                                        media_controller.media_id(),
                                        media_controller.session_id(),
                                        ctx.controller_sender.clone(),
                                    );
                                    task::spawn(async move {
                                        let _ = media_controller
                                            .client_changed(server_controller, None)
                                            .await;
                                    });
                                }
                            }
                        } else {
                            warn!("Error on unknown client {}", client_id);
                        }

                        // We do not remove sessions here as they can run without a client
                        // connected to the server directly, and would time out separately if there
                        // is really no activity any longer.
                    }
                    ClientMessage::Finished(client_id) => {
                        if let Some(client) = ctx.clients.remove(&client_id) {
                            debug!("Client {} finished", client_id);
                            for session_id in client.sessions {
                                if let Some(mut session) = ctx.sessions.get_mut(&session_id) {
                                    assert_eq!(
                                        session.client_id,
                                        Some(client.controller.client_id())
                                    );
                                    session.client_id = None;

                                    let mut media_controller = session.media.clone();
                                    let server_controller = Controller::<controller::Media>::new(
                                        media_controller.media_id(),
                                        media_controller.session_id(),
                                        ctx.controller_sender.clone(),
                                    );
                                    task::spawn(async move {
                                        let _ = media_controller
                                            .client_changed(server_controller, None)
                                            .await;
                                    });
                                }
                            }
                        } else {
                            warn!("Unknown client {} finished", client_id);
                        }

                        // We do not remove sessions here as they can run without a client
                        // connected to the server directly, and would time out separately if there
                        // is really no activity any longer.
                    }
                }
            }
            ServerMessage::Controller(ControllerMessage::Media(msg)) => match msg {
                MediaMessage::KeepAliveSession {
                    media_id,
                    session_id,
                    ret,
                } => {
                    debug!("Media {} keeping alive session id {}", media_id, session_id);

                    if let Some(session) = ctx.sessions.get_mut(&session_id) {
                        assert_eq!(media_id, session.media.media_id());
                        session.last_active = std::time::Instant::now();
                        if let Some(ret) = ret {
                            let _ = ret.send(Ok(()));
                        }
                    } else {
                        info!(
                            "Media {} tried to keep-alive session {} but it does not exist",
                            media_id, session_id
                        );

                        if let Some(ret) = ret {
                            let _ = ret.send(Err(crate::error::ErrorStatus::from(
                                rtsp_types::StatusCode::SessionNotFound,
                            )
                            .into()));
                        }
                    }
                }
                MediaMessage::Finished(media_id, session_id) => {
                    debug!("Media {} for session id {} finished", media_id, session_id);

                    if let Some(session) = ctx.sessions.remove(&session_id) {
                        assert_eq!(media_id, session.media.media_id());

                        if let Some(mut client) = session
                            .client_id
                            .and_then(|client_id| ctx.clients.get_mut(&client_id))
                            .map(|c| c.controller.clone())
                        {
                            task::spawn(async move {
                                // TODO: timed out seems not correct here but will do for now
                                let _ = client.timed_out(session_id).await;
                            });
                        }
                    }
                }
                MediaMessage::Error(media_id, session_id) => {
                    debug!("Media {} for session id {} error", media_id, session_id);

                    if let Some(session) = ctx.sessions.remove(&session_id) {
                        assert_eq!(media_id, session.media.media_id());

                        if let Some(mut client) = session
                            .client_id
                            .and_then(|client_id| ctx.clients.get_mut(&client_id))
                            .map(|c| c.controller.clone())
                        {
                            task::spawn(async move {
                                // TODO: timed out seems not correct here but will do for now
                                let _ = client.timed_out(session_id).await;
                            });
                        }
                    }
                }
            },
            ServerMessage::Controller(ControllerMessage::MediaFactory(msg)) => match msg {
                MediaFactoryMessage::Error(media_factory_id) => {
                    error!("Media factory {} errored", media_factory_id);

                    if let Some(ref mut mounts) = ctx.mounts {
                        mounts.media_factory_finished(media_factory_id);
                    }
                }
                MediaFactoryMessage::Finished(media_factory_id) => {
                    warn!("Media factory {} finished early", media_factory_id);

                    if let Some(ref mut mounts) = ctx.mounts {
                        mounts.media_factory_finished(media_factory_id);
                    }
                }
            },
        }
    }

    debug!("Shutting down server");
    drop(merged_streams);
    let context::Context {
        listeners,
        clients,
        mounts,
        sessions,
        ..
    } = ctx;

    // Wait until everything is actually shut down
    let mut close_tasks =
        stream::FuturesUnordered::<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>>::new();

    for (session_id, mut session) in sessions {
        let mut client_controller = None;
        if let Some(client_id) = session.client_id {
            if let Some(client) = clients.get(&client_id) {
                client_controller = Some(client.controller.clone());
            }
        }

        // TODO: timed out seems not correct here but will do for now
        close_tasks.push(Box::pin(async move {
            if let Some(mut client_controller) = client_controller {
                let _ = client_controller.timed_out(session_id).await;
            }
            let _ = session.media.timed_out().await;
        }));
    }

    for (_, listener) in listeners {
        close_tasks.push(Box::pin(async move {
            let _ = listener.shutdown().await;
        }));
    }

    for (_, client) in clients {
        close_tasks.push(Box::pin(async move {
            let _ = client.controller.shutdown().await;
        }));
    }

    if let Some(mounts) = mounts {
        mounts.shutdown(&mut close_tasks);
    }

    while let Some(_) = close_tasks.next().await {}

    debug!("Server shut down");
}

pub(super) fn spawn(
    ctx: context::Context,
    receiver: mpsc::Receiver<ServerMessage>,
    controller_receiver: mpsc::Receiver<ControllerMessage>,
) -> Controller<controller::App> {
    let controller_sender = ctx.controller_sender.clone();

    let join_handle = task::spawn(task_fn(ctx, receiver, controller_receiver));

    Controller::<controller::App>::new(join_handle, controller_sender)
}

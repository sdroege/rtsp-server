use std::collections::HashMap;
use std::mem;

use futures::prelude::*;
use futures::stream::FuturesUnordered;

use log::{debug, trace, warn};

use async_std::task;

use crate::channel::mpsc;
use crate::media_factory;
use crate::stream_handler;
use crate::typemap::TypeMap;

use super::context::Context;
use super::controller::{self, Controller};
use super::messages::*;
use super::Media;

async fn task_fn<M: Media>(
    mut media: M,
    mut ctx: Context<M>,
    media_receiver: mpsc::Receiver<MediaMessage<M>>,
    controller_receiver: mpsc::Receiver<ControllerMessage>,
    mut stream_handler: stream_handler::StreamHandler<M, Context<M>>,
) {
    let _finish_on_drop = ctx.media_factory_controller.finish_on_drop();

    let mut merged_streams = stream::select(
        media_receiver,
        controller_receiver
            .map(MediaMessage::Controller)
            .chain(stream::once(future::ready(MediaMessage::ControllerClosed))),
    );

    media.startup(&mut ctx);

    let id = ctx.id;

    loop {
        let msg = {
            use future::Either::{Left, Right};

            match future::select(
                merged_streams.next(),
                stream_handler.poll(&mut media, &mut ctx),
            )
            .await
            {
                Left((Some(msg), _)) => msg,
                Left((None, _)) => break,
                Right(_) => continue,
            }
        };

        match msg {
            MediaMessage::MediaFuture(func) => {
                trace!("Media {}: Handling media future", ctx.id);
                let fut = func(&mut media, &mut ctx);
                task::spawn(fut);
            }
            MediaMessage::MediaClosure(func) => {
                trace!("Media {}: Handling media closure", ctx.id);
                func(&mut media, &mut ctx);
            }
            MediaMessage::ControllerClosed => {
                debug!("Media {}: Controller closed, quitting", ctx.id);
                break;
            }
            MediaMessage::Controller(ControllerMessage::MediaFactory(msg)) => match msg {
                MediaFactoryMessage::Quit(media_factory_id) => {
                    debug!(
                        "Media {}: Quitting from media factory {}",
                        ctx.id, media_factory_id
                    );
                    break;
                }
                MediaFactoryMessage::Options {
                    media_factory_id,
                    stream_id,
                    supported,
                    require,
                    extra_data,
                    ret,
                } => {
                    debug!(
                        "Media {}: Options from media factory {}, supported {:?}, require {:?}",
                        ctx.id, media_factory_id, supported, require
                    );

                    let fut =
                        media.options(&mut ctx, None, stream_id, supported, require, extra_data);
                    task::spawn(async move {
                        let res = fut.await;

                        debug!(
                            "Media {}: Options from media factory {}, returned {:?}",
                            id, media_factory_id, res
                        );
                        let _ = ret.send(res);
                    });
                }
                MediaFactoryMessage::Describe {
                    media_factory_id,
                    extra_data,
                    ret,
                } => {
                    debug!(
                        "Media {}: Describe from media factory {}",
                        ctx.id, media_factory_id
                    );

                    let fut = media.describe(&mut ctx, None, extra_data);
                    task::spawn(async move {
                        let res = fut.await;

                        debug!(
                            "Media {}: Describe from media factory {}, returned {:?}",
                            id, media_factory_id, res
                        );
                        let _ = ret.send(res);
                    });
                }
            },
            MediaMessage::Controller(ControllerMessage::Server(msg)) => match msg {
                ServerMessage::ClientChanged(
                    session_id,
                    server_controller,
                    new_client_controller,
                ) => {
                    let new_client_id = new_client_controller.as_ref().map(|c| c.client_id());
                    trace!(
                        "Media {}: Session {} has changed client {:?}",
                        ctx.id,
                        session_id,
                        new_client_id,
                    );

                    let mut old_client_id = None;
                    if let Some((ref mut client_id, ref mut old_server_controller)) =
                        ctx.sessions.get_mut(&session_id)
                    {
                        old_client_id = mem::replace(client_id, new_client_id);
                        // FIXME: Shouldn't really ever change, maybe make it optional and only
                        // provide on session creation so it can be asserted
                        *old_server_controller = server_controller;
                    } else {
                        ctx.sessions
                            .insert(session_id.clone(), (new_client_id, server_controller));

                        if M::AUTOMATIC_IDLE && ctx.idle {
                            trace!("Media {}: Idle {}", ctx.id, false);
                            ctx.idle = false;
                            let mut media_factory_controller = ctx.media_factory_controller.clone();
                            task::spawn(async move {
                                let _ = media_factory_controller
                                    .idle(false, TypeMap::default())
                                    .await;
                            });
                        }
                    }

                    if let Some(old_client_id) = old_client_id {
                        if let Some(mut old_client) = ctx.session_clients.remove(&old_client_id) {
                            task::spawn(async move {
                                let _ = old_client.finished().await;
                            });
                        }
                    }

                    if let Some(new_client_controller) = new_client_controller.clone() {
                        ctx.session_clients
                            .insert(new_client_controller.client_id(), new_client_controller);
                    }

                    let handle = ctx.handle();
                    media.session_new_client(
                        &mut ctx,
                        session_id,
                        new_client_controller
                            .map(|controller| super::ClientHandle { controller, handle }),
                    );
                }
                ServerMessage::TimedOut(session_id) => {
                    if let Some((client_id, mut server_controller)) =
                        ctx.sessions.remove(&session_id)
                    {
                        trace!("Media {}: Session {} timed out", ctx.id, session_id);

                        let mut client = None;
                        if let Some(client_id) = client_id {
                            client = ctx.session_clients.remove(&client_id);
                        }

                        task::spawn(async move {
                            if let Some(mut client) = client {
                                let _ = client.finished().await;
                            }
                            let _ = server_controller.finished().await;
                        });

                        if M::AUTOMATIC_IDLE && !ctx.idle {
                            trace!("Media {}: Idle {}", ctx.id, true);
                            ctx.idle = true;
                            let mut media_factory_controller = ctx.media_factory_controller.clone();
                            task::spawn(async move {
                                let _ = media_factory_controller
                                    .idle(true, TypeMap::default())
                                    .await;
                            });
                        }

                        media.session_timed_out(&mut ctx, session_id);
                    } else {
                        warn!("Media {}: Unknown session {} timed out", ctx.id, session_id);
                    }
                }
            },
            MediaMessage::Controller(ControllerMessage::Client(msg)) => match msg {
                ClientMessage::FindMediaFactory { client_id, ret } => {
                    trace!(
                        "Media {}: Client {} asking for media factory",
                        ctx.id,
                        client_id,
                    );

                    let _ = ret.send(Ok(media_factory::Controller::<
                        media_factory::controller::Client,
                    >::from_media_controller(
                        &ctx.media_factory_controller, client_id
                    )));
                }
                ClientMessage::Options {
                    client_id,
                    stream_id,
                    supported,
                    require,
                    extra_data,
                    ret,
                } => {
                    trace!(
                        "Media {}: Options for client {}, supported {:?}, require {:?}",
                        ctx.id,
                        client_id,
                        supported,
                        require
                    );
                    let fut = media.options(
                        &mut ctx,
                        Some(client_id),
                        stream_id,
                        supported,
                        require,
                        extra_data,
                    );
                    task::spawn(async move {
                        let res = fut.await;
                        trace!(
                            "Media {}: Options for client {} returned {:?}",
                            id,
                            client_id,
                            res
                        );
                        let _ = ret.send(res);
                    });
                }
                ClientMessage::Describe {
                    client_id,
                    extra_data,
                    ret,
                } => {
                    trace!("Media {}: Describe for client {}", ctx.id, client_id);
                    let fut = media.describe(&mut ctx, Some(client_id), extra_data);
                    task::spawn(async move {
                        let res = fut.await;
                        trace!(
                            "Media {}: Describe for client {} returned {:?}",
                            id,
                            client_id,
                            res
                        );
                        let _ = ret.send(res);
                    });
                }
                ClientMessage::AddTransport {
                    client_id,
                    session_id,
                    stream_id,
                    transports,
                    accept_ranges,
                    extra_data,
                    ret,
                } => {
                    trace!(
                        "Media {}: Add Transports {:?} for client {} session {} stream {}",
                        ctx.id,
                        transports,
                        client_id,
                        session_id,
                        stream_id,
                    );

                    if let Err(err) = ctx.is_client_in_session(client_id, &session_id) {
                        let _ = ret.send(Err(err));
                    } else {
                        let fut = media.add_transport(
                            &mut ctx,
                            client_id,
                            session_id,
                            stream_id,
                            transports,
                            accept_ranges,
                            extra_data,
                        );
                        task::spawn(async move {
                            let res = fut.await;
                            trace!(
                                "Media {}: Adding transport for client {} returned {:?}",
                                id,
                                client_id,
                                res
                            );
                            let _ = ret.send(res);
                        });
                    }
                }
                ClientMessage::RemoveTransport {
                    client_id,
                    session_id,
                    stream_id,
                    extra_data,
                    ret,
                } => {
                    trace!(
                        "Media {}: Remove Transport from session {} stream id {} for client {}",
                        ctx.id,
                        session_id,
                        stream_id,
                        client_id
                    );

                    if let Err(err) = ctx.is_client_in_session(client_id, &session_id) {
                        let _ = ret.send(Err(err));
                    } else {
                        let fut = media.remove_transport(
                            &mut ctx, client_id, session_id, stream_id, extra_data,
                        );
                        task::spawn(async move {
                            let res = fut.await;
                            trace!(
                                "Media {}: Removing transport for client {} returned {:?}",
                                id,
                                client_id,
                                res
                            );
                            let _ = ret.send(res);
                        });
                    }
                }
                ClientMessage::ShutdownSession {
                    client_id,
                    session_id,
                    extra_data,
                    ret,
                } => {
                    trace!(
                        "Media {}: Shutting down session {} for client {}",
                        ctx.id,
                        session_id,
                        client_id
                    );

                    if let Err(err) = ctx.is_client_in_session(client_id, &session_id) {
                        let _ = ret.send(Err(err));
                    } else {
                        let fut = media.shutdown_session(
                            &mut ctx,
                            client_id,
                            session_id.clone(),
                            extra_data,
                        );

                        let (_, mut server_controller) =
                            ctx.sessions.remove(&session_id).expect("session not found");
                        let mut client_controller = ctx
                            .session_clients
                            .remove(&client_id)
                            .expect("client not found");

                        if M::AUTOMATIC_IDLE && !ctx.idle {
                            trace!("Media {}: Idle {}", ctx.id, true);
                            ctx.idle = true;
                            let mut media_factory_controller = ctx.media_factory_controller.clone();
                            task::spawn(async move {
                                let _ = media_factory_controller
                                    .idle(true, TypeMap::default())
                                    .await;
                            });
                        }

                        task::spawn(async move {
                            let _ = server_controller.finished().await;
                            let _ = client_controller.finished().await;

                            let res = fut.await;
                            trace!(
                                "Media {}: Shutting down session for client {} returned {:?}",
                                id,
                                client_id,
                                res
                            );
                            let _ = ret.send(res);
                        });
                    }
                }
                ClientMessage::Play {
                    client_id,
                    session_id,
                    stream_id,
                    range,
                    seek_style,
                    scale,
                    speed,
                    extra_data,
                    ret,
                } => {
                    trace!(
                        "Media {}: Play with range {:?} for client {} session {}",
                        ctx.id,
                        range,
                        client_id,
                        session_id
                    );

                    if let Err(err) = ctx.is_client_in_session(client_id, &session_id) {
                        let _ = ret.send(Err(err));
                    } else {
                        let fut = media.play(
                            &mut ctx,
                            client_id,
                            session_id.clone(),
                            stream_id,
                            range,
                            seek_style,
                            scale,
                            speed,
                            extra_data,
                        );
                        task::spawn(async move {
                            let res = fut.await;
                            trace!(
                                "Media {}: Play for client {} session {} returned {:?}",
                                id,
                                client_id,
                                session_id,
                                res
                            );
                            let _ = ret.send(res);
                        });
                    }
                }
                ClientMessage::Pause {
                    client_id,
                    session_id,
                    stream_id,
                    extra_data,
                    ret,
                } => {
                    trace!(
                        "Media {}: Pause for client {} session {}",
                        ctx.id,
                        client_id,
                        session_id
                    );

                    if let Err(err) = ctx.is_client_in_session(client_id, &session_id) {
                        let _ = ret.send(Err(err));
                    } else {
                        let fut = media.pause(
                            &mut ctx,
                            client_id,
                            session_id.clone(),
                            stream_id,
                            extra_data,
                        );
                        task::spawn(async move {
                            let res = fut.await;
                            trace!(
                                "Media {}: Pause for client {} session {} returned {:?}",
                                id,
                                client_id,
                                session_id,
                                res
                            );
                            let _ = ret.send(res);
                        });
                    }
                }
            },
        }
    }

    debug!("Shutting down media {}", ctx.id);
    drop(merged_streams);
    ctx.stream_registration.close();

    media.shutdown(&mut ctx).await;

    let mut close_tasks =
        FuturesUnordered::<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>>::new();

    let mut media_factory_controller = ctx.media_factory_controller;
    close_tasks.push(Box::pin(async move {
        let _ = media_factory_controller.finished().await;
    }));

    for (_, (client_id, mut server_controller)) in ctx.sessions {
        let mut client_controller = None;

        if let Some(client_id) = client_id {
            if let Some(client) = ctx.session_clients.remove(&client_id) {
                client_controller = Some(client);
            }
        }

        close_tasks.push(Box::pin(async move {
            if let Some(mut client_controller) = client_controller {
                let _ = client_controller.finished().await;
            }
            let _ = server_controller.finished().await;
        }));
    }

    for (_, mut client_controller) in ctx.session_clients {
        close_tasks.push(Box::pin(async move {
            let _ = client_controller.finished().await;
        }));
    }

    while let Some(_) = close_tasks.next().await {}

    debug!("Media {} shut down", ctx.id);
}

pub(crate) fn spawn<M: Media>(
    media_factory_controller: media_factory::Controller<media_factory::controller::Media>,
    media: M,
) -> Controller<controller::MediaFactory> {
    let id = media_factory_controller.media_id();
    let media_factory_id = media_factory_controller.media_factory_id();

    let (media_sender, media_receiver) = mpsc::channel();
    let (controller_sender, controller_receiver) = mpsc::channel();

    let (stream_handler, stream_registration) = stream_handler::StreamHandler::<M, _>::new();

    let ctx = Context {
        id,
        media_sender,
        controller_sender: controller_sender.clone(),
        stream_registration,
        media_factory_controller,
        sessions: HashMap::new(),
        session_clients: HashMap::new(),
        idle: true,
    };

    let join_handle = task::spawn(task_fn(
        media,
        ctx,
        media_receiver,
        controller_receiver,
        stream_handler,
    ));

    Controller::<controller::MediaFactory>::new(
        id,
        media_factory_id,
        join_handle,
        controller_sender,
    )
}

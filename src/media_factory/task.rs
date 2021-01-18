use std::collections::HashMap;

use futures::prelude::*;
use futures::stream::FuturesUnordered;

use log::{debug, info, trace, warn};

use async_std::task;

use crate::channel::mpsc;
use crate::media;
use crate::server;
use crate::stream_handler;

use super::context::Context;
use super::controller::{self, Controller};
use super::messages::*;
use super::MediaFactory;

async fn task_fn<MF: MediaFactory>(
    mut media_factory: MF,
    mut ctx: Context<MF>,
    receiver: mpsc::Receiver<MediaFactoryMessage<MF>>,
    controller_receiver: mpsc::Receiver<ControllerMessage>,
    mut stream_handler: stream_handler::StreamHandler<MF, Context<MF>>,
) {
    let mut merged_streams = stream::select(
        receiver,
        controller_receiver
            .map(MediaFactoryMessage::Controller)
            .chain(stream::once(future::ready(
                MediaFactoryMessage::ControllerClosed,
            ))),
    );

    let _finish_on_drop = ctx.server_controller.finish_on_drop();
    let id = ctx.id;

    media_factory.startup(&mut ctx);

    loop {
        let msg = {
            use future::Either::{Left, Right};

            match future::select(
                merged_streams.next(),
                stream_handler.poll(&mut media_factory, &mut ctx),
            )
            .await
            {
                Left((Some(msg), _)) => msg,
                Left((None, _)) => break,
                Right(_) => continue,
            }
        };

        match msg {
            MediaFactoryMessage::ControllerClosed => {
                info!("MediaFactory {}: Controller was closed, quitting", ctx.id);
                break;
            }
            MediaFactoryMessage::MediaFactoryFuture(func) => {
                trace!("MediaFactory {}: Handling media factory future", ctx.id);
                let fut = func(&mut media_factory, &mut ctx);
                task::spawn(fut);
            }
            MediaFactoryMessage::MediaFactoryClosure(func) => {
                trace!("MediaFactory {}: Handling media factory closure", ctx.id);
                func(&mut media_factory, &mut ctx);
            }
            MediaFactoryMessage::CheckMediaTimeout(media_id) => {
                if let Some((_, idle_timeout, idle_time)) = ctx.medias.get(&media_id) {
                    if let (Some(idle_time), Some(idle_timeout)) = (idle_time, idle_timeout) {
                        if idle_time.elapsed() >= *idle_timeout {
                            info!("Media Factory {}: Media {} timed out", ctx.id, media_id);

                            let (media_controller, _, _) =
                                ctx.medias.remove(&media_id).expect("media not found");
                            task::spawn(async move {
                                let _ = media_controller.shutdown().await;
                            });
                        }
                    }
                }
            }
            MediaFactoryMessage::Controller(ControllerMessage::Media(msg)) => match msg {
                MediaMessage::Idle {
                    media_id,
                    idle,
                    extra_data,
                    ret,
                } => {
                    if let Some((_, idle_timeout, ref mut idle_time)) =
                        ctx.medias.get_mut(&media_id)
                    {
                        debug!(
                            "MediaFactory {}: Media {} is idle {}",
                            ctx.id, media_id, idle
                        );

                        if idle {
                            *idle_time = Some(std::time::Instant::now());

                            if let Some(idle_timeout) = idle_timeout {
                                let idle_timeout = *idle_timeout;

                                let mut media_factory_sender = ctx.media_factory_sender.clone();
                                task::spawn(async move {
                                    async_std::task::sleep(idle_timeout).await;
                                    let _ = media_factory_sender
                                        .send(MediaFactoryMessage::CheckMediaTimeout(media_id))
                                        .await;
                                });
                            }

                            let fut =
                                media_factory.media_idle(&mut ctx, media_id, idle, extra_data);
                            task::spawn(async move {
                                let res = fut.await;

                                debug!(
                                    "MediaFactory {}: Media {} is idle {} result {:?}",
                                    id, media_id, idle, res
                                );

                                let _ = ret.send(res);
                            });
                        } else {
                            *idle_time = None;
                        }
                    } else {
                        warn!(
                            "MediaFactory {}: Unknown media {} is idle {}",
                            ctx.id, media_id, idle
                        );

                        let _ = ret.send(Err(crate::error::InternalServerError.into()));
                        continue;
                    }
                }
                MediaMessage::Error(media_id) => {
                    if ctx.medias.contains_key(&media_id) {
                        debug!("MediaFactory {}: Media {} had an error", ctx.id, media_id);

                        ctx.medias.remove(&media_id);
                        media_factory.media_error(&mut ctx, media_id);
                    } else {
                        warn!(
                            "MediaFactory {}: Unknown media {} had an error",
                            ctx.id, media_id
                        );
                    }
                }
                MediaMessage::Finished(media_id) => {
                    if ctx.medias.contains_key(&media_id) {
                        debug!("MediaFactory {}: Media {} finished", ctx.id, media_id);

                        ctx.medias.remove(&media_id);
                        media_factory.media_finished(&mut ctx, media_id);
                    } else {
                        warn!(
                            "MediaFactory {}: Unknown media {} finished",
                            ctx.id, media_id
                        );
                    }
                }
            },
            MediaFactoryMessage::Controller(ControllerMessage::Client(msg)) => match msg {
                ClientMessage::Options {
                    client_id,
                    uri,
                    supported,
                    require,
                    extra_data,
                    ret,
                } => {
                    debug!("MediaFactory {}: Options for URI {} from client {}, supported {:?}, require {:?}", ctx.id, uri, client_id, supported, require);

                    let fut = media_factory.options(&mut ctx, uri, supported, require, extra_data);
                    task::spawn(async move {
                        let res = fut.await;

                        debug!(
                            "MediaFactory {}: Options from client {}, returned {:?}",
                            id, client_id, res
                        );
                        let _ = ret.send(res);
                    });
                }
                ClientMessage::Describe {
                    client_id,
                    uri,
                    extra_data,
                    ret,
                } => {
                    debug!(
                        "MediaFactory {}: Describe for URI {} from client {}",
                        ctx.id, uri, client_id
                    );

                    let fut = media_factory.describe(&mut ctx, uri, extra_data);
                    task::spawn(async move {
                        let res = fut.await;

                        debug!(
                            "MediaFactory {}: Describe from client {}, returned {:?}",
                            id, client_id, res
                        );
                        let _ = ret.send(res);
                    });
                }
                ClientMessage::FindPresentationURI {
                    client_id,
                    uri,
                    extra_data,
                    ret,
                } => {
                    debug!(
                        "MediaFactory {}: Finding presentation URI for URI {} from client {}",
                        ctx.id, uri, client_id
                    );

                    let fut = media_factory.find_presentation_uri(&mut ctx, uri, extra_data);
                    task::spawn(async move {
                        let res = fut.await;

                        debug!(
                            "MediaFactory {}: Finding presentation URI from client {}, returned {:?}",
                            id, client_id, res
                        );
                        let _ = ret.send(res);
                    });
                }
                ClientMessage::CreateMedia {
                    client_id,
                    uri,
                    extra_data,
                    ret,
                } => {
                    debug!(
                        "MediaFactory {}: Create media for URI {} from client {}",
                        ctx.id, uri, client_id
                    );

                    let fut = media_factory.create_media(&mut ctx, uri, client_id, extra_data);
                    task::spawn(async move {
                        let res = fut.await;

                        debug!(
                            "MediaFactory {}: Create media for client {} returned",
                            id, client_id
                        );
                        let _ = ret
                            .send(res.map(|(handle, extra_data)|
                                (
                                    media::Controller::<media::controller::Client>::from_media_factory_controller(
                                        &handle.controller,
                                        client_id
                                    ),
                                    extra_data
                                )
                            ));
                    });
                }
            },
            MediaFactoryMessage::Controller(ControllerMessage::Server(msg)) => match msg {
                ServerMessage::Quit => {
                    info!("Server sent quit message, shutting down");
                    break;
                }
            },
        }
    }

    debug!("Shutting down media factory {}", ctx.id);
    drop(merged_streams);
    ctx.stream_registration.close();

    media_factory.shutdown(&mut ctx).await;

    let mut close_tasks =
        FuturesUnordered::<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>>::new();

    let mut server_controller = ctx.server_controller.clone();
    close_tasks.push(Box::pin(async move {
        let _ = server_controller.finished().await;
    }));

    // Shut down all medias that were not shut down yet
    for (_, (media, _, _)) in ctx.medias {
        close_tasks.push(Box::pin(async move {
            let _ = media.shutdown().await;
        }));
    }

    while let Some(_) = close_tasks.next().await {}

    debug!("MediaFactory {} shut down", ctx.id);
}

pub fn spawn<MF: MediaFactory>(
    server_controller: server::Controller<server::controller::MediaFactory>,
    media_factory: MF,
) -> Controller<controller::Server> {
    let id = server_controller.media_factory_id();
    let (media_factory_sender, media_factory_receiver) = mpsc::channel();
    let (controller_sender, controller_receiver) = mpsc::channel();

    let (stream_handler, stream_registration) = stream_handler::StreamHandler::<MF, _>::new();

    let ctx = Context {
        id,
        media_factory_sender,
        controller_sender: controller_sender.clone(),
        stream_registration,
        server_controller,
        medias: HashMap::new(),
    };

    let join_handle = task::spawn(task_fn(
        media_factory,
        ctx,
        media_factory_receiver,
        controller_receiver,
        stream_handler,
    ));

    Controller::<controller::Server>::new(id, join_handle, controller_sender)
}

use std::collections::HashMap;
use std::pin::Pin;

use futures::prelude::*;

use async_std::task;

use log::{error, info, trace, warn};

use crate::channel::{mpsc, oneshot};
use crate::media;
use crate::server;
use crate::stream_handler;
use crate::typemap::TypeMap;

use super::controller::{self, Controller};
use super::messages::*;
use super::{Id, MediaFactory};

pub struct Handle<MF: MediaFactory + ?Sized> {
    id: Id,
    sender: mpsc::Sender<MediaFactoryMessage<MF>>,
}

impl<MF: MediaFactory + ?Sized> Clone for Handle<MF> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            id: self.id,
        }
    }
}

pub struct MediaHandle<MF: MediaFactory + ?Sized> {
    pub(super) controller: media::Controller<media::controller::MediaFactory>,
    handle: Handle<MF>,
}

impl<MF: MediaFactory + ?Sized> Clone for MediaHandle<MF> {
    fn clone(&self) -> Self {
        Self {
            controller: self.controller.clone(),
            handle: self.handle.clone(),
        }
    }
}

impl<MF: MediaFactory + ?Sized> MediaHandle<MF> {
    pub fn media_id(&self) -> media::Id {
        self.controller.media_id()
    }

    /// Query OPTIONS from the media.
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
        trace!(
            "Media Factory {}: Getting OPTIONS for media {} with supported {:?} require {:?}",
            self.controller.media_factory_id(),
            self.controller.media_id(),
            supported,
            require
        );

        let mut controller = self.controller.clone();

        self.handle
            .spawn(move |_factory, ctx| {
                let found = ctx.medias.contains_key(&controller.media_id());

                async move {
                    if found {
                        let res = controller.options(supported, require, extra_data).await;
                        trace!(
                            "Media Factory {}: Media {} returned OPTIONS: {:?}",
                            controller.media_factory_id(),
                            controller.media_id(),
                            res
                        );

                        res
                    } else {
                        trace!(
                            "Media Factory {}: Media {} not found",
                            controller.media_factory_id(),
                            controller.media_id()
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

    /// Query a session description from the media.
    pub async fn describe(
        &mut self,
        extra_data: TypeMap,
    ) -> Result<(sdp_types::Session, TypeMap), crate::error::Error> {
        trace!(
            "Media Factory {}: Getting session description for media {}",
            self.controller.media_factory_id(),
            self.controller.media_id(),
        );

        let mut controller = self.controller.clone();

        self.handle
            .spawn(move |_factory, ctx| {
                let found = ctx.medias.contains_key(&controller.media_id());

                async move {
                    if found {
                        let res = controller.describe(extra_data).await;
                        trace!(
                            "Media Factory {}: Media {} returned session description: {:?}",
                            controller.media_factory_id(),
                            controller.media_id(),
                            res
                        );

                        res
                    } else {
                        trace!(
                            "Media Factory {}: Media {} not found",
                            controller.media_factory_id(),
                            controller.media_id()
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

    /// Shut down the media.
    pub async fn shutdown(&mut self) -> Result<(), crate::error::Error> {
        self.controller
            .clone()
            .shutdown()
            .await
            .map_err(|_| crate::error::InternalServerError)?;

        Ok(())
    }
}

pub struct Context<MF: MediaFactory + ?Sized> {
    pub(super) id: Id,

    /// Sender for sending internal messages to this media factory's task
    pub(super) media_factory_sender: mpsc::Sender<MediaFactoryMessage<MF>>,

    pub(super) controller_sender: mpsc::Sender<ControllerMessage>,

    /// Registration for custom media factory streams
    pub(super) stream_registration: stream_handler::StreamRegistration<MF, Self>,

    /// Server controller
    pub(super) server_controller: server::Controller<server::controller::MediaFactory>,

    /// Spawned media with their optional idle timeout and the moment when they went idle.
    pub(super) medias: HashMap<
        media::Id,
        (
            media::Controller<media::controller::MediaFactory>,
            Option<std::time::Duration>,
            Option<std::time::Instant>,
        ),
    >,
}

impl<MF: MediaFactory + ?Sized> Context<MF> {
    /// Return this media factory's id.
    pub fn id(&self) -> Id {
        self.id
    }

    /// Spawns a new media.
    ///
    /// The new media will be shut down automatically together with the media factory or can be
    /// shut down explicitly before via the returned controller, or after being idle for the
    /// configured time.
    ///
    /// Newly spawned medias are idle until a session is configured for them so an idle timeout of
    /// multiple seconds is usually a good idea.
    pub fn spawn_media<M: media::Media>(
        &mut self,
        idle_timeout: Option<std::time::Duration>,
        media: M,
    ) -> MediaHandle<MF> {
        let media_id = media::Id::new();

        info!(
            "Media Factory {}: Spawning new media {} with idle timeout {:?}",
            self.id, media_id, idle_timeout
        );

        let media_factory_controller =
            Controller::<controller::Media>::new(self.id, media_id, self.controller_sender.clone());
        let media_controller = media::spawn(media_factory_controller, media);

        self.medias.insert(
            media_id,
            (
                media_controller.clone(),
                idle_timeout,
                Some(std::time::Instant::now()),
            ),
        );

        if let Some(idle_timeout) = idle_timeout {
            let mut media_factory_sender = self.media_factory_sender.clone();
            task::spawn(async move {
                async_std::task::sleep(idle_timeout).await;
                let _ = media_factory_sender
                    .send(MediaFactoryMessage::CheckMediaTimeout(media_id))
                    .await;
            });
        }

        MediaHandle {
            controller: media_controller,
            handle: self.handle(),
        }
    }

    /// Gets a media that was spawned before and is still running.
    pub fn find_media(&self, media_id: media::Id) -> Option<MediaHandle<MF>> {
        self.medias
            .get(&media_id)
            .map(|(controller, _, _)| MediaHandle {
                controller: controller.clone(),
                handle: self.handle(),
            })
    }

    /// Signals to the server that this media factory had a fatal error.
    ///
    /// This shuts down the media factory and all currently spawned medias.
    pub fn error(&mut self) {
        error!("Media Factory {}: Errored", self.id);
        let mut server_controller = self.server_controller.clone();
        let mut controller_sender = self.controller_sender.clone();
        task::spawn(async move {
            let _ = server_controller.error().await;
            controller_sender.close_channel();
        });
    }

    /// Shuts down this media factory.
    ///
    /// This also shuts down all currently spawned medias.
    pub fn shutdown(&mut self) {
        info!("Media Factory {}: Shutting down", self.id);
        self.controller_sender.close_channel();
    }

    pub fn handle(&self) -> Handle<MF> {
        Handle {
            sender: self.media_factory_sender.clone(),
            id: self.id,
        }
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
        MF: stream_handler::MessageHandler<Msg, Token, Context = Self>,
    {
        self.stream_registration.register(token, stream)
    }
}

impl<MF: MediaFactory + ?Sized> Handle<MF> {
    /// Return this media factory's id.
    pub fn id(&self) -> Id {
        self.id
    }

    /// Spawns a new media.
    ///
    /// The new media will be shut down automatically together with the media factory or can be
    /// shut down explicitly before via the returned controller, or after being idle for the
    /// configured time.
    ///
    /// Newly spawned medias are idle until a session is configured for them so an idle timeout of
    /// multiple seconds is usually a good idea.
    pub async fn spawn_media<M: media::Media>(
        &mut self,
        idle_timeout: Option<std::time::Duration>,
        media: M,
    ) -> Result<MediaHandle<MF>, crate::error::Error> {
        self.run(move |_, ctx| ctx.spawn_media(idle_timeout, media))
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))
    }

    /// Gets a media that was spawned before and is still running.
    pub async fn find_media(&mut self, media_id: media::Id) -> Option<MediaHandle<MF>> {
        self.run(move |_, ctx| ctx.find_media(media_id))
            .await
            .ok()
            .flatten()
    }

    /// Signals to the server that this media factory had a fatal error.
    ///
    /// This shuts down the media factory and all currently spawned medias.
    pub async fn error(&mut self) -> Result<(), crate::error::Error> {
        self.run(move |_, ctx| ctx.error())
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))
    }

    /// Shuts down this media factory.
    ///
    /// This also shuts down all currently spawned medias.
    pub async fn shutdown(&mut self) -> Result<(), crate::error::Error> {
        self.run(move |_, ctx| ctx.shutdown())
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))
    }

    /// Spawn a new future to be executed on the media factory's task.
    pub fn spawn<
        Fut,
        T: Send + 'static,
        F: FnOnce(&mut MF, &mut Context<MF>) -> Fut + Send + 'static,
    >(
        &mut self,
        func: F,
    ) -> impl Future<Output = Result<T, mpsc::SendError>> + Send
    where
        Fut: Future<Output = T> + Send + 'static,
    {
        use future::Either;

        let (sender, receiver) = oneshot::channel();

        trace!("Media Factory {}: Spawning media factory future", self.id);
        let func: Box<
            dyn FnOnce(&mut MF, &mut Context<MF>) -> Pin<Box<dyn Future<Output = ()> + Send>>
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

        if let Err(err) = self
            .sender
            .try_send(MediaFactoryMessage::MediaFactoryFuture(func))
        {
            warn!("Media Factory {}: Failed to spawn future", self.id);
            Either::Left(async move { Err(err) })
        } else {
            Either::Right(async move { receiver.await.map_err(|_| mpsc::SendError::Disconnected) })
        }
    }

    /// Run a closure on the media factory's task.
    pub fn run<T: Send + 'static, F: FnOnce(&mut MF, &mut Context<MF>) -> T + Send + 'static>(
        &mut self,
        func: F,
    ) -> impl Future<Output = Result<T, mpsc::SendError>> + Send {
        use future::Either;

        let (sender, receiver) = oneshot::channel();

        trace!("MediaFactory {}: Running media factory closure", self.id);
        let func: Box<dyn FnOnce(&mut MF, &mut Context<MF>) + Send + 'static> =
            Box::new(move |media_factory, context| {
                let res = func(media_factory, context);
                let _ = sender.send(res);
            });

        if let Err(err) = self
            .sender
            .try_send(MediaFactoryMessage::MediaFactoryClosure(func))
        {
            warn!("MediaFactory {}: Failed to media_factory closure", self.id);
            Either::Left(async move { Err(err) })
        } else {
            Either::Right(async move { receiver.await.map_err(|_| mpsc::SendError::Disconnected) })
        }
    }
}

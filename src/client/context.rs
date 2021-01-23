use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_std::task;

use futures::prelude::*;

use log::{debug, error, info, trace, warn};

use crate::body::Body;
use crate::channel::{mpsc, oneshot};
use crate::listener::ConnectionInformation;
use crate::media;
use crate::media_factory;
use crate::server;
use crate::stream_handler;
use crate::typemap::TypeMap;

use super::interleaved::*;
use super::messages::*;
use super::{Client, Id};

/// `Send`-able Client handle.
pub struct Handle<C: Client + ?Sized> {
    id: Id,
    sender: mpsc::Sender<ClientMessage<C>>,
    connection_information: ConnectionInformation,
}

impl<C: Client + ?Sized> Clone for Handle<C> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            sender: self.sender.clone(),
            connection_information: self.connection_information.clone(),
        }
    }
}

pub(super) enum PipelinedRequest<C: Client + ?Sized> {
    Waiting(
        Vec<
            oneshot::Sender<
                Result<
                    (MediaHandle<C>, server::PresentationURI, server::SessionId),
                    crate::error::Error,
                >,
            >,
        >,
    ),
    Registered(server::SessionId),
}

pub struct Context<C: Client + ?Sized> {
    pub(super) id: Id,

    pub(super) connection_information: ConnectionInformation,

    /// Sender for sending RTSP messages to the peer
    pub(super) rtsp_sender: mpsc::Sender<RtspSendMessage>,
    /// Server controller.
    pub(super) server_controller: server::Controller<server::controller::Client>,
    /// Sender for sending internal messages to this client's task
    pub(super) client_sender: mpsc::Sender<ClientMessage<C>>,
    pub(super) controller_sender: mpsc::Sender<ControllerMessage>,

    /// Registration for custom client streams
    pub(super) stream_registration: stream_handler::StreamRegistration<C, Self>,

    /// CSeq counter for requests we send out
    pub(super) cseq_counter: u32,

    /// Requests we sent where we are waiting for responses, indexed by CSeq
    pub(super) pending_responses: HashMap<u32, mpsc::Sender<rtsp_types::Response<Body>>>,

    /// Requests we received for which we're still waiting to create a response, indexed by CSeq
    pub(super) pending_requests: HashMap<u32, (task::JoinHandle<()>, future::AbortHandle)>,

    /// Sessions of this client.
    ///
    /// Media id, presentation URI, possible pipelined requests and interleaved channel ids.
    pub(super) sessions:
        HashMap<server::SessionId, (media::Id, server::PresentationURI, Option<u32>, Vec<u8>)>,

    /// Pipelined requests of this client.
    pub(super) pipelined_requests: HashMap<u32, PipelinedRequest<C>>,

    /// Currently available session medias.
    pub(super) session_medias: HashMap<
        media::Id,
        (
            media::Controller<media::controller::Client>,
            server::SessionId,
        ),
    >,

    /// Currently allocated interleaved channel ids.
    pub(super) interleaved_channels: HashMap<u8, (server::SessionId, Box<dyn DataReceiver>)>,
}

/// Client handle to a media.
pub struct MediaHandle<C: Client + ?Sized> {
    controller: media::Controller<media::controller::Client>,
    sender: mpsc::Sender<ControllerMessage>,
    handle: Handle<C>,
}

impl<C: Client + ?Sized> Clone for MediaHandle<C> {
    fn clone(&self) -> Self {
        Self {
            controller: self.controller.clone(),
            sender: self.sender.clone(),
            handle: self.handle.clone(),
        }
    }
}

impl<C: Client + ?Sized> MediaHandle<C> {
    pub fn media_id(&self) -> media::Id {
        self.controller.media_id()
    }

    pub async fn find_media_factory(
        &mut self,
    ) -> Result<MediaFactoryHandle<C>, crate::error::Error> {
        trace!(
            "Client {}: Getting media factory for media {}",
            self.controller.client_id(),
            self.controller.media_id(),
        );

        match self.controller.find_media_factory().await {
            Ok(media_factory_controller) => {
                trace!(
                    "Client {}: Got media factory {} for media {}",
                    self.controller.client_id(),
                    media_factory_controller.media_factory_id(),
                    self.controller.media_id(),
                );

                Ok(MediaFactoryHandle {
                    controller: media_factory_controller,
                    sender: self.sender.clone(),
                    handle: self.handle.clone(),
                })
            }
            Err(err) => {
                trace!(
                    "Client {}: Got media factory for media {}: Err({:?})",
                    self.controller.client_id(),
                    self.controller.media_id(),
                    err,
                );
                Err(err)
            }
        }
    }

    pub async fn options(
        &mut self,
        stream_id: Option<media::StreamId>,
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
            "Client {}: Getting OPTIONS for media {} with stream id {:?} supported {:?} require {:?}",
            self.controller.client_id(),
            self.controller.media_id(),
            stream_id,
            supported,
            require
        );
        let res = self
            .controller
            .options(stream_id, supported, require, extra_data)
            .await;
        trace!(
            "Client {}: Got OPTIONS for media {}: {:?}",
            self.controller.client_id(),
            self.controller.media_id(),
            res
        );

        res
    }

    pub async fn describe(
        &mut self,
        extra_data: TypeMap,
    ) -> Result<(sdp_types::Session, TypeMap), crate::error::Error> {
        trace!(
            "Client {}: Getting session description for media {}",
            self.controller.client_id(),
            self.controller.media_id()
        );
        let res = self.controller.describe(extra_data).await;
        trace!(
            "Client {}: Got session description for media {}: {:?}",
            self.controller.client_id(),
            self.controller.media_id(),
            res
        );

        res
    }

    pub async fn add_transport(
        &mut self,
        session_id: server::SessionId,
        stream_id: media::StreamId,
        transports: rtsp_types::headers::Transports,
        extra_data: TypeMap,
    ) -> Result<media::ConfiguredTransport, crate::error::Error> {
        trace!(
            "Client {}: Adding transport {:?} to media {} with session {} and stream {}",
            self.controller.client_id(),
            transports,
            self.controller.media_id(),
            session_id,
            stream_id
        );

        let media_id = self.media_id();
        let session_id_clone = session_id.clone();
        self.handle
            .run(move |_client, ctx| {
                if let Some((session_media_id, _, _, _)) = ctx.sessions.get(&session_id_clone) {
                    if *session_media_id == media_id {
                        Ok(())
                    } else {
                        warn!(
                            "Client {}: Media {} not in session {}",
                            ctx.id(),
                            media_id,
                            session_id_clone
                        );
                        Err(crate::error::Error::from(crate::error::InternalServerError))
                    }
                } else {
                    warn!("Client {}: Not in session {}", ctx.id(), session_id_clone);
                    Err(crate::error::Error::from(crate::error::ErrorStatus::from(
                        rtsp_types::StatusCode::SessionNotFound,
                    )))
                }
            })
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))??;

        let res = self
            .controller
            .add_transport(session_id.clone(), stream_id, transports, extra_data)
            .await;

        trace!(
            "Client {}: Added transport to media {} with session {}: {:?}",
            self.controller.client_id(),
            self.controller.media_id(),
            session_id,
            res
        );

        res
    }

    pub async fn remove_transport(
        &mut self,
        session_id: server::SessionId,
        stream_id: media::StreamId,
        extra_data: TypeMap,
    ) -> Result<(), crate::error::Error> {
        trace!(
            "Client {}: Removing transport for session {} and stream {} from media {}",
            self.controller.client_id(),
            session_id,
            stream_id,
            self.controller.media_id()
        );

        let media_id = self.media_id();
        let session_id_clone = session_id.clone();
        self.handle
            .run(move |_client, ctx| {
                if let Some((session_media_id, _, _, _)) = ctx.sessions.get(&session_id_clone) {
                    if *session_media_id == media_id {
                        Ok(())
                    } else {
                        warn!(
                            "Client {}: Media {} not in session {}",
                            ctx.id(),
                            media_id,
                            session_id_clone
                        );
                        Err(crate::error::Error::from(crate::error::InternalServerError))
                    }
                } else {
                    warn!("Client {}: Not in session {}", ctx.id(), session_id_clone);
                    Err(crate::error::Error::from(crate::error::ErrorStatus::from(
                        rtsp_types::StatusCode::SessionNotFound,
                    )))
                }
            })
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))??;

        let res = self
            .controller
            .remove_transport(session_id, stream_id, extra_data)
            .await;

        trace!(
            "Client {}: Removed transport from media {}: {:?}",
            self.controller.client_id(),
            self.controller.media_id(),
            res
        );

        res
    }

    pub async fn play(
        &mut self,
        session_id: server::SessionId,
        stream_id: Option<media::StreamId>,
        range: Option<rtsp_types::headers::Range>,
        extra_data: TypeMap,
    ) -> Result<
        (
            rtsp_types::headers::Range,
            rtsp_types::headers::RtpInfos,
            TypeMap,
        ),
        crate::error::Error,
    > {
        trace!(
            "Client {}: Play for media {} with session {} and range {:?}",
            self.controller.client_id(),
            self.controller.media_id(),
            session_id,
            range
        );

        let media_id = self.media_id();
        let session_id_clone = session_id.clone();
        self.handle
            .run(move |_client, ctx| {
                if let Some((session_media_id, _, _, _)) = ctx.sessions.get(&session_id_clone) {
                    if *session_media_id == media_id {
                        Ok(())
                    } else {
                        warn!(
                            "Client {}: Media {} not in session {}",
                            ctx.id(),
                            media_id,
                            session_id_clone
                        );
                        Err(crate::error::Error::from(crate::error::InternalServerError))
                    }
                } else {
                    warn!("Client {}: Not in session {}", ctx.id(), session_id_clone);
                    Err(crate::error::Error::from(crate::error::ErrorStatus::from(
                        rtsp_types::StatusCode::SessionNotFound,
                    )))
                }
            })
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))??;

        let res = self
            .controller
            .play(session_id.clone(), stream_id, range, extra_data)
            .await;

        trace!(
            "Client {}: Played for media {} with session {}: {:?}",
            self.controller.client_id(),
            self.controller.media_id(),
            session_id,
            res
        );

        res
    }

    pub async fn pause(
        &mut self,
        session_id: server::SessionId,
        stream_id: Option<media::StreamId>,
        extra_data: TypeMap,
    ) -> Result<(rtsp_types::headers::Range, TypeMap), crate::error::Error> {
        trace!(
            "Client {}: Paused for media {} with session {}",
            self.controller.client_id(),
            self.controller.media_id(),
            session_id
        );

        let media_id = self.media_id();
        let session_id_clone = session_id.clone();
        self.handle
            .run(move |_client, ctx| {
                if let Some((session_media_id, _, _, _)) = ctx.sessions.get(&session_id_clone) {
                    if *session_media_id == media_id {
                        Ok(())
                    } else {
                        warn!(
                            "Client {}: Media {} not in session {}",
                            ctx.id(),
                            media_id,
                            session_id_clone
                        );
                        Err(crate::error::Error::from(crate::error::InternalServerError))
                    }
                } else {
                    warn!("Client {}: Not in session {}", ctx.id(), session_id_clone);
                    Err(crate::error::Error::from(crate::error::ErrorStatus::from(
                        rtsp_types::StatusCode::SessionNotFound,
                    )))
                }
            })
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))??;

        let res = self
            .controller
            .pause(session_id.clone(), stream_id, extra_data)
            .await;

        trace!(
            "Client {}: Plaued for media {} with session {}: {:?}",
            self.controller.client_id(),
            self.controller.media_id(),
            session_id,
            res
        );

        res
    }
}

/// Client handle to a media factory.
pub struct MediaFactoryHandle<C: Client + ?Sized> {
    controller: media_factory::Controller<media_factory::controller::Client>,
    sender: mpsc::Sender<ControllerMessage>,
    handle: Handle<C>,
}

impl<C: Client + ?Sized> Clone for MediaFactoryHandle<C> {
    fn clone(&self) -> Self {
        Self {
            controller: self.controller.clone(),
            sender: self.sender.clone(),
            handle: self.handle.clone(),
        }
    }
}

impl<C: Client + ?Sized> MediaFactoryHandle<C> {
    pub fn media_factory_id(&self) -> media_factory::Id {
        self.controller.media_factory_id()
    }

    /// Query the media factory for OPTIONS on the given URI.
    pub async fn options(
        &mut self,
        stream_id: Option<media::StreamId>,
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
        trace!(
            "Client {}: Getting OPTIONS for media factory {} with stream ID {:?} supported {:?} require {:?}",
            self.controller.client_id(),
            self.controller.media_factory_id(),
            stream_id,
            supported,
            require
        );
        let res = self
            .controller
            .options(stream_id, supported, require, extra_data)
            .await;
        trace!(
            "Client {}: Got OPTIONS for media factory {}: {:?}",
            self.controller.client_id(),
            self.controller.media_factory_id(),
            res
        );

        res
    }

    /// Query the media factory for a session description on the given URI.
    pub async fn describe(
        &mut self,
        extra_data: TypeMap,
    ) -> Result<(sdp_types::Session, TypeMap), crate::error::Error> {
        trace!(
            "Client {}: Getting session description for media factory {}",
            self.controller.client_id(),
            self.controller.media_factory_id(),
        );
        let res = self.controller.describe(extra_data).await;
        trace!(
            "Client {}: Got session description for media factory {}: {:?}",
            self.controller.client_id(),
            self.controller.media_factory_id(),
            res
        );

        res
    }

    /// Create a new media from the media factory for the given URI.
    pub async fn create_media(
        &mut self,
        extra_data: TypeMap,
    ) -> Result<(MediaHandle<C>, TypeMap), crate::error::Error> {
        let (media_controller, extra_data) = self.controller.create_media(extra_data).await?;

        Ok((
            MediaHandle {
                controller: media_controller,
                sender: self.sender.clone(),
                handle: self.handle.clone(),
            },
            extra_data,
        ))
    }
}

impl<C: Client + ?Sized> Context<C> {
    /// Return this client's id.
    pub fn id(&self) -> Id {
        self.id
    }

    /// Return this client's connection information.
    pub fn connection_information(&self) -> ConnectionInformation {
        self.connection_information.clone()
    }

    /// Returns a new default client extra data.
    pub fn default_extra_data(&self) -> TypeMap {
        let mut extra_data = TypeMap::default();
        extra_data.insert(self.connection_information());
        extra_data
    }

    /// Returns a new default client extra data for the request.
    pub fn default_extra_data_for_request(&self, req: &super::OriginalRequest) -> TypeMap {
        let mut extra_data = self.default_extra_data();
        extra_data.insert(req.clone());
        extra_data
    }

    /// Send a custom `Request` to the peer.
    pub fn send_request(
        &mut self,
        mut request: rtsp_types::Request<Body>,
    ) -> impl Future<Output = Result<rtsp_types::Response<Body>, crate::error::Error>> + Send {
        use future::Either;

        use rtsp_types::headers::CSeq;

        trace!("Client {}: Sending request {:?}", self.id, request);

        let cseq = self.cseq_counter;
        request.insert_typed_header(&CSeq::from(cseq));
        self.cseq_counter += 1;

        // TODO: Make configurable
        request.insert_header(
            rtsp_types::headers::SERVER,
            "GStreamer RTSP Server/0.0".to_string(),
        );
        {
            use chrono::prelude::*;
            let date = Local::now();
            request.insert_header(rtsp_types::headers::DATE, date.to_rfc2822());
        }

        let (response_sender, mut response_receiver) = mpsc::channel();
        self.pending_responses.insert(cseq, response_sender);

        if let Err(err) = self
            .rtsp_sender
            .try_send(RtspSendMessage(request.into(), None))
        {
            match err {
                mpsc::SendError::Full => {
                    warn!("Client {}: RTSP sender full", self.id);
                    let _ = self.client_sender.send(ClientMessage::SenderFull);
                }
                mpsc::SendError::Disconnected => {
                    debug!("Client {}: Disconnected", self.id);
                }
            }
            self.pending_responses.remove(&cseq);

            return Either::Left(async { Err(crate::error::InternalServerError.into()) });
        }

        // FIXME: Above and below, better error return

        // Make sure to send a message back to the client in
        // all circumstances when the task below is dropped,
        // even on panic so that we don't accumulate useless
        // responses.
        struct RemoveResponseOnDrop<C: Client + ?Sized> {
            id: Id,
            client_sender: mpsc::Sender<ClientMessage<C>>,
            cseq: u32,
        }
        impl<C: Client + ?Sized> Drop for RemoveResponseOnDrop<C> {
            fn drop(&mut self) {
                if let Err(mpsc::SendError::Full) = self
                    .client_sender
                    .try_send(ClientMessage::RequestFinished(self.cseq))
                {
                    error!(
                        "Client {}: Can't finish request for CSeq {}, sender full",
                        self.id, self.cseq
                    );
                }
            }
        }
        let drop_guard = RemoveResponseOnDrop {
            id: self.id,
            client_sender: self.client_sender.clone(),
            cseq,
        };

        // Spawn a separate task and only return the join handle so that
        // we don't depend on the caller to do something with the returned
        // future for handling the response.
        let id = self.id;
        let mut client_sender = self.client_sender.clone();
        let request_task = task::spawn(async move {
            let _drop_guard = drop_guard;
            loop {
                use async_std::future::timeout;

                match timeout(std::time::Duration::from_secs(10), response_receiver.next()).await {
                    Err(_) => {
                        warn!("Client {}: Request with CSeq {} timed out", id, cseq);
                        let _ = client_sender
                            .send(ClientMessage::RequestFinished(cseq))
                            .await;
                        return Err(crate::error::InternalServerError.into());
                    }
                    Ok(None) => {
                        warn!(
                            "Client {}: Request with CSeq {} didn't finish before disconnect",
                            id, cseq
                        );
                        let _ = client_sender
                            .send(ClientMessage::RequestFinished(cseq))
                            .await;
                        return Err(crate::error::InternalServerError.into());
                    }
                    Ok(Some(resp)) => {
                        if resp.status() == rtsp_types::StatusCode::Continue {
                            trace!("Client {}: Received Continue for CSeq {}", id, cseq);
                            continue;
                        } else {
                            trace!("Client {}: Received response {:?}", id, resp);
                            let _ = client_sender
                                .send(ClientMessage::RequestFinished(cseq))
                                .await;
                            return Ok(resp);
                        }
                    }
                }
            }
        });

        Either::Right(request_task)
    }

    /// Register an interleaved channel.
    ///
    /// This will be unregistered once the sender is closed.
    pub fn register_interleaved_channel(
        &mut self,
        session_id: server::SessionId,
        channel_id: Option<u8>,
        receivers: Vec<Box<dyn DataReceiver>>,
    ) -> Result<(u8, Vec<DataSender>), crate::error::Error> {
        let n_receivers = receivers.len();

        debug!(
            "Client {}: Registering {} interleaved channels starting at {:?} for for session {}",
            self.id, n_receivers, channel_id, session_id,
        );

        if !self.sessions.contains_key(&session_id) {
            warn!(
                "Client {}: Tried registering interleaved channels for unknown session {}",
                self.id, session_id
            );
            return Err(
                crate::error::ErrorStatus::from(rtsp_types::StatusCode::SessionNotFound).into(),
            );
        }

        if n_receivers > u8::MAX as usize {
            return Err(crate::error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest).into());
        }

        // Try if all channel ids after the requested one are free and use those in that case
        let mut selected_channel_id = None;
        if let Some(channel_id) = channel_id {
            if channel_id as usize + n_receivers > u8::MAX as usize {
                return Err(
                    crate::error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest).into(),
                );
            }

            let all_free = (channel_id..(channel_id + n_receivers as u8))
                .all(|channel_id| !self.interleaved_channels.contains_key(&channel_id));
            if all_free {
                selected_channel_id = Some(channel_id);
            }
        }

        // Otherwise try to find a block of enough free channel ids
        if selected_channel_id.is_none() {
            for channel_id in 0..=255 {
                if channel_id as usize + n_receivers > u8::MAX as usize {
                    return Err(crate::error::ErrorStatus::from(
                        rtsp_types::StatusCode::UnsupportedTransport,
                    )
                    .into());
                }

                let all_free = (channel_id..(channel_id + n_receivers as u8))
                    .all(|channel_id| !self.interleaved_channels.contains_key(&channel_id));
                if all_free {
                    selected_channel_id = Some(channel_id);
                    break;
                }
            }
        }

        // If none was found return, otherwise use it
        let selected_channel_id = selected_channel_id.ok_or_else(|| {
            crate::error::Error::from(crate::error::ErrorStatus::from(
                rtsp_types::StatusCode::UnsupportedTransport,
            ))
        })?;

        let mut senders = Vec::new();
        for (idx, receiver) in receivers.into_iter().enumerate() {
            let channel_id = selected_channel_id + idx as u8;
            self.interleaved_channels
                .insert(channel_id, (session_id.clone(), receiver));
            senders.push(DataSender {
                client_id: self.id,
                channel_id,
                close_sender: Arc::new(CloseSender {
                    client_id: self.id,
                    channel_id,
                    sender: self
                        .client_sender
                        .clone()
                        .map(move |_| ClientMessage::InterleavedChannelClosed(channel_id)),
                }),
                rtsp_sender: self.rtsp_sender.clone(),
                ack_channel: mpsc::channel(),
            });
        }

        Ok((selected_channel_id, senders))
    }

    /// Find a media factory and presentation URI for a given URI.
    pub fn find_media_factory_for_uri(
        &self,
        uri: url::Url,
        extra_data: TypeMap,
    ) -> impl Future<
        Output = Result<(MediaFactoryHandle<C>, server::PresentationURI), crate::error::Error>,
    > + Send {
        let mut server_controller = self.server_controller.clone();
        let id = self.id;
        let controller_sender = self.controller_sender.clone();
        let client_handle = self.handle();
        let fut = async move {
            let (media_factory_controller, presentation_uri) = server_controller
                .find_media_factory_for_uri(uri.clone(), extra_data)
                .await
                .map_err(|err| {
                    trace!(
                        "Client {}: Found no media factory for URI {}: {}",
                        id,
                        uri,
                        err,
                    );

                    err
                })?;

            trace!(
                "Client {}: Found media factory {} for URI {} with presentation URI {}",
                id,
                media_factory_controller.media_factory_id(),
                uri,
                presentation_uri,
            );

            Ok((
                MediaFactoryHandle {
                    controller: media_factory_controller,
                    sender: controller_sender,
                    handle: client_handle,
                },
                presentation_uri,
            ))
        };

        Box::pin(fut)
    }

    /// Registers a pipelined request.
    ///
    /// This reserves a session ID for this pipelined request.
    ///
    /// On the first call the returned future will immediately resolve and allows the caller to
    /// set up a media and actually create the session properly. On errors before creating the
    /// session, the caller should also report the error to the returned channel, or otherwise an
    /// internal server error will be assumed for other requests for the same pipelined request.
    ///
    /// All future calls will wait for the first caller to succeed creating a session or failing to
    /// do so, or immediately resolve with the already created session.
    // TODO: impl Future
    pub fn find_session_for_pipelined_request(
        &mut self,
        pipelined_request: u32,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = either::Either<
                        oneshot::Sender<Result<(), crate::error::Error>>,
                        Result<
                            (MediaHandle<C>, server::PresentationURI, server::SessionId),
                            crate::error::Error,
                        >,
                    >,
                > + Send,
        >,
    > {
        match self.pipelined_requests.get_mut(&pipelined_request) {
            None => {
                trace!(
                    "Client {}: Did not find media for pipelined request {}",
                    self.id,
                    pipelined_request
                );

                self.pipelined_requests
                    .insert(pipelined_request, PipelinedRequest::Waiting(Vec::new()));

                let (sender, receiver) = oneshot::channel();

                let mut handle = self.handle();
                task::spawn(async move {
                    let res = receiver.await;

                    let _ = handle
                        .run(move |_client, ctx| {
                            trace!(
                                "Client {}: Pipelined request {} resolved: {:?}",
                                ctx.id,
                                pipelined_request,
                                res,
                            );

                            let res = res
                                .unwrap_or_else(|_| Err(crate::error::InternalServerError.into()));

                            match ctx.pipelined_requests.remove(&pipelined_request) {
                                Some(PipelinedRequest::Waiting(waiters)) => {
                                    let res = match res {
                                        Ok(()) => {
                                            // Can't really happen unless the caller does things wrong:
                                            // the waiters would've been woken up from
                                            // create_session() already.
                                            Err(crate::error::InternalServerError.into())
                                        }
                                        Err(err) => Err(err),
                                    };

                                    for waiter in waiters {
                                        let _ = waiter.send(res.clone());
                                    }
                                }
                                Some(PipelinedRequest::Registered(_)) => {
                                    // Was successfully registered before so nothing to do here
                                }
                                _ => unreachable!(),
                            }
                        })
                        .await;
                });

                return Box::pin(async move { either::Either::Left(sender) });
            }
            Some(PipelinedRequest::Waiting(ref mut waiters)) => {
                trace!(
                    "Client {}: Found pending pipelined request {}, waiting for result",
                    self.id,
                    pipelined_request
                );

                let (sender, receiver) = oneshot::channel();
                waiters.push(sender);

                let id = self.id;
                return Box::pin(async move {
                    let res = receiver
                        .await
                        .map_err(|_| crate::error::InternalServerError.into())
                        .and_then(|res| res);

                    match res {
                        Ok((ref media_handle, ref presentation_uri, ref session_id)) => {
                            trace!(
                                "Client {}: Found media {} for pipelined request {} with session id {} and presentation URI {}",
                                id,
                                media_handle.media_id(),
                                pipelined_request,
                                session_id,
                                presentation_uri
                            );
                        }
                        Err(ref err) => {
                            trace!(
                                "Client {}: Did not find media for pipelined request {}: {:?}",
                                id,
                                pipelined_request,
                                err,
                            );
                        }
                    }

                    either::Either::Right(res)
                });
            }
            Some(PipelinedRequest::Registered(session_id)) => {
                if let Some((media_id, presentation_uri, _, _)) = self.sessions.get(session_id) {
                    let session_id = session_id.clone();
                    let presentation_uri = presentation_uri.clone();

                    let (media, _) = self.session_medias.get(&media_id).expect("media not found");
                    trace!(
                        "Client {}: Found media {} for pipelined request {} with session id {} and presentation URI {}",
                        self.id,
                        media_id,
                        pipelined_request,
                        session_id,
                        presentation_uri
                    );

                    let media_handle = MediaHandle {
                        controller: media.clone(),
                        sender: self.controller_sender.clone(),
                        handle: self.handle(),
                    };

                    return Box::pin(async move {
                        either::Either::Right(Ok((media_handle, presentation_uri, session_id)))
                    });
                } else {
                    panic!(
                        "Client {}: Did not find media for pipelined request {}",
                        self.id, pipelined_request
                    );
                }
            }
        }
    }

    /// Find an active session media for this client.
    ///
    /// Checks if the servers knows about this session and makes the current client the active
    /// client for this session if it exists and is active.
    ///
    /// Returns the media handle and presentation URI for the session.
    pub fn find_session(
        &mut self,
        session_id: &server::SessionId,
    ) -> impl Future<Output = Result<(MediaHandle<C>, server::PresentationURI), crate::error::Error>>
           + Send {
        use future::Either;

        if let Some((media_id, presentation_uri, _, _)) = self.sessions.get(session_id) {
            trace!(
                "Client {}: Found media {} for session id {} with presentation URI {}",
                self.id,
                media_id,
                session_id,
                presentation_uri,
            );

            let media_handle = self
                .session_medias
                .get(&media_id)
                .map(|c| MediaHandle {
                    controller: c.0.clone(),
                    sender: self.controller_sender.clone(),
                    handle: self.handle(),
                })
                .expect("no media");

            let presentation_uri = presentation_uri.clone();

            return Either::Left(async move { Ok((media_handle, presentation_uri)) });
        }

        let session_id = session_id.clone();
        let mut server_controller = self.server_controller.clone();
        let mut client_handle = self.handle();
        let fut = async move {
            let (media_controller, presentation_uri) = server_controller
                .find_session(session_id.clone())
                .await
                .map_err(|err| {
                    trace!(
                        "Client {}: Found no media for session {}: {}",
                        client_handle.id(),
                        session_id,
                        err,
                    );
                    err
                })?;

            client_handle
                .run(|_client, ctx| {
                    trace!(
                        "Client {}: Found media {} for session {} with presentation URI {}",
                        ctx.id(),
                        media_controller.media_id(),
                        session_id,
                        presentation_uri,
                    );

                    if let Some((media_id, old_presentation_uri, _, _)) =
                        ctx.sessions.get(&session_id)
                    {
                        assert_eq!(media_controller.media_id(), *media_id);
                        assert_eq!(presentation_uri, *old_presentation_uri);
                    } else {
                        ctx.sessions.insert(
                            session_id.clone(),
                            (
                                media_controller.media_id(),
                                presentation_uri.clone(),
                                None,
                                Vec::new(),
                            ),
                        );
                        ctx.session_medias.insert(
                            media_controller.media_id(),
                            (media_controller.clone(), session_id),
                        );
                    }

                    let client_handle = ctx.handle();
                    (
                        MediaHandle {
                            controller: media_controller,
                            sender: ctx.controller_sender.clone(),
                            handle: client_handle,
                        },
                        presentation_uri,
                    )
                })
                .await
                .map_err(|_| crate::error::InternalServerError.into())
        };

        Either::Right(fut)
    }

    /// Creates a new session for the given media.
    ///
    /// The session will be shut down automatically together with the client, explicitly by calling
    /// `Context::shutdown_session()` or when it times out.
    // TODO: Might want to be able to configure a non-60s timeout for the session
    pub fn create_session(
        &mut self,
        presentation_uri: server::PresentationURI,
        pipelined_request: Option<u32>,
        media_handle: &MediaHandle<C>,
    ) -> impl Future<Output = Result<server::SessionId, crate::error::Error>> + Send {
        let session_id = server::SessionId::new();
        let media_id = media_handle.media_id();

        trace!(
            "Client {}: Creating session {} for pipelined request {:?} with media {}",
            self.id,
            session_id,
            pipelined_request,
            media_id
        );

        if let Some(pipelined_request) = pipelined_request {
            if let Some(PipelinedRequest::Registered(existing_session_id)) =
                self.pipelined_requests.get(&pipelined_request)
            {
                panic!(
                    "Client {}: Existing session {} found for pipelined request {}",
                    self.id, existing_session_id, pipelined_request
                );
            }
        }

        self.sessions.insert(
            session_id.clone(),
            (
                media_id,
                presentation_uri.clone(),
                pipelined_request,
                Vec::new(),
            ),
        );

        self.session_medias.insert(
            media_id,
            (media_handle.controller.clone(), session_id.clone()),
        );

        let media_controller =
            media::Controller::<media::controller::Server>::from_client_controller(
                &media_handle.controller,
                session_id.clone(),
            );

        let mut client_handle = self.handle();
        let media_handle = media_handle.clone();
        let mut server_controller = self.server_controller.clone();
        let fut = async move {
            let res = server_controller
                .create_session(presentation_uri.clone(), media_controller)
                .await;

            match res {
                Ok(()) => {
                    trace!(
                        "Client {}: Created session {} for presentation URI {}",
                        server_controller.client_id(),
                        session_id,
                        presentation_uri
                    );

                    if let Some(pipelined_request) = pipelined_request {
                        let session_id = session_id.clone();
                        let _ = client_handle.run(move |_client, ctx| {
                            match ctx.pipelined_requests.remove(&pipelined_request) {
                                Some(PipelinedRequest::Registered(existing_session_id)) => {
                                    panic!(
                                        "Client {}: Existing session {} found for pipelined request {}",
                                        ctx.id, existing_session_id, pipelined_request
                                        );
                                },
                                Some(PipelinedRequest::Waiting(waiters)) => {
                                    ctx.pipelined_requests.insert(pipelined_request, PipelinedRequest::Registered(session_id.clone()));
                                    for waiter in waiters {
                                        let _ = waiter.send(Ok((media_handle.clone(), presentation_uri.clone(), session_id.clone())));
                                    }
                                }
                                _ => (),
                            }

                    }).await;
                    }

                    Ok(session_id)
                }
                Err(err) => {
                    warn!(
                        "Client {}: Failed creating session {}: {}",
                        server_controller.client_id(),
                        session_id,
                        err
                    );
                    let _ = client_handle
                        .spawn(move |client, ctx| {
                            // FIXME: Not actually timed out...
                            client.session_timed_out(ctx, session_id.clone());

                            if let Some(pipelined_request) = pipelined_request {
                                ctx.pipelined_requests.remove(&pipelined_request);
                            }

                            if let Some((_, _, _, channel_ids)) = ctx.sessions.remove(&session_id) {
                                for channel_id in channel_ids {
                                    if let Some((_, mut sender)) =
                                        ctx.interleaved_channels.remove(&channel_id)
                                    {
                                        sender.closed();
                                    }
                                }
                            }

                            let media_controller =
                                ctx.session_medias.remove(&media_id).map(|(c, _)| c);

                            async move {
                                if let Some(mut media_controller) = media_controller {
                                    let _ = media_controller
                                        .shutdown_session(session_id, TypeMap::default())
                                        .await;
                                }
                            }
                        })
                        .await;

                    Err(err)
                }
            }
        };

        fut
    }

    /// Keeps alive the session explicitly.
    ///
    /// This fails if the session does not exist or timed out in the meantime.
    pub fn keep_alive_session(
        &mut self,
        session_id: &server::SessionId,
    ) -> impl Future<Output = Result<(), crate::error::Error>> + Send {
        use future::Either;

        trace!("Client {}: Keeping alive session {}", self.id, session_id);

        if !self.sessions.contains_key(session_id) {
            trace!("Client {}: Session {} not found", self.id, session_id);
            return Either::Left(async {
                Err(crate::error::ErrorStatus::from(rtsp_types::StatusCode::SessionNotFound).into())
            });
        }

        let mut media_controller = None;
        if let Some((media_id, _, _, _)) = self.sessions.get(session_id) {
            media_controller = self.session_medias.get(&media_id).map(|(c, _)| c.clone());
        }

        let mut server_controller = self.server_controller.clone();
        let session_id = session_id.clone();

        let fut = async move {
            let res = server_controller
                .keep_alive_session(session_id.clone())
                .await;

            match res {
                Ok(()) => Ok(()),
                Err(err) => {
                    if let Some(mut media_controller) = media_controller {
                        let _ = media_controller
                            .shutdown_session(session_id, TypeMap::default())
                            .await;
                    }
                    Err(err)
                }
            }
        };

        Either::Right(fut)
    }

    /// Shut down the session explicitly.
    ///
    /// This fails if the session does not exist or timed out in the meantime.
    pub fn shutdown_session(
        &mut self,
        session_id: &server::SessionId,
    ) -> impl Future<Output = Result<(), crate::error::Error>> + Send {
        use future::Either;

        trace!("Client {}: Shutting down session {}", self.id, session_id);

        if !self.sessions.contains_key(session_id) {
            trace!("Client {}: Session {} not found", self.id, session_id);
            return Either::Left(async {
                Err(crate::error::ErrorStatus::from(rtsp_types::StatusCode::SessionNotFound).into())
            });
        }

        let mut media_controller = None;
        if let Some((media_id, _, pipelined_request, channel_ids)) =
            self.sessions.remove(session_id)
        {
            media_controller = self.session_medias.remove(&media_id).map(|(c, _)| c);

            if let Some(pipelined_request) = pipelined_request {
                self.pipelined_requests.remove(&pipelined_request);
            }

            for channel_id in channel_ids {
                if let Some((_, mut sender)) = self.interleaved_channels.remove(&channel_id) {
                    sender.closed();
                }
            }
        }

        let mut server_controller = self.server_controller.clone();
        let session_id = session_id.clone();

        let id = self.id;
        let fut = async move {
            let res = server_controller.shutdown_session(session_id.clone()).await;

            if let Some(mut media_controller) = media_controller {
                let _ = media_controller
                    .shutdown_session(session_id.clone(), TypeMap::default())
                    .await;
            }

            trace!("Client {}: Shut down session {}", id, session_id);

            res
        };

        Either::Right(fut)
    }

    /// Signals to the server that this client had a fatal error.
    ///
    /// This shuts down the client.
    pub fn error(&mut self) {
        error!("Client {}: Errored", self.id);
        let mut server_controller = self.server_controller.clone();
        let mut controller_sender = self.controller_sender.clone();
        task::spawn(async move {
            let _ = server_controller.error().await;
            controller_sender.close_channel();
        });
    }

    /// Shuts down this client.
    pub fn shutdown(&mut self) {
        info!("Client {}: Shutting down", self.id);
        self.controller_sender.close_channel();
    }

    /// Register a custom stream to be handled by this client.
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
        C: stream_handler::MessageHandler<Msg, Token, Context = Self>,
    {
        self.stream_registration.register(token, stream)
    }

    /// Returns a `Send`-able client handle that can be used to spawn futures on the client task.
    pub fn handle(&self) -> Handle<C> {
        Handle {
            id: self.id,
            sender: self.client_sender.clone(),
            connection_information: self.connection_information.clone(),
        }
    }
}

impl<C: Client + ?Sized> Handle<C> {
    /// Return this client's id.
    pub fn id(&self) -> Id {
        self.id
    }

    /// Return this client's connection information.
    pub fn connection_information(&self) -> ConnectionInformation {
        self.connection_information.clone()
    }

    /// Returns a new default client extra data.
    pub fn default_extra_data(&self) -> TypeMap {
        let mut extra_data = TypeMap::default();
        extra_data.insert(self.connection_information());
        extra_data
    }

    /// Returns a new default client extra data for the request.
    pub fn default_extra_data_for_request(&self, req: &super::OriginalRequest) -> TypeMap {
        let mut extra_data = self.default_extra_data();
        extra_data.insert(req.clone());
        extra_data
    }

    /// Send a custom `Request` to the peer.
    pub async fn send_request(
        &mut self,
        request: rtsp_types::Request<Body>,
    ) -> Result<rtsp_types::Response<Body>, crate::error::Error> {
        self.spawn(move |_, ctx| ctx.send_request(request))
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))?
    }

    /// Register an interleaved channel.
    ///
    /// This will be unregistered once the sender is closed.
    pub async fn register_interleaved_channel(
        &mut self,
        session_id: server::SessionId,
        channel_id: Option<u8>,
        receivers: Vec<Box<dyn DataReceiver>>,
    ) -> Result<(u8, Vec<DataSender>), crate::error::Error> {
        self.run(move |_, ctx| ctx.register_interleaved_channel(session_id, channel_id, receivers))
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))?
    }

    /// Find a media factory and presentation URI for a given URI.
    pub async fn find_media_factory_for_uri(
        &mut self,
        uri: url::Url,
        extra_data: TypeMap,
    ) -> Result<(MediaFactoryHandle<C>, server::PresentationURI), crate::error::Error> {
        self.spawn(move |_, ctx| ctx.find_media_factory_for_uri(uri, extra_data))
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))?
    }

    /// Registers a pipelined request.
    ///
    /// This reserves a session ID for this pipelined request.
    ///
    /// On the first call the returned future will immediately resolve and allows the caller to
    /// set up a media and actually create the session properly. On errors before creating the
    /// session, the caller should also report the error to the returned channel, or otherwise an
    /// internal server error will be assumed for other requests for the same pipelined request.
    ///
    /// All future calls will wait for the first caller to succeed creating a session or failing to
    /// do so, or immediately resolve with the already created session.
    pub async fn find_session_for_pipelined_request(
        &mut self,
        pipelined_request: u32,
    ) -> either::Either<
        oneshot::Sender<Result<(), crate::error::Error>>,
        Result<(MediaHandle<C>, server::PresentationURI, server::SessionId), crate::error::Error>,
    > {
        use either::Either::{Left, Right};

        match self
            .spawn(move |_, ctx| ctx.find_session_for_pipelined_request(pipelined_request))
            .await
        {
            Ok(Left(sender)) => Left(sender),
            Ok(Right(res)) => Right(res),
            Err(_) => Right(Err(crate::error::InternalServerError.into())),
        }
    }

    /// Find an active session media for this client.
    ///
    /// Checks if the servers knows about this session and makes the current client the active
    /// client for this session if it exists and is active.
    ///
    /// Returns the media handle and presentation URI for the session.
    pub async fn find_session(
        &mut self,
        session_id: &server::SessionId,
    ) -> Result<(MediaHandle<C>, server::PresentationURI), crate::error::Error> {
        let session_id = session_id.clone();
        self.spawn(move |_, ctx| ctx.find_session(&session_id))
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))?
    }

    /// Creates a new session for the given media.
    ///
    /// The session will be shut down automatically together with the client, explicitly by calling
    /// `Context::shutdown_session()` or when it times out.
    pub async fn create_session(
        &mut self,
        presentation_uri: server::PresentationURI,
        pipelined_request: Option<u32>,
        media_handle: &MediaHandle<C>,
    ) -> Result<server::SessionId, crate::error::Error> {
        let media_handle = media_handle.clone();
        self.spawn(move |_, ctx| {
            ctx.create_session(presentation_uri, pipelined_request, &media_handle)
        })
        .await
        .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))?
    }

    /// Keeps alive the session explicitly.
    ///
    /// This fails if the session does not exist or timed out in the meantime.
    pub async fn keep_alive_session(
        &mut self,
        session_id: &server::SessionId,
    ) -> Result<(), crate::error::Error> {
        let session_id = session_id.clone();
        self.spawn(move |_, ctx| ctx.keep_alive_session(&session_id))
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))?
    }

    /// Shut down the session explicitly.
    ///
    /// This fails if the session does not exist or timed out in the meantime.
    pub async fn shutdown_session(
        &mut self,
        session_id: &server::SessionId,
    ) -> Result<(), crate::error::Error> {
        let session_id = session_id.clone();
        self.spawn(move |_, ctx| ctx.shutdown_session(&session_id))
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))?
    }

    /// Signals to the server that this client had a fatal error.
    ///
    /// This shuts down the client.
    pub async fn error(&mut self) -> Result<(), crate::error::Error> {
        self.run(move |_, ctx| ctx.error())
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))
    }

    /// Shuts down this client.
    pub async fn shutdown(&mut self) -> Result<(), crate::error::Error> {
        self.run(move |_, ctx| ctx.shutdown())
            .await
            .map_err(|_| crate::error::Error::from(crate::error::InternalServerError))
    }

    /// Spawn a new future to be executed on the client's task.
    pub fn spawn<
        Fut,
        T: Send + 'static,
        F: FnOnce(&mut C, &mut Context<C>) -> Fut + Send + 'static,
    >(
        &mut self,
        func: F,
    ) -> impl Future<Output = Result<T, mpsc::SendError>> + Send
    where
        Fut: Future<Output = T> + Send + 'static,
    {
        use future::Either;

        let (sender, receiver) = oneshot::channel();

        trace!("Client {}: Spawning client future", self.id);
        let func: Box<
            dyn FnOnce(&mut C, &mut Context<C>) -> Pin<Box<dyn Future<Output = ()> + Send>>
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

        if let Err(err) = self.sender.try_send(ClientMessage::ClientFuture(func)) {
            warn!("Client {}: Failed to spawn future", self.id);
            Either::Left(async move { Err(err) })
        } else {
            Either::Right(async move { receiver.await.map_err(|_| mpsc::SendError::Disconnected) })
        }
    }

    /// Run a closure on the client's task.
    pub fn run<T: Send + 'static, F: FnOnce(&mut C, &mut Context<C>) -> T + Send + 'static>(
        &mut self,
        func: F,
    ) -> impl Future<Output = Result<T, mpsc::SendError>> + Send {
        use future::Either;

        let (sender, receiver) = oneshot::channel();

        trace!("Client {}: Running client closure", self.id);
        let func: Box<dyn FnOnce(&mut C, &mut Context<C>) + Send + 'static> =
            Box::new(move |client, context| {
                let res = func(client, context);
                let _ = sender.send(res);
            });

        if let Err(err) = self.sender.try_send(ClientMessage::ClientClosure(func)) {
            warn!("Client {}: Failed to client closure", self.id);
            Either::Left(async move { Err(err) })
        } else {
            Either::Right(async move { receiver.await.map_err(|_| mpsc::SendError::Disconnected) })
        }
    }
}

use futures::prelude::*;
use futures::stream::FuturesUnordered;

use std::collections::HashMap;
use std::sync::Arc;

use log::{debug, error, trace, warn};

use async_std::task;

use crate::body::Body;
use crate::channel::mpsc;
use crate::listener;
use crate::server;
use crate::stream_handler;

use super::context::Context;
use super::controller::{self, Controller};
use super::messages::*;
use super::{Client, Id};

async fn send_task<C: Client>(
    id: Id,
    mut client_sender: mpsc::Sender<ClientMessage<C>>,
    mut rtsp_sink: listener::MessageSink,
    mut rtsp_receiver: mpsc::Receiver<RtspSendMessage>,
) {
    use async_std::future::timeout;

    let duration = std::time::Duration::from_secs(10);

    while let Some(RtspSendMessage(msg, sender)) = rtsp_receiver.next().await {
        match timeout(duration, rtsp_sink.send(msg)).await {
            Err(_) => {
                warn!("Client {}: Send timeout", id);
                let _ = client_sender
                    .send(ClientMessage::SenderError(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Sender timed out",
                    )))
                    .await;
                break;
            }
            Ok(Ok(())) => {
                trace!("Client {}: Successfully sent message", id);
                if let Some(mut sender) = sender {
                    // Must never be full by design
                    if let Err(err) = sender.try_send(()) {
                        assert!(!err.is_full());
                    }
                }
            }
            Ok(Err(err)) => {
                warn!("Client {}: Send error {:?}", id, err);
                let _ = client_sender.send(ClientMessage::SenderError(err)).await;
                break;
            }
        }
    }

    debug!("Client {}: Send task finished", id);
}

async fn task_fn<C: Client>(
    mut client: C,
    mut ctx: Context<C>,
    client_receiver: mpsc::Receiver<ClientMessage<C>>,
    controller_receiver: mpsc::Receiver<ControllerMessage>,
    mut stream_handler: stream_handler::StreamHandler<C, Context<C>>,
    mut rtsp_stream: listener::MessageStream,
    send_task: task::JoinHandle<()>,
) {
    let _finish_on_drop = ctx.server_controller.finish_on_drop();

    let mut merged_stream = stream::select(
        client_receiver,
        controller_receiver
            .map(ClientMessage::Controller)
            .chain(stream::once(future::ready(ClientMessage::ControllerClosed))),
    );

    client.startup(&mut ctx);

    loop {
        use future::Either::{Left, Right};

        // Receive a message from all the possible sources
        let msg = {
            use async_std::future::timeout;

            let id = ctx.id;

            // And then the custom stream handlers
            let fut = future::select(
                merged_stream.next(),
                stream_handler.poll(&mut client, &mut ctx),
            );

            // 10s timeout but only if there is no session active
            let timeout_rtsp_stream =
                timeout(std::time::Duration::from_secs(10), rtsp_stream.next());
            pin_utils::pin_mut!(timeout_rtsp_stream);

            // Select over the above streams to get whatever result comes first
            let res = future::select(fut, timeout_rtsp_stream).await;

            // Reformat the complicated return value into something more digestable
            match res {
                // Stream handler, can't really happen
                Left((Right(_), _)) => continue,
                // Client/server stream
                Left((Left((res, _)), _)) => Left(res),
                // RTSP message stream
                Right((Ok(res), _)) => Right(res),
                Right((Err(_), _)) => {
                    warn!("Client {}: Timed out", id);
                    // TODO: Check if a session is actually active on this client and in
                    // that case `continue`
                    //
                    // Convert into a timeout error from the RTSP stream below
                    Right(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Read timed out",
                    )
                    .into())))
                }
            }
        };

        match msg {
            Left(Some(ClientMessage::ControllerClosed)) => {
                debug!("Controller closed, shutting down");
                // TODO: Send teardown and wait for response or 10s timeout while handling other messages, then break
                break;
            }
            Left(Some(ClientMessage::InterleavedChannelClosed(channel_id))) => {
                if let Some((session_id, mut sender)) = ctx.interleaved_channels.remove(&channel_id)
                {
                    debug!(
                        "Client {}: Interleaved channel {} of session {} closed",
                        ctx.id, channel_id, session_id
                    );
                    sender.closed();
                } else {
                    warn!(
                        "Client {}: Unknown interleaved channel {} closed",
                        ctx.id, channel_id
                    );
                }
            }
            Left(Some(ClientMessage::Controller(ControllerMessage::Server(msg)))) => {
                match msg {
                    ServerMessage::Quit => {
                        debug!("Client {}: Quitting", ctx.id);
                        // TODO: Send teardown and wait for response or 10s timeout while handling other messages, then break
                        break;
                    }
                    ServerMessage::TimedOut(session_id) => {
                        if ctx.sessions.contains_key(&session_id) {
                            debug!("Client {}: Session timeout {}", ctx.id, session_id);
                            client.session_timed_out(&mut ctx, session_id.clone());
                            if let Some((media_id, _, pipelined_request, channel_ids)) =
                                ctx.sessions.remove(&session_id)
                            {
                                if let Some(pipelined_request) = pipelined_request {
                                    ctx.pipelined_requests.remove(&pipelined_request);
                                }

                                ctx.session_medias.remove(&media_id);
                                for channel_id in channel_ids {
                                    if let Some((_, mut sender)) =
                                        ctx.interleaved_channels.remove(&channel_id)
                                    {
                                        sender.closed();
                                    }
                                }
                            }
                        } else {
                            warn!("Client {}: Unknown session timeout {}", ctx.id, session_id);
                        }
                    }
                    ServerMessage::ReplacedClient(session_id) => {
                        if ctx.sessions.contains_key(&session_id) {
                            debug!("Client {}: Session client replaced {}", ctx.id, session_id);
                            client.session_replaced_client(&mut ctx, session_id.clone());
                            if let Some((media_id, _, pipelined_request, channel_ids)) =
                                ctx.sessions.remove(&session_id)
                            {
                                if let Some(pipelined_request) = pipelined_request {
                                    ctx.pipelined_requests.remove(&pipelined_request);
                                }

                                ctx.session_medias.remove(&media_id);
                                for channel_id in channel_ids {
                                    if let Some((_, mut sender)) =
                                        ctx.interleaved_channels.remove(&channel_id)
                                    {
                                        sender.closed();
                                    }
                                }
                            }
                        } else {
                            warn!(
                                "Client {}: Unknown session client replaced {}",
                                ctx.id, session_id
                            );
                        }
                    }
                }
            }
            Left(Some(ClientMessage::Controller(ControllerMessage::Media(msg)))) => match msg {
                MediaMessage::RegisterInterleavedChannel {
                    media_id,
                    session_id,
                    channel_id,
                    receivers,
                    extra_data,
                    ret,
                } => {
                    let n_receivers = receivers.len();

                    if let Some((session_media_id, _, _, _)) = ctx.sessions.get(&session_id) {
                        if media_id == *session_media_id {
                            debug!(
                                "Client {}: Media {} registering {} interleaved channels starting at {:?} for session {}",
                                ctx.id,
                                media_id,
                                n_receivers,
                                channel_id,
                                session_id,
                            );

                            let res = client.media_register_interleaved_channel(
                                &mut ctx,
                                media_id,
                                session_id.clone(),
                                channel_id,
                                receivers,
                                extra_data,
                            );

                            debug!(
                                "Client {}: Media {} registered {} interleaved channels starting at {:?} for session {}: {:?}",
                                ctx.id,
                                media_id,
                                n_receivers,
                                channel_id,
                                session_id,
                                res,
                            );

                            let _ = ret.send(res);
                        } else {
                            warn!(
                                "Client {}: Media {} tried registering interleaved channels for session {} but media {} owns session",
                                ctx.id,
                                media_id,
                                session_id,
                                session_media_id,
                            );
                            let _ = ret.send(Err(crate::error::InternalServerError.into()));
                        }
                    } else {
                        warn!(
                            "Client {}: Media {} tried registering interleaved channels for unknown session {}",
                            ctx.id,
                            media_id,
                            session_id,
                        );
                        let _ = ret.send(Err(crate::error::ErrorStatus::from(
                            rtsp_types::StatusCode::SessionNotFound,
                        )
                        .into()));
                    }
                }
                MediaMessage::PlayNotify(media_id, session_id, play_notify) => {
                    if let Some((session_media_id, _, _, _)) = ctx.sessions.get(&session_id) {
                        if media_id == *session_media_id {
                            debug!(
                                "Client {}: Media {} session {} play notify message {:?}",
                                ctx.id, media_id, session_id, play_notify
                            );
                            client.media_play_notify(&mut ctx, media_id, session_id, play_notify);
                        } else {
                            warn!(
                                "Client {}: Media {} session {} play notify message {:?} but session owned by media {}",
                                ctx.id, media_id, session_id, play_notify, session_media_id,
                            );
                        }
                    } else {
                        warn!(
                            "Client {}: Media {} session {} play notify message {:?} for unknown session",
                            ctx.id, media_id, session_id, play_notify
                        );
                    }
                }
                MediaMessage::Error(media_id) => {
                    if ctx.session_medias.contains_key(&media_id) {
                        error!("Client {}: Media {} error", ctx.id, media_id);
                        client.media_error(&mut ctx, media_id);
                    } else {
                        error!("Client {}: Unknown media {} error", ctx.id, media_id);
                    }

                    if let Some((_, session_id)) = ctx.session_medias.remove(&media_id) {
                        if let Some((_, _, pipelined_request, channel_ids)) =
                            ctx.sessions.remove(&session_id)
                        {
                            if let Some(pipelined_request) = pipelined_request {
                                ctx.pipelined_requests.remove(&pipelined_request);
                            }

                            for channel_id in channel_ids {
                                if let Some((_, mut sender)) =
                                    ctx.interleaved_channels.remove(&channel_id)
                                {
                                    sender.closed();
                                }
                            }
                        }
                    }
                }
                MediaMessage::Finished(media_id) => {
                    if ctx.session_medias.contains_key(&media_id) {
                        debug!("Client {}: Media {} finished", ctx.id, media_id);
                        client.media_finished(&mut ctx, media_id);
                    } else {
                        warn!("Client {}: Unknown media {} finished", ctx.id, media_id);
                    }

                    if let Some((_, session_id)) = ctx.session_medias.remove(&media_id) {
                        if let Some((_, _, pipelined_request, channel_ids)) =
                            ctx.sessions.remove(&session_id)
                        {
                            if let Some(pipelined_request) = pipelined_request {
                                ctx.pipelined_requests.remove(&pipelined_request);
                            }

                            for channel_id in channel_ids {
                                if let Some((_, mut sender)) =
                                    ctx.interleaved_channels.remove(&channel_id)
                                {
                                    sender.closed();
                                }
                            }
                        }
                    }
                }
            },
            Left(Some(ClientMessage::Controller(ControllerMessage::App(_)))) => {
                // TODO
                todo!()
            }
            Left(Some(ClientMessage::ClientFuture(func))) => {
                trace!("Client {}: Handling client future", ctx.id);
                let fut = func(&mut client, &mut ctx);
                task::spawn(fut);
            }
            Left(Some(ClientMessage::ClientClosure(func))) => {
                trace!("Client {}: Handling client closure", ctx.id);
                func(&mut client, &mut ctx);
            }
            Left(Some(ClientMessage::SenderError(err))) => {
                warn!("Client {}: RTSP sender error {:?}", ctx.id, err);
                // TODO: Something to shut down?
                break;
            }
            Left(Some(ClientMessage::SenderFull)) => {
                warn!("Client {}: RTSP sender full", ctx.id);
                // TODO: Something to shut down?
                break;
            }
            Left(Some(ClientMessage::RequestFinished(cseq))) => {
                // Can be called multiple times
                if ctx.pending_responses.remove(&cseq).is_some() {
                    trace!("Client {}: Request with CSeq {} finished", ctx.id, cseq);
                }
            }
            Left(Some(ClientMessage::ResponseFinished(cseq))) => {
                // Can be called multiple times
                if ctx.pending_requests.remove(&cseq).is_some() {
                    trace!("Client {}: Response with CSeq {} finished", ctx.id, cseq);
                }
            }
            Left(None) => {
                // Can't really happen
                trace!("Client {}: Internal streams closed", ctx.id);
                break;
            }
            Right(Some(Ok(rtsp_types::Message::Request(req)))) => {
                use rtsp_types::headers::CSeq;

                let req = super::OriginalRequest(Arc::new(req));

                trace!("Client {}: Received request {:?}", ctx.id, req);

                let (cseq, session_id) = match (
                    req.typed_header::<CSeq>(),
                    req.typed_header::<rtsp_types::headers::Session>(),
                ) {
                    // The Session header must not be included in DESCRIBE requests
                    (Ok(Some(cseq)), Ok(None)) if req.method() == rtsp_types::Method::Describe => {
                        (*cseq, None)
                    }
                    (Ok(Some(cseq)), Ok(session_id))
                        if req.method() != rtsp_types::Method::Describe =>
                    {
                        (*cseq, session_id)
                    }
                    _ => {
                        warn!(
                            "Client {}: No valid CSeq in request or invalid Session",
                            ctx.id
                        );

                        let mut resp = rtsp_types::Response::builder(
                            req.version(),
                            rtsp_types::StatusCode::BadRequest,
                        )
                        .build(Body::default());

                        // TODO: Make configurable
                        resp.insert_header(
                            rtsp_types::headers::SERVER,
                            "GStreamer RTSP Server/0.0".to_string(),
                        );

                        {
                            use chrono::prelude::*;
                            let date = Local::now();
                            resp.insert_header(rtsp_types::headers::DATE, date.to_rfc2822());
                        }

                        trace!("Client {}: Sending response {:?}", ctx.id, resp);

                        if let Err(err) =
                            ctx.rtsp_sender.try_send(RtspSendMessage(resp.into(), None))
                        {
                            if err.is_full() {
                                warn!("Client {}: RTSP sender full", ctx.id);
                                let _ = ctx.client_sender.send(ClientMessage::SenderFull).await;
                            }

                            break;
                        }

                        continue;
                    }
                };

                if let Some(rtsp_types::headers::Session(ref session_id, _)) = session_id {
                    if let Some((session_id, _)) = ctx.sessions.get_key_value(session_id.as_str()) {
                        let mut server_controller = ctx.server_controller.clone();
                        let session_id = session_id.clone();
                        task::spawn(async move {
                            let _ = server_controller.keep_alive_session(session_id).await;
                        });
                    }
                }

                // Make sure to send a message back to the client in
                // all circumstances when the task below is dropped,
                // even on panic so that we don't accumulate useless
                // requests.
                struct RemoveRequestOnDrop<C: Client> {
                    id: Id,
                    client_sender: mpsc::Sender<ClientMessage<C>>,
                    cseq: u32,
                }
                impl<C: Client> Drop for RemoveRequestOnDrop<C> {
                    fn drop(&mut self) {
                        if let Err(mpsc::SendError::Full) = self
                            .client_sender
                            .try_send(ClientMessage::ResponseFinished(self.cseq))
                        {
                            error!(
                                "Client {}: Can't finish response for CSeq {}, sender full",
                                self.id, self.cseq
                            );
                        }
                    }
                }
                let drop_guard = RemoveRequestOnDrop {
                    id: ctx.id,
                    client_sender: ctx.client_sender.clone(),
                    cseq,
                };

                let mut session_known = session_id
                    .as_ref()
                    .map(|session_id| ctx.sessions.contains_key(session_id.0.as_str()));

                let mut handle = ctx.handle();
                let version = req.version();
                let id = ctx.id;
                let mut resp = client.handle_request(&mut ctx, req.clone());
                let mut rtsp_sender = ctx.rtsp_sender.clone();
                let mut client_sender = ctx.client_sender.clone();
                let (abort_handle, abort_registration) = future::AbortHandle::new_pair();
                let request_task = task::spawn(
                    future::Abortable::new(
                        async move {
                            let _drop_guard = drop_guard;

                            use async_std::future::timeout;

                            loop {
                                // Session not known here yet but the client sent one, so let's check
                                // if the server knows about the session first and otherwise directly
                                // error out.
                                let resp = if session_known == Some(false) {
                                    if let Some(ref session_id) = session_id {
                                        debug!("Client {}: Checking if session {} is known to the server", id, session_id.as_ref());
                                        let session_id = server::SessionId::from(session_id.as_ref());
                                        if handle.find_session(&session_id).await.is_err() {
                                            warn!("Client {}: Session {} is not known to the server", id, session_id.as_ref());
                                            let resp = rtsp_types::Response::builder(
                                                version,
                                                rtsp_types::StatusCode::SessionNotFound,
                                            )
                                                .header(rtsp_types::headers::SESSION, session_id.as_ref())
                                                .build(crate::body::Body::default());

                                            Ok(Ok(resp))
                                        } else {
                                            // Remember that the session is known so we don't have to check
                                            // again
                                            session_known = Some(true);
                                            continue;
                                        }
                                    } else {
                                        // Remember that the session is known so we don't have to check
                                        // again
                                        session_known = Some(true);
                                        continue;
                                    }
                                } else {
                                    // Send a Continue response every 5s to prevent timeout
                                    timeout(std::time::Duration::from_secs(5), &mut resp).await
                                };

                                // Client must return a non-Continue response
                                if let Ok(Ok(ref resp)) = resp {
                                    assert_ne!(resp.status(), rtsp_types::StatusCode::Continue);
                                }

                                // Convert timeout into a Continue response
                                let resp = resp.unwrap_or_else(|_| {
                                    let empty_body: &[u8] = &[];
                                    // TODO: Maybe include something more in the response
                                    let resp = rtsp_types::Response::builder(
                                        version,
                                        rtsp_types::StatusCode::Continue,
                                    )
                                    .build(empty_body.into());

                                    Ok(resp)
                                });

                                let mut resp = resp.unwrap_or_else(|err| {
                                    let empty_body: &[u8] = &[];
                                    // TODO: Maybe include something more in the response
                                    rtsp_types::Response::builder(version, err.status_code())
                                        .build(empty_body.into())
                                });

                                // Insert session header into the response if the request had one
                                // but only for non-DESCRIBE requests as required by the RFC, and
                                // also don't include it if this was a teardown or error and the session
                                // does not exist afterwards.
                                if let Some(ref session_id) = session_id {
                                    if req.method() != rtsp_types::Method::Describe {
                                        let session_id = server::SessionId::from(session_id.as_ref());
                                        if (!resp.status().is_client_error() &&
                                            resp.status().is_server_error() &&
                                                req.method() != rtsp_types::Method::Teardown) ||
                                                handle.find_session(&session_id).await.is_ok() {
                                            resp.insert_header(rtsp_types::headers::SESSION, session_id.as_ref());
                                        }
                                    }
                                }

                                resp.insert_typed_header(&CSeq::from(cseq));
                                // TODO: Make configurable
                                resp.insert_header(
                                    rtsp_types::headers::SERVER,
                                    "GStreamer RTSP Server/0.0".to_string(),
                                );
                                {
                                    use chrono::prelude::*;
                                    let date = Local::now();
                                    resp.insert_header(
                                        rtsp_types::headers::DATE,
                                        date.to_rfc2822(),
                                    );
                                }

                                trace!("Client {}: Sending response {:?}", id, resp);

                                let was_continue =
                                    resp.status() == rtsp_types::StatusCode::Continue;

                                if let Err(err) =
                                    rtsp_sender.try_send(RtspSendMessage(resp.into(), None))
                                {
                                    if err.is_full() {
                                        warn!("Client {}: RTSP sender full", id);
                                        let _ = client_sender.send(ClientMessage::SenderFull).await;
                                    }

                                    break;
                                }

                                if !was_continue {
                                    let _ = client_sender
                                        .send(ClientMessage::ResponseFinished(cseq))
                                        .await;
                                    break;
                                }
                            }
                        },
                        abort_registration,
                    )
                    .map(|_| ()),
                );

                ctx.pending_requests
                    .insert(cseq, (request_task, abort_handle));
            }
            Right(Some(Ok(rtsp_types::Message::Response(resp)))) => {
                use rtsp_types::headers::CSeq;

                trace!("Client {}: Received response {:?}", ctx.id, resp);

                match resp.typed_header::<CSeq>() {
                    Ok(Some(cseq)) => {
                        if let Some(pending) = ctx.pending_responses.get_mut(&cseq) {
                            // If disconnected, nothing to do here. Can't really
                            // ever be full unless something goes very wrong.
                            if pending.send(resp).await.is_err() {
                                ctx.pending_responses.remove(&cseq);
                            }
                        } else {
                            debug!("Client {}: Response with unknown CSeq {}", ctx.id, *cseq);
                        }
                    }
                    _ => {
                        debug!("Client {}: Response with invalid CSeq", ctx.id);
                    }
                }
            }
            Right(Some(Ok(rtsp_types::Message::Data(data)))) => {
                trace!(
                    "Client {}: Received data message for channel {}",
                    ctx.id,
                    data.channel_id()
                );

                if let Some((session_id, ref mut receiver)) =
                    ctx.interleaved_channels.get_mut(&data.channel_id())
                {
                    let mut server_controller = ctx.server_controller.clone();
                    let session_id = session_id.clone();
                    task::spawn(async move {
                        let _ = server_controller.keep_alive_session(session_id).await;
                    });

                    receiver.handle_data(data);
                }
            }
            Right(Some(Err(err))) => {
                warn!("Client {}: Receive error {}", ctx.id, err);
                break;
            }
            Right(None) => {
                debug!("Client {}: Disconnected", ctx.id);
                break;
            }
        }
    }

    debug!("Client {}: Shutting down", ctx.id);
    ctx.stream_registration.close();
    drop(merged_stream);

    client.shutdown(&mut ctx).await;

    let mut close_tasks =
        FuturesUnordered::<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>>::new();

    let mut server_controller = ctx.server_controller.clone();
    close_tasks.push(Box::pin(async move {
        let _ = server_controller.finished().await;
    }));

    for (_, mut pending) in ctx.pending_responses {
        pending.close_channel();
    }

    for (_, (join_handle, abort_handle)) in ctx.pending_requests {
        close_tasks.push(Box::pin(join_handle));
        abort_handle.abort();
    }

    close_tasks.push(Box::pin(async move {
        let _ = send_task.await;
    }));

    ctx.rtsp_sender.close_channel();

    while let Some(_) = close_tasks.next().await {}

    debug!("Client {}: Finished shutting down", ctx.id);
}

/// Spawn and run the client task.
pub fn spawn<C: Client>(
    server_controller: server::Controller<server::controller::Client>,
    incoming_connection: listener::IncomingConnection,
    client: C,
) -> Controller<controller::Server> {
    let id = server_controller.client_id();

    let listener::IncomingConnection {
        stream: rtsp_stream,
        sink: rtsp_sink,
        connection_info,
    } = incoming_connection;

    let (rtsp_sender, rtsp_receiver) = mpsc::channel();
    let (client_sender, client_receiver) = mpsc::channel();
    let (controller_sender, controller_receiver) = mpsc::channel();

    let (stream_handler, stream_registration) = stream_handler::StreamHandler::new();

    let ctx = Context {
        id,
        connection_information: connection_info,
        rtsp_sender,
        server_controller,
        client_sender,
        controller_sender,
        stream_registration,
        cseq_counter: 1, // Recommeded to start at 0 according to the RFC but VLC considers this an error
        pending_responses: HashMap::new(),
        pending_requests: HashMap::new(),
        sessions: HashMap::new(),
        pipelined_requests: HashMap::new(),
        session_medias: HashMap::new(),
        interleaved_channels: HashMap::new(),
    };

    // Spawn another task with a bit of buffering for the sink
    // - 10s timeout for sending each message -> timeout causes error to client
    // - 100 messages buffering, if full then error
    // - shareable into tasks below
    let send_task = {
        let client_sender = ctx.client_sender.clone();
        task::spawn(send_task(id, client_sender, rtsp_sink, rtsp_receiver))
    };

    let controller_sender = ctx.controller_sender.clone();

    // Main client task that handles everything
    let join_handle = task::spawn(task_fn(
        client,
        ctx,
        client_receiver,
        controller_receiver,
        stream_handler,
        rtsp_stream,
        send_task,
    ));

    Controller::<controller::Server>::new(id, join_handle, controller_sender)
}

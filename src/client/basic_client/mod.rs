use std::pin::Pin;

use futures::prelude::*;

use log::warn;

use crate::body::Body;
use crate::client;
use crate::error;
use crate::server;

#[derive(Debug)]
pub struct Client {
    dummy: (),
}

impl Default for Client {
    fn default() -> Self {
        Client { dummy: () }
    }
}

impl Client {
    fn handle_options(
        &mut self,
        ctx: &mut client::Context<Self>,
        req: client::OriginalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_types::Response<Body>, error::Error>> + Send>>
    {
        let mut handle = ctx.handle();

        let fut = async move {
            use rtsp_types::headers::{Public, Require, Session, Supported, Unsupported};
            use rtsp_types::Method;

            let mut resp = rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                // Add Public header for all methods this client supports
                .typed_header(
                    &Public::builder()
                        .method(Method::Options)
                        .method(Method::Describe)
                        .method(Method::Setup)
                        .method(Method::Play)
                        .method(Method::Pause)
                        .method(Method::Teardown)
                        .method(Method::GetParameter)
                        .method(Method::SetParameter)
                        .build(),
                )
                .build(Body::default());

            let req_require = req
                .typed_header::<Require>()
                .ok()
                .flatten()
                .unwrap_or_else(|| rtsp_types::headers::Require::builder().build());
            let req_supported = req
                .typed_header::<Supported>()
                .ok()
                .flatten()
                .unwrap_or_else(|| rtsp_types::headers::Supported::builder().build());

            // Here we could add additional information to be used in all requests below
            let extra_data = handle.default_extra_data_for_request(&req);

            let resp_allow;
            let resp_supported;
            let mut resp_unsupported;

            // Only valid for RTSP 2.0 and only if this is not for a media/session
            if req.request_uri().is_none() {
                resp_allow = None;
                resp_supported = Supported::builder()
                    .play_basic()
                    .setup_rtp_rtcp_mux()
                    .build();
                resp_unsupported = rtsp_types::headers::Unsupported::builder().build();
            } else if let Ok(Some(Session(session_id, _))) = req.typed_header::<Session>() {
                // TODO: need to check the URI also

                let session_id = server::SessionId::from(session_id.as_str());

                let mut media = if let Some(media) = handle.find_session_media(&session_id).await {
                    media
                } else {
                    handle.find_server_session_media(&session_id).await?
                };

                let (sup, unsup, _) = media
                    .options(req_supported, req_require.clone(), extra_data.clone())
                    .await?;
                resp_supported = sup;
                resp_unsupported = unsup;
                resp_allow = None;
            } else if let Some(request_uri) = req.request_uri() {
                let mut media_factory = handle
                    .find_media_factory_for_uri(request_uri.clone(), extra_data.clone())
                    .await?;

                let (allow, sup, unsup, _) = media_factory
                    .options(
                        request_uri.clone(),
                        req_supported,
                        req_require.clone(),
                        extra_data.clone(),
                    )
                    .await?;
                resp_supported = sup;
                resp_unsupported = unsup;
                resp_allow = Some(allow);
            } else {
                unreachable!();
            }

            // Check if the request required any features and if we support all of them. Otherwise the
            // request has to fail.

            if req.version() == rtsp_types::Version::V2_0 && !resp_supported.is_empty() {
                resp.insert_typed_header(&resp_supported);
            }

            for required in &*req_require {
                if !resp_supported.contains(required) && !resp_unsupported.contains(&required) {
                    resp_unsupported.push(required.to_owned());
                }
            }

            if !resp_unsupported.is_empty() {
                warn!(
                    "Client {} OPTIONS has unsupported features: {:?}",
                    handle.id(),
                    resp_unsupported
                );
                resp.set_status(rtsp_types::StatusCode::OptionNotSupported);
                resp.insert_typed_header(&Unsupported::from(resp_unsupported));
            } else {
                if let Some(allow) = resp_allow {
                    if !allow.is_empty()
                        && &*allow != &*resp.typed_header::<Public>().unwrap().unwrap()
                    {
                        resp.insert_typed_header(&allow);
                    }
                }
            }

            Ok(resp)
        };

        Box::pin(fut)
    }

    fn handle_describe(
        &mut self,
        ctx: &mut client::Context<Self>,
        req: client::OriginalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_types::Response<Body>, error::Error>> + Send>>
    {
        let mut handle = ctx.handle();
        let fut = async move {
            if let Some(request_uri) = req.request_uri() {
                if let Some(accept) = req.header(&rtsp_types::headers::ACCEPT) {
                    if accept
                        .as_str()
                        .split(',')
                        .map(str::trim)
                        .find(|f| f == &"application/sdp")
                        .is_none()
                    {
                        return Ok(rtsp_types::Response::builder(
                            req.version(),
                            rtsp_types::StatusCode::NotAcceptable,
                        )
                        .build(Body::default()));
                    }
                }

                // Here we could add additional information to be used in all requests below
                let extra_data = handle.default_extra_data_for_request(&req);

                let mut media_factory = handle
                    .find_media_factory_for_uri(request_uri.clone(), extra_data.clone())
                    .await?;
                let (sdp, _) = media_factory
                    .describe(request_uri.clone(), extra_data.clone())
                    .await?;
                let mut sdp_bytes = Vec::new();
                sdp.write(&mut sdp_bytes).unwrap();

                Ok(
                    rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                        .header(rtsp_types::headers::CONTENT_TYPE, "application/sdp")
                        // TODO: Figure out something more reliable for Content-Base
                        .header(rtsp_types::headers::CONTENT_BASE, request_uri.to_string())
                        .build(Body::from(sdp_bytes)),
                )
            } else {
                Ok(
                    rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::NotFound)
                        .build(Body::default()),
                )
            }
        };

        Box::pin(fut)
    }

    fn handle_setup(
        &mut self,
        ctx: &mut client::Context<Self>,
        req: client::OriginalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_types::Response<Body>, error::Error>> + Send>>
    {
        use rtsp_types::headers::{PipelinedRequests, Session, Transport, Transports};

        let mut handle = ctx.handle();

        let fut = async move {
            let pipelined_requests = req.typed_header::<PipelinedRequests>().map_err(|_| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;
            let session = req.typed_header::<Session>().map_err(|_| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;
            let transports = req
                .typed_header::<Transports>()
                .ok()
                .flatten()
                .ok_or_else(|| {
                    error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
                })?;
            let uri = req.request_uri().ok_or_else(|| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;

            let mut extra_data = handle.default_extra_data_for_request(&req);
            let mut pipelined_request_sender = None;
            let media_and_session_id = if let Some(session) = session {
                let session_id = server::SessionId::from(session.as_ref());
                if let Some(media) = handle.find_session_media(&session_id).await {
                    Some((media, session_id))
                } else {
                    Some((
                        handle.find_server_session_media(&session_id).await?,
                        session_id,
                    ))
                }
            } else if let Some(pipelined_requests) = pipelined_requests {
                let pipelined_request = handle
                    .find_media_for_pipelined_request(*pipelined_requests)
                    .await;

                use either::Either::{Left, Right};
                match pipelined_request {
                    Left(sender) => {
                        pipelined_request_sender = Some(sender);
                        None
                    }
                    Right(res) => Some(res?),
                }
            } else {
                None
            };

            let (mut media, stream_id, session_id) = if let Some((mut media, session_id)) =
                media_and_session_id
            {
                let mut media_factory = media.find_media_factory().await?;
                let (_, stream_id, _) = media_factory
                    .find_presentation_uri(uri.clone(), extra_data.clone())
                    .await?;

                let stream_id = stream_id.ok_or_else(|| {
                    error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::NotFound))
                })?;

                (media, stream_id, session_id)
            } else {
                let create_session_fut = async {
                    let mut media_factory = handle
                        .find_media_factory_for_uri(uri.clone(), extra_data.clone())
                        .await?;
                    let (presentation_uri, stream_id, _) = media_factory
                        .find_presentation_uri(uri.clone(), extra_data.clone())
                        .await?;

                    let stream_id = stream_id.ok_or_else(|| {
                        error::Error::from(error::ErrorStatus::from(
                            rtsp_types::StatusCode::NotFound,
                        ))
                    })?;

                    let (media, _) = media_factory
                        .create_media(uri.clone(), extra_data.clone())
                        .await?;
                    let session_id = handle
                        .create_session(presentation_uri, pipelined_requests.map(|p| *p), &media)
                        .await?;

                    Ok::<_, crate::error::Error>((media, stream_id, session_id))
                };

                match create_session_fut.await {
                    Ok(res) => {
                        if let Some(pipelined_request_sender) = pipelined_request_sender {
                            let _ = pipelined_request_sender.send(Ok(()));
                        }
                        res
                    }
                    Err(err) => {
                        if let Some(pipelined_request_sender) = pipelined_request_sender {
                            let _ = pipelined_request_sender.send(Err(err.clone()));
                        }
                        return Err(err);
                    }
                }
            };

            extra_data.insert(session_id.clone());

            match media
                .add_transport(
                    session_id.clone(),
                    stream_id,
                    transports,
                    extra_data.clone(),
                )
                .await
            {
                Ok(configured_transport) => {
                    let transports =
                        Transports::from(vec![Transport::Rtp(configured_transport.transport)]);

                    // TODO: Convert 1.0/2.0 transports for UDP by splitting/combining IP:port and
                    // the separate fields

                    let resp =
                        rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                            .header(rtsp_types::headers::SESSION, session_id.as_str())
                            .typed_header::<Transports>(&transports)
                            .build(Body::default());

                    Ok(resp)
                }
                Err(err) => {
                    // Manually create response instead of bubbling up the error so we can add the
                    // Session header if the session was not previously known
                    let resp = rtsp_types::Response::builder(req.version(), err.status_code())
                        .header(rtsp_types::headers::SESSION, session_id.as_str())
                        .build(Body::default());

                    Ok(resp)
                }
            }
        };

        Box::pin(fut)
    }

    fn handle_play(
        &mut self,
        ctx: &mut client::Context<Self>,
        req: client::OriginalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_types::Response<Body>, error::Error>> + Send>>
    {
        use rtsp_types::headers::{Range, RtpInfos, Session};

        let mut handle = ctx.handle();

        let fut = async move {
            let mut extra_data = handle.default_extra_data_for_request(&req);

            let range = req.typed_header::<Range>().ok().flatten().ok_or_else(|| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;
            let session = req
                .typed_header::<Session>()
                .map_err(|_| {
                    error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
                })?
                .ok_or_else(|| {
                    error::Error::from(error::ErrorStatus::from(
                        rtsp_types::StatusCode::SessionNotFound,
                    ))
                })?;
            let uri = req.request_uri().ok_or_else(|| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;

            let session_id = server::SessionId::from(session.as_ref());
            extra_data.insert(session_id.clone());

            let mut media = if let Some(media) = handle.find_session_media(&session_id).await {
                media
            } else {
                handle.find_server_session_media(&session_id).await?
            };

            let mut media_factory = media.find_media_factory().await?;
            let (_presentation_uri, stream_id, _) = media_factory
                .find_presentation_uri(uri.clone(), extra_data.clone())
                .await?;

            if stream_id.is_some() {
                return Err(
                    error::ErrorStatus::from(rtsp_types::StatusCode::MethodNotAllowed).into(),
                );
            }

            let (range, rtp_infos, _) = media
                .play(session_id.clone(), range, extra_data.clone())
                .await?;

            let rtp_infos = if req.version() == rtsp_types::Version::V1_0 {
                rtp_infos.try_into_v1().map_err(|_| {
                    error::Error::from(error::ErrorStatus::from(
                        rtsp_types::StatusCode::UnsupportedTransport,
                    ))
                })?
            } else {
                // FIXME: GStreamer wants 1.0 RTP-Info for RTSP 2.0...
                rtp_infos
            };

            let resp = rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                .header(rtsp_types::headers::SESSION, session_id.as_str())
                .typed_header::<Range>(&range)
                .typed_header::<RtpInfos>(&rtp_infos)
                .build(Body::default());

            Ok(resp)
        };

        Box::pin(fut)
    }

    fn handle_pause(
        &mut self,
        ctx: &mut client::Context<Self>,
        req: client::OriginalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_types::Response<Body>, error::Error>> + Send>>
    {
        use rtsp_types::headers::{Range, Session};

        let mut handle = ctx.handle();

        let fut = async move {
            let mut extra_data = handle.default_extra_data_for_request(&req);

            let session = req
                .typed_header::<Session>()
                .map_err(|_| {
                    error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
                })?
                .ok_or_else(|| {
                    error::Error::from(error::ErrorStatus::from(
                        rtsp_types::StatusCode::SessionNotFound,
                    ))
                })?;
            let uri = req.request_uri().ok_or_else(|| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;

            let session_id = server::SessionId::from(session.as_ref());
            extra_data.insert(session_id.clone());

            let mut media = if let Some(media) = handle.find_session_media(&session_id).await {
                media
            } else {
                handle.find_server_session_media(&session_id).await?
            };

            let mut media_factory = media.find_media_factory().await?;
            let (_presentation_uri, stream_id, _) = media_factory
                .find_presentation_uri(uri.clone(), extra_data.clone())
                .await?;

            if stream_id.is_some() {
                return Err(
                    error::ErrorStatus::from(rtsp_types::StatusCode::MethodNotAllowed).into(),
                );
            }

            let (range, _) = media.pause(session_id.clone(), extra_data.clone()).await?;

            let resp = rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                .header(rtsp_types::headers::SESSION, session_id.as_str())
                .typed_header::<Range>(&range)
                .build(Body::default());

            Ok(resp)
        };

        Box::pin(fut)
    }

    fn handle_teardown(
        &mut self,
        ctx: &mut client::Context<Self>,
        req: client::OriginalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_types::Response<Body>, error::Error>> + Send>>
    {
        use rtsp_types::headers::Session;

        let mut handle = ctx.handle();

        let fut = async move {
            let mut extra_data = handle.default_extra_data_for_request(&req);

            let session = req
                .typed_header::<Session>()
                .ok()
                .flatten()
                .ok_or_else(|| {
                    error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
                })?;
            let uri = req.request_uri().ok_or_else(|| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;

            let session_id = server::SessionId::from(session.as_ref());
            extra_data.insert(session_id.clone());

            let mut media = if let Some(media) = handle.find_session_media(&session_id).await {
                media
            } else {
                handle.find_server_session_media(&session_id).await?
            };

            let mut media_factory = media.find_media_factory().await?;
            let (_presentation_uri, stream_id, _) = media_factory
                .find_presentation_uri(uri.clone(), extra_data.clone())
                .await?;

            if let Some(ref stream_id) = stream_id {
                media
                    .remove_transport(session_id.clone(), stream_id.clone(), extra_data.clone())
                    .await?;
            } else {
                handle.shutdown_session(&session_id).await?;
            }

            let mut resp = rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                .build(Body::default());

            if stream_id.is_some() {
                resp.insert_header(rtsp_types::headers::SESSION, session_id.as_str());
            }

            Ok(resp)
        };

        Box::pin(fut)
    }

    fn handle_get_parameter(
        &mut self,
        _ctx: &mut client::Context<Self>,
        req: client::OriginalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_types::Response<Body>, error::Error>> + Send>>
    {
        Box::pin(async move {
            // Empty keep-alive GET_PARAMETER. The session was already kept alive by the Context
            // before passing the request here.
            if req.body().is_empty() || req.body().as_ref()[0] == 0 {
                let mut resp =
                    rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                        .build(Body::default());
                if let Some(session) = req.header(&rtsp_types::headers::SESSION) {
                    resp.insert_header(rtsp_types::headers::SESSION, session.as_str());
                }

                return Ok(resp);
            }

            Err(error::ErrorStatus::from(rtsp_types::StatusCode::ParameterNotUnderstood).into())
        })
    }

    fn handle_set_parameter(
        &mut self,
        _ctx: &mut client::Context<Self>,
        req: client::OriginalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_types::Response<Body>, error::Error>> + Send>>
    {
        Box::pin(async move {
            // Empty keep-alive SET_PARAMETER. The session was already kept alive by the Context
            // before passing the request here.
            if req.body().is_empty() || req.body().as_ref()[0] == 0 {
                let mut resp =
                    rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                        .build(Body::default());
                if let Some(session) = req.header(&rtsp_types::headers::SESSION) {
                    resp.insert_header(rtsp_types::headers::SESSION, session.as_str());
                }

                return Ok(resp);
            }

            Err(error::ErrorStatus::from(rtsp_types::StatusCode::ParameterNotUnderstood).into())
        })
    }
}

impl client::Client for Client {
    fn handle_request(
        &mut self,
        ctx: &mut client::Context<Self>,
        request: client::OriginalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_types::Response<Body>, error::Error>> + Send>>
    {
        match request.method() {
            rtsp_types::Method::Options => self.handle_options(ctx, request),
            rtsp_types::Method::Describe => self.handle_describe(ctx, request),
            rtsp_types::Method::Setup => self.handle_setup(ctx, request),
            rtsp_types::Method::Play => self.handle_play(ctx, request),
            rtsp_types::Method::Pause => self.handle_pause(ctx, request),
            rtsp_types::Method::Teardown => self.handle_teardown(ctx, request),
            rtsp_types::Method::GetParameter => self.handle_get_parameter(ctx, request),
            rtsp_types::Method::SetParameter => self.handle_set_parameter(ctx, request),
            _ => Box::pin(async move {
                Ok(rtsp_types::Response::builder(
                    request.version(),
                    rtsp_types::StatusCode::MethodNotAllowed,
                )
                .build(Body::default()))
            }),
        }
    }
}

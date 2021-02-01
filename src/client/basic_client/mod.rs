use std::pin::Pin;

use async_std::task;

use futures::prelude::*;

use log::warn;

use crate::body::Body;
use crate::client;
use crate::error;
use crate::media;
use crate::server;

#[derive(Debug)]
pub struct Client {
    rtsp_version: Option<rtsp_types::Version>,
}

impl Default for Client {
    fn default() -> Self {
        Client { rtsp_version: None }
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
            let mut extra_data = handle.default_extra_data_for_request(&req);

            let resp_allow;
            let resp_supported;
            let resp_extra_data;
            let mut resp_unsupported;

            // Only valid for RTSP 2.0 and only if this is not for a media/session
            if req.request_uri().is_none() {
                resp_allow = None;
                resp_supported = Supported::builder()
                    .play_basic()
                    .setup_rtp_rtcp_mux()
                    .build();
                resp_unsupported = rtsp_types::headers::Unsupported::builder().build();
                resp_extra_data = None;
            } else if let Ok(Some(Session(session_id, _))) = req.typed_header::<Session>() {
                let request_uri = req.request_uri().expect("no URI");

                let (media_factory, presentation_uri, mounts_extra_data) = handle
                    .find_media_factory_for_uri(request_uri.clone(), extra_data.clone())
                    .await?;

                let session_id = server::SessionId::from(session_id.as_str());
                let (mut media, session_presentation_uri) =
                    handle.find_session(&session_id).await?;

                // Check if the session ID and URI are matching correctly
                if presentation_uri != session_presentation_uri
                    || media_factory.media_factory_id()
                        != media.find_media_factory().await?.media_factory_id()
                {
                    return Err(crate::error::ErrorStatus::from(
                        rtsp_types::StatusCode::SessionNotFound,
                    )
                    .into());
                }

                let stream_id = media::extract_stream_id_from_uri(&presentation_uri, request_uri)?;

                extra_data.extend(&mounts_extra_data);
                extra_data.insert(presentation_uri);
                extra_data.insert(session_id);

                let (sup, unsup, extra_data) = media
                    .options(
                        stream_id,
                        req_supported,
                        req_require.clone(),
                        extra_data.clone(),
                    )
                    .await?;

                resp_supported = sup;
                resp_unsupported = unsup;
                resp_allow = None;
                resp_extra_data = Some(extra_data);
            } else if let Some(request_uri) = req.request_uri() {
                let (mut media_factory, presentation_uri, mounts_extra_data) = handle
                    .find_media_factory_for_uri(request_uri.clone(), extra_data.clone())
                    .await?;

                let stream_id = media::extract_stream_id_from_uri(&presentation_uri, request_uri)?;

                extra_data.extend(&mounts_extra_data);
                extra_data.insert(presentation_uri);

                let (allow, sup, unsup, extra_data) = media_factory
                    .options(
                        stream_id,
                        req_supported,
                        req_require.clone(),
                        extra_data.clone(),
                    )
                    .await?;
                resp_supported = sup;
                resp_unsupported = unsup;
                resp_allow = Some(allow);
                resp_extra_data = Some(extra_data);
            } else {
                unreachable!();
            }

            if let Some(extra_data) = resp_extra_data {
                if let Some(extra_headers) = extra_data.get::<client::ExtraHeaders>() {
                    for (name, value) in extra_headers.iter() {
                        resp.insert_header(name.clone(), value.clone());
                    }
                }
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
                if let Some(accept) =
                    req.typed_header::<rtsp_types::headers::Accept>()
                        .map_err(|_| {
                            crate::error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest)
                        })?
                {
                    if !accept.iter().any(|media_type| {
                        (media_type.type_ == Some(rtsp_types::headers::MediaType::Application)
                            && (media_type.subtype.as_deref() == Some("sdp")
                                || media_type.subtype.is_none()))
                            || media_type.type_.is_none()
                    }) {
                        return Ok(rtsp_types::Response::builder(
                            req.version(),
                            rtsp_types::StatusCode::NotAcceptable,
                        )
                        .build(Body::default()));
                    }
                }

                // Here we could add additional information to be used in all requests below
                let mut extra_data = handle.default_extra_data_for_request(&req);

                let (mut media_factory, presentation_uri, mounts_extra_data) = handle
                    .find_media_factory_for_uri(request_uri.clone(), extra_data.clone())
                    .await?;

                let stream_id = media::extract_stream_id_from_uri(&presentation_uri, request_uri)?;

                // Must be for the presentation URI!
                if !stream_id.is_none() {
                    return Err(
                        crate::error::ErrorStatus::from(rtsp_types::StatusCode::NotFound).into(),
                    );
                }

                extra_data.extend(&mounts_extra_data);
                extra_data.insert(presentation_uri.clone());

                let (sdp, extra_data) = media_factory.describe(extra_data.clone()).await?;
                let mut sdp_bytes = Vec::new();
                sdp.write(&mut sdp_bytes).unwrap();

                let mut resp =
                    rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                        .typed_header(&rtsp_types::headers::ContentType {
                            media_type: rtsp_types::headers::MediaType::Application,
                            media_subtype: "sdp".into(),
                            params: Vec::new(),
                        })
                        .header(
                            rtsp_types::headers::CONTENT_BASE,
                            presentation_uri.to_string(),
                        )
                        .build(Body::from(sdp_bytes));

                if let Some(extra_headers) = extra_data.get::<client::ExtraHeaders>() {
                    for (name, value) in extra_headers.iter() {
                        resp.insert_header(name.clone(), value.clone());
                    }
                }

                Ok(resp)
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
        use rtsp_types::headers::{
            AcceptRanges, PipelinedRequests, Session, Transport, Transports,
        };

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
            let accept_ranges = req.typed_header::<AcceptRanges>().map_err(|_| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;
            let uri = req.request_uri().ok_or_else(|| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;

            let mut extra_data = handle.default_extra_data_for_request(&req);
            let mut pipelined_request_sender = None;
            let media_and_session_id = if let Some(session) = session {
                let session_id = server::SessionId::from(session.as_ref());
                let (media, session_presentation_uri) = handle.find_session(&session_id).await?;
                Some((media, session_presentation_uri, session_id))
            } else if let Some(pipelined_requests) = pipelined_requests {
                let pipelined_request = handle
                    .find_session_for_pipelined_request(*pipelined_requests)
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

            let (mut media, presentation_uri, stream_id, session_id) =
                if let Some((mut media, session_presentation_uri, session_id)) =
                    media_and_session_id
                {
                    let session_media_factory = media.find_media_factory().await?;

                    let (media_factory, presentation_uri, _mounts_extra_data) = handle
                        .find_media_factory_for_uri(uri.clone(), extra_data.clone())
                        .await?;

                    if session_media_factory.media_factory_id() != media_factory.media_factory_id()
                        || session_presentation_uri != presentation_uri
                    {
                        return Err(crate::error::ErrorStatus::from(
                            rtsp_types::StatusCode::NotFound,
                        )
                        .into());
                    }

                    let stream_id = media::extract_stream_id_from_uri(&presentation_uri, uri)?
                        .ok_or_else(|| {
                            crate::error::ErrorStatus::from(rtsp_types::StatusCode::NotFound)
                        })?;

                    (media, presentation_uri, stream_id, session_id)
                } else {
                    let create_session_fut = async {
                        let (mut media_factory, presentation_uri, mounts_extra_data) = handle
                            .find_media_factory_for_uri(uri.clone(), extra_data.clone())
                            .await?;

                        extra_data.extend(&mounts_extra_data);
                        extra_data.insert(presentation_uri.clone());

                        let stream_id = media::extract_stream_id_from_uri(&presentation_uri, uri)?
                            .ok_or_else(|| {
                                crate::error::ErrorStatus::from(rtsp_types::StatusCode::NotFound)
                            })?;

                        let (media, _extra_data) =
                            media_factory.create_media(extra_data.clone()).await?;
                        let session_id = handle
                            .create_session(
                                presentation_uri.clone(),
                                pipelined_requests.map(|p| *p),
                                &media,
                            )
                            .await?;

                        Ok::<_, crate::error::Error>((
                            media,
                            presentation_uri,
                            stream_id,
                            session_id,
                        ))
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

            extra_data.insert(presentation_uri.clone());
            extra_data.insert(session_id.clone());
            let stream_id = crate::media::StreamId::from(stream_id);

            match media
                .add_transport(
                    session_id.clone(),
                    stream_id,
                    transports,
                    accept_ranges,
                    extra_data.clone(),
                )
                .await
            {
                Ok((transport, media_properties, accept_ranges, media_range, extra_data)) => {
                    let transports = Transports::from(vec![Transport::Rtp(transport)]);

                    // TODO: Convert 1.0/2.0 transports for UDP by splitting/combining IP:port and
                    // the separate fields

                    let mut resp =
                        rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                            .header(rtsp_types::headers::SESSION, session_id.as_str())
                            .typed_header::<Transports>(&transports)
                            .build(Body::default());

                    if let Some(extra_headers) = extra_data.get::<client::ExtraHeaders>() {
                        for (name, value) in extra_headers.iter() {
                            resp.insert_header(name.clone(), value.clone());
                        }
                    }

                    if req.version() == rtsp_types::Version::V2_0 {
                        resp.insert_typed_header(&media_properties);
                        resp.insert_typed_header(&accept_ranges);
                        if let Some(media_range) = media_range {
                            resp.insert_typed_header(&media_range);
                        }
                    }

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
        use rtsp_types::headers::{Range, RtpInfos, Scale, SeekStyle, Session, Speed};

        let mut handle = ctx.handle();

        let fut = async move {
            let mut extra_data = handle.default_extra_data_for_request(&req);

            let range = req.typed_header::<Range>().map_err(|_| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;
            let seek_style = req.typed_header::<SeekStyle>().map_err(|_| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;
            let scale = req.typed_header::<Scale>().map_err(|_| {
                error::Error::from(error::ErrorStatus::from(rtsp_types::StatusCode::BadRequest))
            })?;
            let speed = req.typed_header::<Speed>().map_err(|_| {
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
            let (mut media, session_presentation_uri) = handle.find_session(&session_id).await?;

            let session_media_factory = media.find_media_factory().await?;

            let (media_factory, presentation_uri, _mounts_extra_data) = handle
                .find_media_factory_for_uri(uri.clone(), extra_data.clone())
                .await?;

            if session_media_factory.media_factory_id() != media_factory.media_factory_id()
                || session_presentation_uri != presentation_uri
            {
                return Err(
                    crate::error::ErrorStatus::from(rtsp_types::StatusCode::NotFound).into(),
                );
            }

            extra_data.insert(session_id.clone());
            extra_data.insert(presentation_uri.clone());

            let stream_id = media::extract_stream_id_from_uri(&presentation_uri, uri)?;
            // TODO: Allow aggregate control
            if stream_id.is_some() {
                return Err(
                    error::ErrorStatus::from(rtsp_types::StatusCode::MethodNotAllowed).into(),
                );
            }

            let (range, rtp_infos, seek_style, scale, speed, extra_data) = media
                .play(
                    session_id.clone(),
                    stream_id,
                    range,
                    seek_style,
                    scale,
                    speed,
                    extra_data.clone(),
                )
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

            let mut resp = rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                .header(rtsp_types::headers::SESSION, session_id.as_str())
                .typed_header::<Range>(&range)
                .typed_header::<RtpInfos>(&rtp_infos)
                .build(Body::default());

            if req.version() == rtsp_types::Version::V2_0 {
                if let Some(seek_style) = seek_style {
                    resp.insert_typed_header(&seek_style);
                }
            }

            if let Some(scale) = scale {
                resp.insert_typed_header(&scale);
            }

            if let Some(speed) = speed {
                resp.insert_typed_header(&speed);
            }

            if let Some(extra_headers) = extra_data.get::<client::ExtraHeaders>() {
                for (name, value) in extra_headers.iter() {
                    resp.insert_header(name.clone(), value.clone());
                }
            }

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
            let (mut media, session_presentation_uri) = handle.find_session(&session_id).await?;

            let session_media_factory = media.find_media_factory().await?;

            let (media_factory, presentation_uri, _mounts_extra_data) = handle
                .find_media_factory_for_uri(uri.clone(), extra_data.clone())
                .await?;

            if session_media_factory.media_factory_id() != media_factory.media_factory_id()
                || session_presentation_uri != presentation_uri
            {
                return Err(
                    crate::error::ErrorStatus::from(rtsp_types::StatusCode::NotFound).into(),
                );
            }

            extra_data.insert(session_id.clone());
            extra_data.insert(presentation_uri.clone());

            let stream_id = media::extract_stream_id_from_uri(&presentation_uri, uri)?;
            // TODO: Allow aggregate control
            if stream_id.is_some() {
                return Err(
                    error::ErrorStatus::from(rtsp_types::StatusCode::MethodNotAllowed).into(),
                );
            }

            let (range, extra_data) = media
                .pause(session_id.clone(), stream_id, extra_data.clone())
                .await?;

            let mut resp = rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
                .header(rtsp_types::headers::SESSION, session_id.as_str())
                .typed_header::<Range>(&range)
                .build(Body::default());

            if let Some(extra_headers) = extra_data.get::<client::ExtraHeaders>() {
                for (name, value) in extra_headers.iter() {
                    resp.insert_header(name.clone(), value.clone());
                }
            }

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

            let (mut media, session_presentation_uri) = handle.find_session(&session_id).await?;

            let session_media_factory = media.find_media_factory().await?;

            let (media_factory, presentation_uri, _mounts_extra_data) = handle
                .find_media_factory_for_uri(uri.clone(), extra_data.clone())
                .await?;

            if session_media_factory.media_factory_id() != media_factory.media_factory_id()
                || session_presentation_uri != presentation_uri
            {
                return Err(
                    crate::error::ErrorStatus::from(rtsp_types::StatusCode::NotFound).into(),
                );
            }

            extra_data.insert(session_id.clone());
            extra_data.insert(presentation_uri.clone());

            let stream_id = media::extract_stream_id_from_uri(&presentation_uri, uri)?;

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

    // TODO: Pass to MF/Media based on URI?
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

    // TODO: Pass to MF/Media based on URI?
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
        // Remember the last used RTSP version
        self.rtsp_version = Some(request.version());

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

    fn media_play_notify(
        &mut self,
        ctx: &mut client::Context<Self>,
        _media_id: media::Id,
        session_id: server::SessionId,
        play_notify: media::PlayNotifyMessage,
    ) {
        if self.rtsp_version != Some(rtsp_types::Version::V2_0) {
            return;
        }

        let mut req =
            rtsp_types::Request::builder(rtsp_types::Method::PlayNotify, rtsp_types::Version::V2_0)
                .header(rtsp_types::headers::SESSION, session_id.as_str())
                .build(Body::default());

        match play_notify {
            media::PlayNotifyMessage::EndOfStream {
                range,
                rtp_info,
                extra_data,
            } => {
                req.insert_typed_header(&rtsp_types::headers::NotifyReason::EndOfStream);
                req.insert_typed_header(&range);
                req.insert_typed_header(&rtp_info);
                if let Some(extra_headers) = extra_data.get::<client::ExtraHeaders>() {
                    for (name, value) in extra_headers.iter() {
                        req.insert_header(name.clone(), value.clone());
                    }
                }
            }
            media::PlayNotifyMessage::MediaPropertiesUpdate {
                range,
                media_properties,
                media_range,
                extra_data,
            } => {
                req.insert_typed_header(&rtsp_types::headers::NotifyReason::MediaPropertiesUpdate);
                req.insert_typed_header(&range);
                req.insert_typed_header(&media_properties);
                req.insert_typed_header(&media_range);
                if let Some(extra_headers) = extra_data.get::<client::ExtraHeaders>() {
                    for (name, value) in extra_headers.iter() {
                        req.insert_header(name.clone(), value.clone());
                    }
                }
            }
            media::PlayNotifyMessage::ScaleChange {
                range,
                media_properties,
                media_range,
                scale,
                rtp_info,
                extra_data,
            } => {
                req.insert_typed_header(&rtsp_types::headers::NotifyReason::MediaPropertiesUpdate);
                req.insert_typed_header(&range);
                req.insert_typed_header(&media_properties);
                req.insert_typed_header(&media_range);
                req.insert_typed_header(&rtp_info);
                req.insert_typed_header(&scale);
                if let Some(extra_headers) = extra_data.get::<client::ExtraHeaders>() {
                    for (name, value) in extra_headers.iter() {
                        req.insert_header(name.clone(), value.clone());
                    }
                }
            }
        }

        let fut = ctx.send_request(req);
        task::spawn(async move {
            let _resp = fut.await;
            // TODO: Do something with the response?
        });
    }

    fn media_error(&mut self, _ctx: &mut client::Context<Self>, _media_id: media::Id) {
        // TODO: Send teardown to the client
    }

    fn media_finished(&mut self, _ctx: &mut client::Context<Self>, _media_id: media::Id) {
        // TODO: Send teardown to the client
    }
}

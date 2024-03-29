// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

use futures::prelude::*;

mod context;
pub use context::*;

pub(crate) mod controller;
pub(crate) use controller::Controller;

pub(self) mod messages;
pub(self) mod task;
pub(crate) use task::spawn;

pub use messages::PlayNotifyMessage;

use crate::client;
use crate::server;
use crate::typemap::TypeMap;

/// Unique identifier for a specific media
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(uuid::Uuid);

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Id {
    pub fn new() -> Self {
        use once_cell::sync::Lazy;
        use std::time;
        use uuid::v1;

        static INIT: Lazy<uuid::v1::Context> = Lazy::new(|| v1::Context::new(0));

        let time = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap();

        let timestamp = v1::Timestamp::from_unix(&*INIT, time.as_secs(), time.subsec_nanos());

        let mut node_id = [0; 6];
        node_id[0..4].copy_from_slice(&std::process::id().to_be_bytes());

        Id(uuid::Uuid::new_v1(timestamp, &node_id).unwrap())
    }
}

/// Stream ID.
///
/// Equal to the control attribute of the SDP.
///
/// Joining this with the [`server::PresentationURI`] should give the control URI of the stream.
///
/// This allows referring directly to the different streams of the media.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(Arc<String>);

impl StreamId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::ops::Deref for StreamId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for StreamId {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl std::borrow::Borrow<str> for StreamId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <String as fmt::Display>::fmt(&self.0, f)
    }
}

impl<'a> From<&'a str> for StreamId {
    fn from(s: &'a str) -> Self {
        String::from(s).into()
    }
}

impl From<String> for StreamId {
    fn from(s: String) -> Self {
        {
            // Don't allow .. in the path part of the relative URI
            let path_end_idx = s.find('?').or_else(|| s.find('#'));
            let s = if let Some(path_end_idx) = path_end_idx {
                s.split_at(path_end_idx).0
            } else {
                &s[..]
            };
            assert!(!s.contains(".."));
        }

        StreamId(Arc::new(s))
    }
}

/// Parses all stream IDs from the `control` attributes of the medias.
// FIXME: Proper error handling
pub fn stream_ids_from_sdp(
    session: &sdp_types::Session,
) -> Result<Vec<StreamId>, crate::error::Error> {
    let mut stream_ids = Vec::new();

    for media in &session.medias {
        let mut control = None;
        for attr in &media.attributes {
            if attr.attribute == "control" {
                if control.is_some() {
                    return Err(crate::error::InternalServerError.into());
                }

                control = attr.value.as_ref();
            }
        }

        match control {
            Some(control) => {
                stream_ids.push(control.as_str().into());
            }
            None => return Err(crate::error::InternalServerError.into()),
        }
    }

    Ok(stream_ids)
}

pub fn extract_stream_id_from_uri(
    presentation_uri: &url::Url,
    control_uri: &url::Url,
) -> Result<Option<StreamId>, crate::error::Error> {
    use crate::UrlExt;

    // Special case: control URI is actually the presentation URI with or without trailing '/'.
    if presentation_uri.path().starts_with(control_uri.path())
        && (control_uri.path().len() == presentation_uri.path().len()
            || (control_uri.path().len() + 1 == presentation_uri.path().len()
                && presentation_uri.path().ends_with('/')))
    {
        return Ok(None);
    }

    // If the control URI is smaller then there's nothing to return
    if control_uri.path().len() < presentation_uri.path().len() {
        return Err(crate::error::ErrorStatus::from(rtsp_types::StatusCode::NotFound).into());
    }

    let relative = presentation_uri
        .make_relative(control_uri)
        .ok_or_else(|| crate::error::ErrorStatus::from(rtsp_types::StatusCode::NotFound))?;

    // Must not go a level up
    if relative.starts_with('.') {
        return Err(crate::error::ErrorStatus::from(rtsp_types::StatusCode::NotFound).into());
    }

    Ok(Some(StreamId::from(relative)))
}

/// RTSP `Media`
pub trait Media: Send + 'static {
    const AUTOMATIC_IDLE: bool = true;

    /// Handle an OPTIONS request.
    fn options(
        &mut self,
        ctx: &mut Context<Self>,
        client_id: Option<client::Id>,
        stream_id: Option<StreamId>,
        supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        extra_data: TypeMap,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (
                            rtsp_types::headers::Supported,
                            rtsp_types::headers::Unsupported,
                            TypeMap,
                        ),
                        crate::error::Error,
                    >,
                > + Send,
        >,
    >;

    /// Return an SDP description for this media.
    fn describe(
        &mut self,
        ctx: &mut Context<Self>,
        client_id: Option<client::Id>,
        extra_data: TypeMap,
    ) -> Pin<
        Box<dyn Future<Output = Result<(sdp_types::Session, TypeMap), crate::error::Error>> + Send>,
    >;

    /// Add a transport for the given stream.
    fn add_transport(
        &mut self,
        ctx: &mut Context<Self>,
        client_id: client::Id,
        session_id: server::SessionId,
        stream_id: StreamId,
        transports: rtsp_types::headers::Transports,
        accept_ranges: Option<rtsp_types::headers::AcceptRanges>,
        extra_data: TypeMap,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (
                            rtsp_types::headers::RtpTransport,
                            rtsp_types::headers::MediaProperties,
                            rtsp_types::headers::AcceptRanges,
                            Option<rtsp_types::headers::MediaRange>,
                            TypeMap,
                        ),
                        crate::error::Error,
                    >,
                > + Send,
        >,
    >;

    /// Remove a previously configured transport.
    fn remove_transport(
        &mut self,
        ctx: &mut Context<Self>,
        client_id: client::Id,
        session_id: server::SessionId,
        stream_id: StreamId,
        extra_data: TypeMap,
    ) -> Pin<Box<dyn Future<Output = Result<(), crate::error::Error>> + Send>>;

    /// Shut down a complete client session with all transports.
    fn shutdown_session(
        &mut self,
        ctx: &mut Context<Self>,
        client_id: client::Id,
        session_id: server::SessionId,
        extra_data: TypeMap,
    ) -> Pin<Box<dyn Future<Output = Result<(), crate::error::Error>> + Send>>;

    /// Start playback of the media at a given range.
    fn play(
        &mut self,
        ctx: &mut Context<Self>,
        client_id: client::Id,
        session_id: server::SessionId,
        stream_id: Option<StreamId>,
        range: Option<rtsp_types::headers::Range>,
        seek_style: Option<rtsp_types::headers::SeekStyle>,
        scale: Option<rtsp_types::headers::Scale>,
        speed: Option<rtsp_types::headers::Speed>,
        extra_data: TypeMap,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (
                            rtsp_types::headers::Range,
                            rtsp_types::headers::RtpInfos,
                            Option<rtsp_types::headers::SeekStyle>,
                            Option<rtsp_types::headers::Scale>,
                            Option<rtsp_types::headers::Speed>,
                            TypeMap,
                        ),
                        crate::error::Error,
                    >,
                > + Send,
        >,
    >;

    /// Pause playback of the media.
    fn pause(
        &mut self,
        ctx: &mut Context<Self>,
        client_id: client::Id,
        session_id: server::SessionId,
        stream_id: Option<StreamId>,
        extra_data: TypeMap,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(rtsp_types::headers::Range, TypeMap), crate::error::Error>>
                + Send,
        >,
    >;

    /// Session has a new client.
    ///
    /// This is also called for the first client of a new session.
    fn session_new_client(
        &mut self,
        _ctx: &mut Context<Self>,
        _session_id: server::SessionId,
        _new_client_controller: Option<ClientHandle<Self>>,
    ) {
    }

    /// Session client timed out.
    fn session_timed_out(&mut self, _ctx: &mut Context<Self>, _session_id: server::SessionId) {}

    /// Startup.
    ///
    /// This is called exactly once in the beginning after spawning.
    fn startup(&mut self, _ctx: &mut Context<Self>) {}

    /// Shut down the media.
    ///
    /// No other functions on the media can be called afterwards.
    fn shutdown(&mut self, _ctx: &mut Context<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(future::ready(()))
    }
}

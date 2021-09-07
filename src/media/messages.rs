// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::pin::Pin;

use futures::prelude::*;

use crate::channel::oneshot;
use crate::client;
use crate::media_factory;
use crate::server;
use crate::typemap::TypeMap;

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum ServerMessage {
    ClientChanged(
        server::SessionId,
        #[derivative(Debug = "ignore")] server::Controller<server::controller::Media>,
        #[derivative(Debug = "ignore")] Option<client::Controller<client::controller::Media>>,
    ),
    TimedOut(server::SessionId),
}

impl From<ServerMessage> for ControllerMessage {
    fn from(msg: ServerMessage) -> ControllerMessage {
        ControllerMessage::Server(msg)
    }
}

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum ClientMessage {
    FindMediaFactory {
        client_id: client::Id,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<
                media_factory::Controller<media_factory::controller::Client>,
                crate::error::Error,
            >,
        >,
    },
    Options {
        client_id: client::Id,
        stream_id: Option<super::StreamId>,
        supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<
                (
                    rtsp_types::headers::Supported,
                    rtsp_types::headers::Unsupported,
                    TypeMap,
                ),
                crate::error::Error,
            >,
        >,
    },
    Describe {
        client_id: client::Id,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(sdp_types::Session, TypeMap), crate::error::Error>>,
    },
    AddTransport {
        client_id: client::Id,
        session_id: server::SessionId,
        stream_id: super::StreamId,
        transports: rtsp_types::headers::Transports,
        accept_ranges: Option<rtsp_types::headers::AcceptRanges>,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<
                (
                    rtsp_types::headers::RtpTransport,
                    rtsp_types::headers::MediaProperties,
                    rtsp_types::headers::AcceptRanges,
                    Option<rtsp_types::headers::MediaRange>,
                    TypeMap,
                ),
                crate::error::Error,
            >,
        >,
    },
    RemoveTransport {
        client_id: client::Id,
        session_id: server::SessionId,
        stream_id: super::StreamId,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(), crate::error::Error>>,
    },
    ShutdownSession {
        client_id: client::Id,
        session_id: server::SessionId,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(), crate::error::Error>>,
    },
    Play {
        client_id: client::Id,
        session_id: server::SessionId,
        stream_id: Option<super::StreamId>,
        range: Option<rtsp_types::headers::Range>,
        seek_style: Option<rtsp_types::headers::SeekStyle>,
        scale: Option<rtsp_types::headers::Scale>,
        speed: Option<rtsp_types::headers::Speed>,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<
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
        >,
    },
    Pause {
        client_id: client::Id,
        session_id: server::SessionId,
        stream_id: Option<super::StreamId>,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(rtsp_types::headers::Range, TypeMap), crate::error::Error>>,
    },
}

impl From<ClientMessage> for ControllerMessage {
    fn from(msg: ClientMessage) -> ControllerMessage {
        ControllerMessage::Client(msg)
    }
}

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(super) enum MediaFactoryMessage {
    Options {
        media_factory_id: media_factory::Id,
        stream_id: Option<super::StreamId>,
        supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<
            Result<
                (
                    rtsp_types::headers::Supported,
                    rtsp_types::headers::Unsupported,
                    TypeMap,
                ),
                crate::error::Error,
            >,
        >,
    },
    Describe {
        media_factory_id: media_factory::Id,
        extra_data: TypeMap,
        #[derivative(Debug = "ignore")]
        ret: oneshot::Sender<Result<(sdp_types::Session, TypeMap), crate::error::Error>>,
    },
    Quit(media_factory::Id),
}

impl From<MediaFactoryMessage> for ControllerMessage {
    fn from(msg: MediaFactoryMessage) -> ControllerMessage {
        ControllerMessage::MediaFactory(msg)
    }
}

#[derive(Debug)]
pub(super) enum ControllerMessage {
    Server(ServerMessage),
    Client(ClientMessage),
    MediaFactory(MediaFactoryMessage),
}

#[derive(derivative::Derivative)]
#[derivative(Debug)]
/// Messages handled by the media context task
pub(super) enum MediaMessage<M: super::Media + ?Sized> {
    Controller(ControllerMessage),
    ControllerClosed,
    MediaFuture(
        #[derivative(Debug = "ignore")]
        Box<
            dyn FnOnce(&mut M, &mut super::Context<M>) -> Pin<Box<dyn Future<Output = ()> + Send>>
                + Send,
        >,
    ),
    MediaClosure(
        #[derivative(Debug = "ignore")]
        Box<dyn FnOnce(&mut M, &mut super::Context<M>) + Send + 'static>,
    ),
}

#[derive(Debug, Clone)]
pub enum PlayNotifyMessage {
    EndOfStream {
        // TODO: Request-Status
        range: rtsp_types::headers::Range,
        rtp_info: rtsp_types::headers::RtpInfos,
        extra_data: TypeMap,
    },
    MediaPropertiesUpdate {
        range: rtsp_types::headers::Range,
        media_properties: rtsp_types::headers::MediaProperties,
        media_range: rtsp_types::headers::MediaRange,
        extra_data: TypeMap,
    },
    ScaleChange {
        range: rtsp_types::headers::Range,
        media_properties: rtsp_types::headers::MediaProperties,
        media_range: rtsp_types::headers::MediaRange,
        scale: rtsp_types::headers::Scale,
        rtp_info: rtsp_types::headers::RtpInfos,
        extra_data: TypeMap,
    },
}

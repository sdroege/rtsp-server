// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! # RTSP Server Library
//!
//! ## Overview of the components
//!
//! ### `Server`
//!
//! The [`server::Server`] is the most basic type to create a new server instance, define the ports
//! to listen on or other [`listener::Listener`]s and the mapping of the server paths to
//! [`media_factory::MediaFactory`] via the [`server::Mounts`]s.
//!
//! See the [`server`] module for details.
//!
//! ### Customizeable Components
//!
//! Various parts of the server can be customized outside this crate. The general design pattern
//! for this is based on actors and the following pieces:
//!
//!  * A trait to implement the custom behaviour plus trait functions that get a mutable reference
//!    to the implementation plus a `Context`. Many of the trait functions return a `Pin<Box<dyn
//!    Future>>` for allowing the response to be handled asynchronously.
//!  * A `Context` that keeps track of the execution state and provides access to various API to
//!    modify the state or communicate with other components.
//!  * A `Handle` that can be cloned and can be used from arbitrary other threads, and which
//!    provides access to the same APIs as the `Context` via `async fn`s.
//!
//! The `Handle` allows running custom `Future`s and closures in the executation context of the
//! trait implementation, as well as registering custom `Stream`s as part of the
//! [`stream_handler::MessageHandler`] trait.
//!
//! #### `Client`
//!
//! A [`client::Client`] implementation handles incoming RTSP requests and produces the
//! corresponding RTSP responses, is responsible for setting up and managing RTSP sessions and
//! managing TCP/interleaved channels.
//!
//! [`client::basic_client::Client`] provides a basic implementation of this that follows the RTSP
//! 1.0 and RTSP 2.0 standards.
//!
//! See the [`client`] module for details.
//!
//! #### `MediaFactory`
//!
//! A [`media_factory::MediaFactory`] implementation is responsible for handling medias at a
//! specific server path. As part of this it needs to be able to answer `OPTIONS` and `DESCRIBE`
//! requests to provide more information about the medias, and be able to create new medias or
//! potentially re-use existing medias for client RTSP sessions.
//!
//! See the [`media_factory`] module for details.
//!
//! #### `Media`
//!
//! A [`media::Media`] implementation provides the actual media streams for one or multiple client
//! RTSP sessions. As part of this the various RTSP commands (`OPTIONS`, `DESCRIBE`, `PLAY`,
//! `PAUSE`, `SETUP`) have to be handled accordingly to set up the underlying media implementation.
//!
//! See the [`media`] module for details.
//!
//! #### Passing Custom State
//!
//! Custom state can be passed between the different components via the [`typemap::TypeMap`] type.
//! Many operations take this as additional parameter or return it as additional result.
//!
//! A `TypeMap` allows to store exactly one value of each type.

pub mod body;
pub mod channel;
pub mod client;
pub mod error;
pub mod listener;
pub mod media;
pub mod media_factory;
pub mod server;
pub mod stream_handler;
pub mod typemap;
pub(crate) mod utils;

pub use either::Either;
pub use rtsp_types as types;
pub use url::Url;

pub use utils::UrlExt;

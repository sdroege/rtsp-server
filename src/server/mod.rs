// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

mod context;

pub(crate) mod controller;
pub(crate) use controller::Controller;
pub use controller::{Builder, Server};

pub mod mounts;
pub use mounts::Mounts;

pub(self) mod messages;
pub(self) mod session;
pub(self) mod task;

pub(self) const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

pub use mounts::PresentationURI;
pub use session::Id as SessionId;

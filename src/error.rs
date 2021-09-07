// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::any::{Any, TypeId};
use std::sync::Arc;
use std::{error, fmt, ops};

#[derive(Debug, Clone)]
pub struct Error(Arc<dyn ServerError>);

impl ops::Deref for Error {
    type Target = dyn ServerError;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl Error {
    pub fn is<T: ServerError>(&self) -> bool {
        <dyn ServerError as Any>::type_id(&*self.0) == TypeId::of::<T>()
    }

    pub fn downcast<T: ServerError>(&self) -> Option<&T> {
        if self.is::<T>() {
            unsafe { Some(&*(&*self.0 as *const dyn ServerError as *const T)) }
        } else {
            None
        }
    }
}

pub trait ServerError: Any + std::error::Error + Send + Sync {
    fn status_code(&self) -> rtsp_types::StatusCode;
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*self.0, fmt)
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        error::Error::source(&*self.0)
    }
}

impl<T: ServerError + 'static> From<T> for Error {
    fn from(v: T) -> Error {
        Error(Arc::new(v))
    }
}

impl ServerError for std::io::Error {
    fn status_code(&self) -> rtsp_types::StatusCode {
        rtsp_types::StatusCode::InternalServerError
    }
}

#[derive(Debug)]
pub struct InternalServerError;

impl fmt::Display for InternalServerError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "Internal server error")
    }
}

impl error::Error for InternalServerError {}

impl ServerError for InternalServerError {
    fn status_code(&self) -> rtsp_types::StatusCode {
        rtsp_types::StatusCode::InternalServerError
    }
}

#[derive(Debug)]
pub struct ErrorStatus(rtsp_types::StatusCode);

impl From<rtsp_types::StatusCode> for ErrorStatus {
    fn from(code: rtsp_types::StatusCode) -> Self {
        assert!(code.is_client_error() || code.is_server_error());

        Self(code)
    }
}

impl fmt::Display for ErrorStatus {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        <rtsp_types::StatusCode as fmt::Display>::fmt(&self.0, fmt)
    }
}

impl error::Error for ErrorStatus {}

impl ServerError for ErrorStatus {
    fn status_code(&self) -> rtsp_types::StatusCode {
        self.0
    }
}

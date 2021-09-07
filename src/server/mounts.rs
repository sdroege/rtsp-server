// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::collections::HashMap;
use std::fmt;
use std::mem;
use std::sync::Arc;

use futures::stream::FuturesUnordered;

use log::debug;

use super::controller;
use super::messages::*;
use crate::channel::mpsc;
use crate::client;
use crate::media_factory;
use crate::server;
use crate::typemap::TypeMap;

/// Presentation URI.
///
/// Joining the presentation URI with the control attribute of the SDP should give the control URI
/// of the stream.
///
/// This is also passed through the extra data in various places.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PresentationURI(Arc<url::Url>);

impl AsRef<url::Url> for PresentationURI {
    fn as_ref(&self) -> &url::Url {
        &self.0
    }
}

impl std::borrow::Borrow<url::Url> for PresentationURI {
    fn borrow(&self) -> &url::Url {
        self.as_ref()
    }
}

impl std::ops::Deref for PresentationURI {
    type Target = url::Url;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl fmt::Display for PresentationURI {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <url::Url as fmt::Display>::fmt(&self.0, f)
    }
}

impl From<url::Url> for PresentationURI {
    fn from(uri: url::Url) -> Self {
        PresentationURI(Arc::new(uri))
    }
}

/// Matches RTSP URLs and returns a handle to the corresponding media factory
pub struct Mounts {
    /// All match functions in the order they were defined.
    match_fns: HashMap<
        media_factory::Id,
        Box<
            dyn Fn(&url::Url, &TypeMap) -> Option<(PresentationURI, TypeMap)>
                + Send
                + Sync
                + 'static,
        >,
    >,

    /// Spawned media factories with their controllers.
    ///
    /// Empty until spawning.
    factories:
        HashMap<media_factory::Id, media_factory::Controller<media_factory::controller::Server>>,

    /// Factory functions for spawning the media factories.
    ///
    /// Empty after spawning.
    factory_fns: Vec<(
        media_factory::Id,
        Box<
            dyn FnOnce(
                    server::Controller<server::controller::MediaFactory>,
                )
                    -> media_factory::Controller<media_factory::controller::Server>
                + Send
                + 'static,
        >,
    )>,
}

/// Builder for `Mounts`
pub struct Builder {
    factory_fns: Vec<(
        media_factory::Id,
        Box<
            dyn FnOnce(
                    server::Controller<server::controller::MediaFactory>,
                )
                    -> media_factory::Controller<media_factory::controller::Server>
                + Send
                + 'static,
        >,
    )>,
    match_fns: Vec<(
        media_factory::Id,
        Box<
            dyn Fn(&url::Url, &TypeMap) -> Option<(PresentationURI, TypeMap)>
                + Send
                + Sync
                + 'static,
        >,
    )>,
}

impl Builder {
    /// Build the mounts.
    pub fn build(self) -> Mounts {
        Mounts {
            match_fns: self.match_fns.into_iter().collect(),
            factories: HashMap::new(),
            factory_fns: self.factory_fns,
        }
    }

    /// Matches the factory if the requested path is a prefix of `path`.
    ///
    /// The corresponding presentation URI will be the path followed by a '/'.
    ///
    /// Must start with `/` or otherwise this function panics.
    pub fn path<
        MF: media_factory::MediaFactory,
        F: FnOnce(media_factory::Id) -> MF + Send + 'static,
    >(
        mut self,
        path: &str,
        factory_func: F,
    ) -> Self {
        assert!(path.starts_with('/'));

        let id = media_factory::Id::new();
        self.factory_fns.push((
            id,
            Box::new(move |controller| {
                let factory = factory_func(controller.media_factory_id());
                media_factory::spawn(controller, factory)
            }),
        ));

        let path = if path.ends_with('/') {
            String::from(path)
        } else {
            format!("{}/", path)
        };

        self.match_fns.push((
            id,
            Box::new(move |url, _extra_data| {
                if url.cannot_be_a_base() {
                    return None;
                }

                let mut uri_path_segments = match url.path_segments() {
                    None => return None,
                    Some(segments) => segments,
                };
                let match_path_segments = path[1..].split('/');

                // Check if all path segments that the match path contains are
                // also contained in the URI's path.
                //
                // This match `/foo/bar` for `/foo` but not `/foobar`.
                for match_path_segment in match_path_segments {
                    // Skip the last, empty path segment
                    if match_path_segment.is_empty() {
                        break;
                    }

                    if let Some(uri_path_segment) = uri_path_segments.next() {
                        if match_path_segment != uri_path_segment {
                            return None;
                        }
                    } else {
                        return None;
                    }
                }

                let mut presentation_uri = url.clone();
                presentation_uri.set_query(None);
                presentation_uri.set_fragment(None);
                let _ = presentation_uri.set_username("");
                let _ = presentation_uri.set_password(None);
                presentation_uri.set_path(&path);

                Some((PresentationURI::from(presentation_uri), Default::default()))
            }),
        ));

        self
    }

    /// Matches the factory based on `func` and returns the presentation URI if it matches.
    ///
    /// The presentation URI would be equivalent to the `Content-Base` and joining it with the
    /// media's stream IDs should result in the stream control URIs.
    pub fn matches<
        MF: media_factory::MediaFactory,
        F: FnOnce(media_factory::Id) -> MF + Send + 'static,
        G: Fn(&url::Url, &TypeMap) -> Option<(PresentationURI, TypeMap)> + Send + Sync + 'static,
    >(
        mut self,
        func: G,
        factory_func: F,
    ) -> Self {
        let id = media_factory::Id::new();
        self.factory_fns.push((
            id,
            Box::new(move |controller| {
                let factory = factory_func(controller.media_factory_id());
                media_factory::spawn(controller, factory)
            }),
        ));
        self.match_fns.push((id, Box::new(func)));

        self
    }
}

impl Mounts {
    /// Create a new `Builder` for mounts.
    pub fn builder() -> Builder {
        Builder {
            factory_fns: Vec::new(),
            match_fns: Vec::new(),
        }
    }

    /// Match `uri` and return the first matching media factory and the corresponding presentation URI.
    pub(super) fn match_uri(
        &self,
        client_id: client::Id,
        uri: &url::Url,
        extra_data: TypeMap,
    ) -> Option<(
        media_factory::Controller<media_factory::controller::Client>,
        PresentationURI,
        TypeMap,
    )> {
        assert!(self.factory_fns.is_empty());

        for (id, match_fn) in &self.match_fns {
            if let Some((presentation_uri, extra_data)) = match_fn(uri, &extra_data) {
                debug!(
                    "Match URI {} to media factory {} with presentation URI {}",
                    uri, id, presentation_uri
                );
                let factory_controller = self.factories.get(&id).expect("factory not found");
                return Some((media_factory::controller::Controller::<
                    media_factory::controller::Client,
                >::from_server_controller(
                    &factory_controller, client_id
                ), presentation_uri, extra_data));
            }
        }

        debug!("Couldn't match URI {} to a media factory", uri);

        None
    }

    /// Spawn all media factories.
    pub(super) fn spawn(&mut self, server_sender: mpsc::Sender<ControllerMessage>) {
        let factory_fns = mem::replace(&mut self.factory_fns, Vec::new());

        for (id, factory) in factory_fns {
            let server_controller =
                controller::Controller::<controller::MediaFactory>::new(id, server_sender.clone());

            let factory_controller = factory(server_controller);

            self.factories.insert(id, factory_controller);
        }
    }

    /// Shut down all media factories.
    pub(super) fn shutdown(
        self,
        close_tasks: &mut FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        >,
    ) {
        for (_, factory_controller) in self.factories {
            close_tasks.push(Box::pin(async move {
                let _ = factory_controller.shutdown().await;
            }));
        }
    }

    /// Media factory finished early.
    pub(super) fn media_factory_finished(&mut self, media_factory_id: media_factory::Id) {
        self.match_fns.remove(&media_factory_id);
        self.factories.remove(&media_factory_id);
    }
}

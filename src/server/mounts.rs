use std::collections::HashMap;
use std::mem;

use futures::stream::FuturesUnordered;

use log::debug;

use super::controller;
use super::messages::*;
use crate::channel::mpsc;
use crate::client;
use crate::media_factory;
use crate::server;
use crate::typemap::TypeMap;

/// Matches RTSP URLs and returns a handle to the corresponding media factory
pub struct Mounts {
    /// All match functions in the order they were defined.
    match_fns: HashMap<
        media_factory::Id,
        Box<dyn Fn(&url::Url, &TypeMap) -> bool + Send + Sync + 'static>,
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
        Box<dyn Fn(&url::Url, &TypeMap) -> bool + Send + Sync + 'static>,
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

        let path = String::from(path);
        self.match_fns.push((
            id,
            Box::new(move |url, _extra_data| {
                let mut uri_path_segments = match url.path_segments() {
                    None => return false,
                    Some(segments) => segments,
                };
                let match_path_segments = path[1..].split('/');

                // Check if all path segments that the match path contains are
                // also contained in the URI's path.
                //
                // This match `/foo/bar` for `/foo` but not `/foobar`.
                for match_path_segment in match_path_segments {
                    if let Some(uri_path_segment) = uri_path_segments.next() {
                        if match_path_segment != uri_path_segment {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }

                true
            }),
        ));

        self
    }

    /// Matches the factory based on `func` returning `true`.
    pub fn matches<
        MF: media_factory::MediaFactory,
        F: FnOnce(media_factory::Id) -> MF + Send + 'static,
        G: Fn(&url::Url, &TypeMap) -> bool + Send + Sync + 'static,
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

    /// Match `uri` and return the first matching media factory.
    pub(super) fn match_uri(
        &self,
        client_id: client::Id,
        uri: &url::Url,
        extra_data: TypeMap,
    ) -> Option<media_factory::Controller<media_factory::controller::Client>> {
        assert!(self.factory_fns.is_empty());

        for (id, match_fn) in &self.match_fns {
            if match_fn(uri, &extra_data) {
                debug!("Match URI {} to media factory {}", uri, id);
                let factory_controller = self.factories.get(&id).expect("factory not found");
                return Some(media_factory::controller::Controller::<
                    media_factory::controller::Client,
                >::from_server_controller(
                    &factory_controller, client_id
                ));
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

use std::fmt;
use std::pin::Pin;

use futures::prelude::*;

use crate::client;
use crate::media;
use crate::typemap::TypeMap;

mod context;
pub use context::*;

pub(crate) mod controller;
pub(crate) use controller::Controller;

pub(self) mod messages;
pub(self) mod task;
pub(crate) use task::spawn;

/// Unique identifier for a specific media factory
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

/// RTSP `MediaFactory`
pub trait MediaFactory: Send + 'static {
    /// Handle an OPTIONS request.
    fn options(
        &mut self,
        ctx: &mut Context<Self>,
        uri: url::Url,
        supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        extra_data: TypeMap,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (
                            rtsp_types::headers::Allow,
                            rtsp_types::headers::Supported,
                            rtsp_types::headers::Unsupported,
                            TypeMap,
                        ),
                        crate::error::Error,
                    >,
                > + Send,
        >,
    >;

    /// Return an SDP description for this media factory.
    fn describe(
        &mut self,
        ctx: &mut Context<Self>,
        uri: url::Url,
        extra_data: TypeMap,
    ) -> Pin<
        Box<dyn Future<Output = Result<(sdp_types::Session, TypeMap), crate::error::Error>> + Send>,
    >;

    /// Return the presentation URI for the media at the given URI.
    fn find_presentation_uri(
        &mut self,
        ctx: &mut Context<Self>,
        uri: url::Url,
        extra_data: TypeMap,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (url::Url, Option<media::StreamId>, TypeMap),
                        crate::error::Error,
                    >,
                > + Send,
        >,
    >;

    /// Create a new media for a client.
    fn create_media(
        &mut self,
        ctx: &mut Context<Self>,
        uri: url::Url,
        client_id: client::Id,
        extra_data: TypeMap,
    ) -> Pin<
        Box<dyn Future<Output = Result<(MediaHandle<Self>, TypeMap), crate::error::Error>> + Send>,
    >;

    /// Media signals that it is becoming idle or not idle any longer.
    fn media_idle(
        &mut self,
        ctx: &mut Context<Self>,
        media_id: media::Id,
        idle: bool,
        extra_data: TypeMap,
    ) -> Pin<Box<dyn Future<Output = Result<TypeMap, crate::error::Error>> + Send>>;

    /// Media signals an error.
    fn media_error(&mut self, _ctx: &mut Context<Self>, _media_id: media::Id) {}

    /// Media signals that it is finished.
    fn media_finished(&mut self, _ctx: &mut Context<Self>, _media_id: media::Id) {}

    /// Startup.
    ///
    /// This is called exactly once in the beginning after spawning.
    fn startup(&mut self, _ctx: &mut Context<Self>) {}

    /// Shut down the media factory.
    ///
    /// No other functions on the media can be called afterwards.
    fn shutdown(&mut self, _ctx: &mut Context<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(future::ready(()))
    }
}

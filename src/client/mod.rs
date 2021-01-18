use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

use futures::prelude::*;

use crate::body::Body;
use crate::media;
use crate::server;
use crate::typemap::TypeMap;

mod context;
pub use context::*;

pub(crate) mod controller;
pub(crate) use controller::Controller;

mod interleaved;
pub use interleaved::*;

pub(self) mod messages;
pub(self) mod task;
pub(crate) use task::spawn;

/// Unique identifier for a specific client
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

/// Original request.
///
/// This is also passed through the extra data in various places.
#[derive(Debug, Clone)]
pub struct OriginalRequest(pub(self) Arc<rtsp_types::Request<Body>>);

impl std::ops::Deref for OriginalRequest {
    type Target = rtsp_types::Request<Body>;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

pub trait Client: Send + 'static {
    /// Handle an RTSP request asynchronously.
    fn handle_request(
        &mut self,
        ctx: &mut Context<Self>,
        request: OriginalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_types::Response<Body>, crate::error::Error>> + Send>>;

    /// Session has timed out.
    fn session_timed_out(&mut self, _ctx: &mut Context<Self>, _session_id: server::SessionId) {}

    /// Session has a new client.
    fn session_replaced_client(
        &mut self,
        _ctx: &mut Context<Self>,
        _session_id: server::SessionId,
    ) {
    }

    /// Media wants to register an interleaved channel.
    fn media_register_interleaved_channel(
        &mut self,
        ctx: &mut Context<Self>,
        _media_id: media::Id,
        session_id: server::SessionId,
        channel_id: Option<u8>,
        receivers: Vec<Box<dyn DataReceiver>>,
        _extra_data: TypeMap,
    ) -> Result<(u8, Vec<DataSender>, TypeMap), crate::error::Error> {
        let (channel_id, senders) =
            ctx.register_interleaved_channel(session_id, channel_id, receivers)?;

        Ok((channel_id, senders, TypeMap::default()))
    }

    /// Media reports a play notify message.
    fn media_play_notify(
        &mut self,
        _ctx: &mut Context<Self>,
        _media_id: media::Id,
        _session_id: server::SessionId,
        _play_notify: media::PlayNotifyMessage,
    ) {
    }

    /// Media has an error.
    fn media_error(&mut self, _ctx: &mut Context<Self>, _media_id: media::Id) {}

    /// Media has finished running.
    fn media_finished(&mut self, _ctx: &mut Context<Self>, _media_id: media::Id) {}

    /// Startup.
    ///
    /// This is called exactly once in the beginning after spawning.
    fn startup(&mut self, _ctx: &mut Context<Self>) {}

    /// Shut down the client.
    ///
    /// No other functions of the client can be called afterwards.
    fn shutdown(&mut self, _ctx: &mut Context<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(future::ready(()))
    }
}

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

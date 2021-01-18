use std::collections::{HashMap, HashSet};

use super::mounts;
use super::session;
use crate::channel::mpsc;
use crate::client;
use crate::listener;

use super::controller::{self, Controller};
use super::messages::*;

/// Server state
pub(super) struct Context {
    pub(super) factory_fn: Box<
        dyn FnMut(
                Controller<controller::Client>,
                listener::IncomingConnection,
            ) -> Option<client::Controller<client::controller::Server>>
            + Send,
    >,
    pub(super) server_sender: mpsc::Sender<ServerMessage>,
    pub(super) controller_sender: mpsc::Sender<ControllerMessage>,
    pub(super) listeners: HashMap<listener::Id, listener::Controller>,
    pub(super) clients: HashMap<client::Id, Client>,
    pub(super) sessions: HashMap<session::Id, session::Session>,
    pub(super) mounts: Option<mounts::Mounts>,
}

pub(super) struct Client {
    pub(super) controller: client::Controller<client::controller::Server>,
    pub(super) sessions: HashSet<session::Id>,
}

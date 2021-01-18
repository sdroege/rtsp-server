use std::error;
use std::fmt;
use std::sync::Arc;

use futures::prelude::*;

use log::error;

use crate::body::Body;
use crate::channel::mpsc;

use super::messages::RtspSendMessage;

/// Trait to implement for data receivers of interleaved channels.
pub trait DataReceiver: Send + 'static {
    /// Handle the received data.
    ///
    /// This function must not block.
    fn handle_data(&mut self, data: rtsp_types::Data<Body>);

    /// The interleaved channel was closed.
    ///
    /// This function must not block.
    fn closed(&mut self) {}
}

impl DataReceiver for Box<dyn DataReceiver> {
    fn handle_data(&mut self, data: rtsp_types::Data<Body>) {
        self.as_mut().handle_data(data);
    }
    fn closed(&mut self) {
        self.as_mut().closed();
    }
}

/// Sender for an interleaved channel.
///
/// Once the last sender for the channel is dropped it will be automatically closed.
#[derive(Debug)]
pub struct DataSender {
    /// Client ID.
    pub(super) client_id: super::Id,
    /// Channel ID of this sender.
    pub(super) channel_id: u8,
    /// Sender for notifying the client that this channel can be closed
    pub(super) close_sender: Arc<CloseSender>,
    /// Sender for sending RTSP messages to the peer
    pub(super) rtsp_sender: mpsc::Sender<RtspSendMessage>,
    /// Sender,receiver for getting ACKs from the RTSP messages
    pub(super) ack_channel: (mpsc::Sender<()>, mpsc::Receiver<()>),
}

impl Clone for DataSender {
    fn clone(&self) -> Self {
        DataSender {
            client_id: self.client_id,
            channel_id: self.channel_id,
            close_sender: self.close_sender.clone(),
            rtsp_sender: self.rtsp_sender.clone(),
            ack_channel: mpsc::channel(),
        }
    }
}

#[derive(Debug)]
pub(super) struct CloseSender {
    pub(super) client_id: super::Id,
    pub(super) channel_id: u8,
    pub(super) sender: mpsc::Sender<()>,
}

impl Drop for CloseSender {
    fn drop(&mut self) {
        if let Err(err) = self.sender.try_send(()) {
            if err.is_full() {
                error!(
                    "Client {}: Can't notify about interleaved sender {} drop: {}",
                    self.client_id, self.channel_id, err
                );
            }
        }
    }
}

impl DataSender {
    /// Channel ID of the interleaved channel.
    pub fn channel_id(&self) -> u8 {
        self.channel_id
    }

    /// Send data via the interleaved channel.
    ///
    /// This only returns on error or once the data is sent.
    pub async fn send_data(&mut self, data: Body) -> Result<(), InterleavedChannelClosedError> {
        let data = rtsp_types::Data::new(self.channel_id, data);

        self.rtsp_sender
            .send(RtspSendMessage(
                data.into(),
                Some(self.ack_channel.0.clone()),
            ))
            .await
            .map_err(|_| InterleavedChannelClosedError(self.channel_id))?;
        self.ack_channel
            .1
            .next()
            .await
            .ok_or_else(|| InterleavedChannelClosedError(self.channel_id))?;

        Ok(())
    }

    /// Close the interleaved channel.
    ///
    /// The same will happen when the sender is dropped.
    pub async fn close(&mut self) {
        let _ = self.close_sender.sender.clone().send(()).await;
    }
}

/// Error returned when the interleaved channel was closed.
#[derive(Debug)]
pub struct InterleavedChannelClosedError(u8);

impl InterleavedChannelClosedError {
    /// Corresponding channel's id.
    pub fn channel_id(&self) -> u8 {
        self.0
    }
}

impl fmt::Display for InterleavedChannelClosedError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "Channel {} was closed", self.0)
    }
}

impl error::Error for InterleavedChannelClosedError {}

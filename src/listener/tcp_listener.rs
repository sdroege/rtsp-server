// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::prelude::*;

use super::{
    Id, IncomingConnection, IncomingConnectionStream, Listener, MessageSink, MessageStream,
};
use log::{debug, error, warn};

pub(crate) fn tcp_listener(addr: std::net::SocketAddr, max_size: usize) -> Listener {
    use async_std::net::TcpListener;

    let id = Id::from(url::Url::parse(&format!("tcp://{}", &addr)).expect("Invalid URI"));

    let stream = move |listener: TcpListener| {
        futures::stream::unfold(Some(listener), move |listener| async move {
            let listener = listener?;

            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let local_addr = match stream.local_addr() {
                        Ok(addr) => addr,
                        Err(err) => {
                            warn!(
                                "Can't get local address for new connection on {} from {}",
                                addr, peer_addr
                            );
                            return Some((Err(err), Some(listener)));
                        }
                    };

                    debug!(
                        "Accepted new connection on {}/{} from {}",
                        addr, local_addr, peer_addr
                    );

                    let (read, write) = stream.split();

                    let stream = super::message_socket::async_read(read, max_size);
                    let sink = super::message_socket::async_write(write);

                    let stream: MessageStream = Box::pin(stream);
                    let sink: MessageSink = Box::pin(sink);

                    Some((
                        Ok(IncomingConnection {
                            stream,
                            sink,
                            connection_info: super::ConnectionInformation::new(
                                Some(local_addr),
                                Some(peer_addr),
                                Default::default(),
                            ),
                        }),
                        Some(listener),
                    ))
                }
                Err(err) => Some((Err(err), None)),
            }
        })
    };

    let setup = async move {
        debug!("Starting new TCP listener on {}", addr);
        let listener = TcpListener::bind(&addr).await.map_err(|err| {
            error!("Failed binding to address {}", addr);
            err
        })?;

        let stream: IncomingConnectionStream = Box::pin(stream(listener));

        Ok(stream)
    };

    Listener::new(id, setup)
}

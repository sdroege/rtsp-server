use rtsp_server::body::Body;
use rtsp_server::client::basic_client;
use rtsp_server::error::Error;
use rtsp_server::server::Server;
use rtsp_server::stream_handler::MessageHandler;

use gst::prelude::*;

use async_std::task;

use std::pin::Pin;
use std::sync::Arc;

use futures::executor::block_on;
use futures::lock::Mutex;
use futures::prelude::*;

use log::{error, warn};

#[derive(Copy, Clone, PartialEq, Eq)]
enum MediaState {
    Init,
    Ready,
    Playing,
}

struct Media {
    /// GStreamer pipeline.
    pipeline: gst::Pipeline,
    rtpbin: gst::Element,

    /// Keep track of the state of the single session:
    /// - transports can only be added/removed while not playing
    /// - only up to two transports (audio/video) are allowed
    state: MediaState,
    video_stream: Stream,
    audio_stream: Stream,
}

#[derive(Clone)]
struct Stream {
    /// SSRC, seqnum and rtptime configured on the payloader for this session.
    ssrc: u32,
    seqnum: u16,
    rtptime: u32,

    /// Sources/sinks for transmitting the data via TCP/interleaved.
    rtcp_src: gst_app::AppSrc,
    rtp_sink: gst_app::AppSink,
    rtcp_sink: gst_app::AppSink,

    rtp_sender: Arc<Mutex<Option<rtsp_server::client::DataSender>>>,
    rtcp_sender: Arc<Mutex<Option<rtsp_server::client::DataSender>>>,
}

struct DataReceiver {
    src: gst_app::AppSrc,
}

impl rtsp_server::client::DataReceiver for DataReceiver {
    fn handle_data(&mut self, data: rtsp_types::Data<Body>) {
        // TODO Handle errors?
        let _ = self
            .src
            .push_buffer(gst::Buffer::from_slice(data.into_body()));
    }

    fn closed(&mut self) {
        let _ = self.src.end_of_stream();
    }
}

struct DummyDataReceiver;
impl rtsp_server::client::DataReceiver for DummyDataReceiver {
    fn handle_data(&mut self, _data: rtsp_types::Data<Body>) {}
}

impl MessageHandler<gst::Message, ()> for Media {
    type Context = rtsp_server::media::Context<Self>;

    fn handle_message(&mut self, ctx: &mut Self::Context, _token: &(), msg: gst::Message) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Error(err) => {
                error!(
                    "Error from {:?}: {} ({:?})",
                    err.get_src().map(|s| s.get_path_string()),
                    err.get_error(),
                    err.get_debug()
                );
                ctx.error();
            }
            MessageView::Eos(_) => {
                // TODO: Need to provide actual range and end rtptimes, etc
                // FIXME: Only do this for RTSP 2.0
                let notify = rtsp_server::media::PlayNotifyMessage::EndOfStream {
                    range: rtsp_types::headers::Range::Npt(rtsp_types::headers::NptRange::FromTo(
                        rtsp_types::headers::NptTime::Seconds(0, None),
                        rtsp_types::headers::NptTime::Now,
                    )),
                    rtp_info: rtsp_types::headers::RtpInfos::V2(Vec::new()),
                    extra_data: Default::default(),
                };
                let fut = ctx.play_notify(notify);
                task::spawn(async move {
                    let _ = fut.await;
                });
            }
            MessageView::ClockLost(_) => {
                self.pipeline.call_async(|pipeline| {
                    let _ = pipeline.set_state(gst::State::Paused);
                    let _ = pipeline.set_state(gst::State::Playing);
                });
            }
            MessageView::Buffering(_) => {
                // TODO
            }
            MessageView::Latency(_) => {
                self.pipeline.call_async(|pipeline| {
                    let _ = pipeline.recalculate_latency();
                });
            }
            _ => (),
        }
    }
}

impl rtsp_server::media::Media for Media {
    const AUTOMATIC_IDLE: bool = true;

    fn startup(&mut self, ctx: &mut rtsp_server::media::Context<Self>) {
        ctx.register_stream((), self.pipeline.get_bus().expect("No bus").stream())
            .unwrap();
    }

    fn options(
        &mut self,
        _ctx: &mut rtsp_server::media::Context<Self>,
        _client_id: Option<rtsp_server::client::Id>,
        _stream_id: Option<rtsp_server::media::StreamId>,
        _supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        _extra_data: rtsp_server::typemap::TypeMap,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (
                            rtsp_types::headers::Supported,
                            rtsp_types::headers::Unsupported,
                            rtsp_server::typemap::TypeMap,
                        ),
                        Error,
                    >,
                > + Send,
        >,
    > {
        let supported = rtsp_types::headers::Supported::builder()
            .play_basic()
            .setup_rtp_rtcp_mux()
            .build();

        let mut unsupported = Vec::new();
        for require in &*require {
            if require != rtsp_types::headers::features::PLAY_BASIC {
                unsupported.push(String::from(require));
            }
        }

        Box::pin(async move {
            Ok((
                supported,
                rtsp_types::headers::Unsupported::from(unsupported),
                Default::default(),
            ))
        })
    }

    fn describe(
        &mut self,
        _ctx: &mut rtsp_server::media::Context<Self>,
        _client_id: Option<rtsp_server::client::Id>,
        _extra_data: rtsp_server::typemap::TypeMap,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(sdp_types::Session, rtsp_server::typemap::TypeMap), Error>>
                + Send,
        >,
    > {
        Box::pin(async move { Ok((create_sdp(), Default::default())) })
    }

    fn add_transport(
        &mut self,
        ctx: &mut rtsp_server::media::Context<Self>,
        _client_id: rtsp_server::client::Id,
        session_id: rtsp_server::server::SessionId,
        stream_id: rtsp_server::media::StreamId,
        transports: rtsp_types::headers::Transports,
        _extra_data: rtsp_server::typemap::TypeMap,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_server::media::ConfiguredTransport, Error>> + Send>>
    {
        let mut client = match ctx.find_session_client(&session_id) {
            Some(client) => client,
            None => {
                return Box::pin(async {
                    Err(rtsp_server::error::ErrorStatus::from(
                        rtsp_types::StatusCode::SessionNotFound,
                    )
                    .into())
                });
            }
        };

        let stream = if stream_id.as_str() == "audio" {
            self.audio_stream.clone()
        } else if stream_id.as_str() == "video" {
            self.video_stream.clone()
        } else {
            return Box::pin(async {
                Err(rtsp_server::error::ErrorStatus::from(rtsp_types::StatusCode::NotFound).into())
            });
        };

        // TODO: We only support TCP-interleaved for now
        let mut suitable_transport = None;
        for transport in &*transports {
            match transport {
                rtsp_types::headers::Transport::Rtp(rtp) => {
                    if rtp.lower_transport
                        == Some(rtsp_types::headers::transport::RtpLowerTransport::Tcp)
                    {
                        if rtp.params.interleaved.is_some() {
                            suitable_transport = Some(rtp);
                        }
                    }
                }
                _ => (),
            }
        }

        let mut suitable_transport = match suitable_transport {
            Some(transport) => transport.clone(),
            None => {
                return Box::pin(async {
                    Err(rtsp_server::error::ErrorStatus::from(
                        rtsp_types::StatusCode::UnsupportedTransport,
                    )
                    .into())
                });
            }
        };

        let handle = ctx.handle();
        let fut = async move {
            let mut data_receiver = Vec::<Box<dyn rtsp_server::client::DataReceiver>>::new();
            data_receiver.push(Box::new(DummyDataReceiver));

            let interleaved = suitable_transport.params.interleaved.unwrap();

            if let Some(channel_end) = interleaved.1 {
                if channel_end == interleaved.0 + 1 {
                    data_receiver.push(Box::new(DataReceiver {
                        src: stream.rtcp_src.clone(),
                    }));
                } else {
                    return Err(rtsp_server::error::ErrorStatus::from(
                        rtsp_types::StatusCode::UnsupportedTransport,
                    )
                    .into());
                }
            }

            let (channel_start, senders, _) = client
                .register_interleaved_channel(
                    session_id.clone(),
                    Some(interleaved.0),
                    data_receiver,
                    Default::default(),
                )
                .await?;

            // Update channel id as needed
            suitable_transport.params.interleaved = Some((
                channel_start,
                if senders.len() == 1 {
                    None
                } else {
                    Some(channel_start + senders.len() as u8 - 1)
                },
            ));
            suitable_transport.params.ssrc = vec![stream.ssrc];
            suitable_transport.params.mode = vec![rtsp_types::headers::TransportMode::Play];

            let mut rtp_sender = stream.rtp_sender.lock().await;
            let mut rtcp_sender = stream.rtcp_sender.lock().await;

            if rtp_sender.is_some() || rtcp_sender.is_some() {
                return Err(rtsp_server::error::ErrorStatus::from(
                    rtsp_types::StatusCode::UnsupportedTransport,
                )
                .into());
            }

            let mut senders = senders.into_iter();
            *rtp_sender = Some(senders.next().unwrap());
            *rtcp_sender = senders.next();
            drop(rtp_sender);
            drop(rtcp_sender);

            let configured_transport = rtsp_server::media::ConfiguredTransport {
                session_id,
                media_id: handle.id(),
                stream_id,
                extra_data: Default::default(),
                transport: suitable_transport.clone(),
            };

            Ok(configured_transport)
        };

        Box::pin(fut)
    }

    fn play(
        &mut self,
        ctx: &mut rtsp_server::media::Context<Self>,
        _client_id: rtsp_server::client::Id,
        session_id: rtsp_server::server::SessionId,
        _stream_id: Option<rtsp_server::media::StreamId>,
        range: Option<rtsp_types::headers::Range>,
        extra_data: rtsp_server::typemap::TypeMap,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (
                            rtsp_types::headers::Range,
                            rtsp_types::headers::RtpInfos,
                            rtsp_server::typemap::TypeMap,
                        ),
                        Error,
                    >,
                > + Send,
        >,
    > {
        let _client = match ctx.find_session_client(&session_id) {
            Some(client) => client,
            None => {
                return Box::pin(async {
                    Err(rtsp_server::error::ErrorStatus::from(
                        rtsp_types::StatusCode::SessionNotFound,
                    )
                    .into())
                });
            }
        };

        let video_stream = self.video_stream.clone();
        let audio_stream = self.audio_stream.clone();
        let req = extra_data
            .get::<rtsp_server::client::OriginalRequest>()
            .unwrap()
            .clone();

        let pipeline = self.pipeline.clone();

        let fut = async move {
            use rtsp_types::headers::{rtp_info, NptRange, NptTime, Range, RtpInfos};

            // Check valid ranges
            // TODO: Handle range
            if let Some(range) = range {
                match range {
                    Range::Npt(npt) => match npt {
                        NptRange::From(NptTime::Seconds(seconds, nanoseconds))
                            if seconds == 0 && nanoseconds == None || nanoseconds == Some(0) => {}
                        NptRange::From(NptTime::Hms(hours, minutes, seconds, nanoseconds))
                            if hours == 0
                                && minutes == 0
                                && seconds == 0
                                && nanoseconds == None
                                || nanoseconds == Some(0) => {}
                        NptRange::From(NptTime::Now) => {}
                        _ => {
                            return Err(rtsp_server::error::ErrorStatus::from(
                                rtsp_types::StatusCode::InvalidRange,
                            )
                            .into());
                        }
                    },
                    _ => {
                        return Err(rtsp_server::error::ErrorStatus::from(
                            rtsp_types::StatusCode::InvalidRange,
                        )
                        .into());
                    }
                }
            }

            let rtp_infos = if req.version() == rtsp_types::Version::V1_0 {
                let mut rtp_infos = Vec::new();
                for (stream_id, stream) in &[("video", &video_stream), ("audio", &audio_stream)] {
                    let rtp_sender = stream.rtp_sender.lock().await;
                    if rtp_sender.is_some() {
                        rtp_infos.push(rtp_info::v1::RtpInfo {
                            uri: req.request_uri().unwrap().join(stream_id).unwrap(),
                            seq: Some(stream.seqnum),
                            rtptime: Some(stream.rtptime),
                        });
                    }
                    drop(rtp_sender);
                }
                RtpInfos::V1(rtp_infos)
            } else {
                let mut rtp_infos = Vec::new();
                for (stream_id, stream) in &[("video", &video_stream), ("audio", &audio_stream)] {
                    let rtp_sender = stream.rtp_sender.lock().await;
                    if rtp_sender.is_some() {
                        rtp_infos.push(rtp_info::v2::RtpInfo {
                            uri: req.request_uri().unwrap().join(stream_id).unwrap(),
                            ssrc_infos: vec![rtp_info::v2::SsrcInfo {
                                ssrc: stream.ssrc,
                                seq: Some(stream.seqnum),
                                rtptime: Some(stream.rtptime),
                                others: Default::default(),
                            }],
                        });
                    }
                    drop(rtp_sender);
                }
                RtpInfos::V2(rtp_infos)
            };

            if let Err(_) = pipeline
                .call_async_future(|pipeline| pipeline.set_state(gst::State::Playing))
                .await
            {
                return Err(rtsp_server::error::InternalServerError.into());
            }

            Ok((
                Range::Npt(NptRange::From(NptTime::Seconds(0, None))),
                rtp_infos,
                Default::default(),
            ))
        };

        Box::pin(fut)
    }

    fn pause(
        &mut self,
        _ctx: &mut rtsp_server::media::Context<Self>,
        _client_id: rtsp_server::client::Id,
        _session_id: rtsp_server::server::SessionId,
        _stream_id: Option<rtsp_server::media::StreamId>,
        _extra_data: rtsp_server::typemap::TypeMap,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (rtsp_types::headers::Range, rtsp_server::typemap::TypeMap),
                        Error,
                    >,
                > + Send,
        >,
    > {
        // TODO: Implement
        Box::pin(async {
            Err(
                rtsp_server::error::ErrorStatus::from(rtsp_types::StatusCode::MethodNotAllowed)
                    .into(),
            )
        })
    }

    fn remove_transport(
        &mut self,
        ctx: &mut rtsp_server::media::Context<Self>,
        _client_id: rtsp_server::client::Id,
        _session_id: rtsp_server::server::SessionId,
        stream_id: rtsp_server::media::StreamId,
        _extra_data: rtsp_server::typemap::TypeMap,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send>> {
        let (stream, other_stream) = if stream_id.as_str() == "audio" {
            (self.audio_stream.clone(), self.video_stream.clone())
        } else if stream_id.as_str() == "video" {
            (self.video_stream.clone(), self.audio_stream.clone())
        } else {
            return Box::pin(async {
                Err(rtsp_server::error::ErrorStatus::from(rtsp_types::StatusCode::NotFound).into())
            });
        };

        let mut handle = ctx.handle();
        let fut = async move {
            if let Some(mut sender) = stream.rtp_sender.lock().await.take() {
                sender.close().await;
            }
            if let Some(mut sender) = stream.rtcp_sender.lock().await.take() {
                sender.close().await;
            }

            // FIXME: Shut down the whole session and media if no transports are left?
            let other_stream_rtp_sender = other_stream.rtp_sender.lock().await;
            let other_stream_rtcp_sender = other_stream.rtcp_sender.lock().await;

            if other_stream_rtp_sender.is_none() && other_stream_rtcp_sender.is_none() {
                drop(other_stream_rtp_sender);
                drop(other_stream_rtcp_sender);

                let _ = handle.shutdown().await;
            }

            Ok(())
        };

        Box::pin(fut)
    }

    fn shutdown_session(
        &mut self,
        ctx: &mut rtsp_server::media::Context<Self>,
        _client_id: rtsp_server::client::Id,
        _session_id: rtsp_server::server::SessionId,
        _extra_data: rtsp_server::typemap::TypeMap,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send>> {
        Box::pin(self.shutdown(ctx).map(|_| Ok(())))
    }

    fn shutdown(
        &mut self,
        _ctx: &mut rtsp_server::media::Context<Self>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let fut = self.pipeline.call_async_future(|pipeline| {
            let _ = pipeline.set_state(gst::State::Null);
        });

        Box::pin(fut)
    }
}

struct MediaFactory {
    base_path: String,
}

impl rtsp_server::media_factory::MediaFactory for MediaFactory {
    fn options(
        &mut self,
        _ctx: &mut rtsp_server::media_factory::Context<Self>,
        _stream_id: Option<rtsp_server::media::StreamId>,
        _supported: rtsp_types::headers::Supported,
        require: rtsp_types::headers::Require,
        _extra_data: rtsp_server::typemap::TypeMap,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (
                            rtsp_types::headers::Allow,
                            rtsp_types::headers::Supported,
                            rtsp_types::headers::Unsupported,
                            rtsp_server::typemap::TypeMap,
                        ),
                        Error,
                    >,
                > + Send,
        >,
    > {
        // TODO: Other factories might have to ask their medias too
        // TODO: check URI?
        let allowed = rtsp_types::headers::Allow::builder()
            .method(rtsp_types::Method::Options)
            .method(rtsp_types::Method::Describe)
            .method(rtsp_types::Method::Setup)
            .method(rtsp_types::Method::Play)
            .method(rtsp_types::Method::Pause)
            .method(rtsp_types::Method::Teardown)
            .method(rtsp_types::Method::GetParameter)
            .method(rtsp_types::Method::SetParameter)
            .build();

        let supported = rtsp_types::headers::Supported::builder()
            .play_basic()
            .setup_rtp_rtcp_mux()
            .build();

        let mut unsupported = Vec::new();
        for require in &*require {
            if require != rtsp_types::headers::features::PLAY_BASIC {
                unsupported.push(String::from(require));
            }
        }

        Box::pin(async move {
            Ok((
                allowed,
                supported,
                rtsp_types::headers::Unsupported::from(unsupported),
                Default::default(),
            ))
        })
    }

    fn describe(
        &mut self,
        _ctx: &mut rtsp_server::media_factory::Context<Self>,
        _extra_data: rtsp_server::typemap::TypeMap,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(sdp_types::Session, rtsp_server::typemap::TypeMap), Error>>
                + Send,
        >,
    > {
        // TODO: check URI?
        Box::pin(async move { Ok((create_sdp(), Default::default())) })
    }

    fn create_media(
        &mut self,
        ctx: &mut rtsp_server::media_factory::Context<Self>,
        client_id: rtsp_server::client::Id,
        _extra_data: rtsp_server::typemap::TypeMap,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (
                            rtsp_server::media_factory::MediaHandle<Self>,
                            rtsp_server::typemap::TypeMap,
                        ),
                        Error,
                    >,
                > + Send,
        >,
    > {
        // TODO: check URI etc?

        let mut handle = ctx.handle();

        Box::pin(async move {
            let pipeline = gst::Pipeline::new(Some(&client_id.to_string()));
            let rtpbin = gst::ElementFactory::make("rtpbin", Some("rtpbin")).unwrap();
            pipeline.add(&rtpbin).unwrap();

            // Create video part
            let video_stream = {
                use rand::prelude::*;

                let src = gst::ElementFactory::make("videotestsrc", None).unwrap();
                let enc = gst::ElementFactory::make("vp8enc", None).unwrap();
                let pay = gst::ElementFactory::make("rtpvp8pay", None).unwrap();

                let mut rng = rand::thread_rng();
                let ssrc = rng.gen::<u32>();
                let rtptime = rng.gen::<u32>();
                let seqnum = rng.gen::<u16>();

                pay.set_property("pt", &96u32).unwrap();
                pay.set_property("ssrc", &ssrc).unwrap();
                pay.set_property("timestamp-offset", &rtptime).unwrap();
                pay.set_property("seqnum-offset", &(seqnum as i32)).unwrap();

                pipeline.add_many(&[&src, &enc, &pay]).unwrap();
                gst::Element::link_many(&[&src, &enc, &pay]).unwrap();

                // FIXME: Only create RTCP elements if requested
                // TODO: data senders, need to keep both alive
                let rtcp_src = gst::ElementFactory::make("appsrc", None)
                    .unwrap()
                    .downcast::<gst_app::AppSrc>()
                    .unwrap();

                rtcp_src.set_property("format", &gst::Format::Time).unwrap();
                rtcp_src.set_property("do-timestamp", &true).unwrap();
                rtcp_src.set_property("is-live", &true).unwrap();

                let rtp_sink = gst::ElementFactory::make("appsink", None)
                    .unwrap()
                    .downcast::<gst_app::AppSink>()
                    .unwrap();

                // FIXME: RTCP-MUX requires passing the RTP sender to the RTCP appsink

                let rtp_sender = Arc::new(Mutex::new(None::<rtsp_server::client::DataSender>));
                let rtp_sender_clone = rtp_sender.clone();
                rtp_sink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |appsink| {
                            let sample = match appsink.pull_sample() {
                                Ok(sample) => sample,
                                Err(_) => return Err(gst::FlowError::Flushing),
                            };

                            // FIXME: Handle multiple senders, dropping of data when slow
                            // FIXME: The locking is suboptimal
                            block_on(async {
                                if let Some(sender) = &mut *rtp_sender_clone.lock().await {
                                    let buffer = sample.get_buffer_owned().unwrap();
                                    let buffer = buffer.into_mapped_buffer_readable().unwrap();
                                    let _ = sender
                                        .send_data(rtsp_server::body::Body::custom(buffer))
                                        .await;
                                }
                            });

                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );

                let rtcp_sink = gst::ElementFactory::make("appsink", None)
                    .unwrap()
                    .downcast::<gst_app::AppSink>()
                    .unwrap();
                rtcp_sink.set_property("async", &false).unwrap();
                rtcp_sink.set_property("sync", &false).unwrap();

                let rtcp_sender = Arc::new(Mutex::new(None::<rtsp_server::client::DataSender>));
                let rtcp_sender_clone = rtcp_sender.clone();
                rtcp_sink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |appsink| {
                            let sample = match appsink.pull_sample() {
                                Ok(sample) => sample,
                                Err(_) => return Err(gst::FlowError::Flushing),
                            };

                            // FIXME: Handle multiple senders, dropping of data when slow
                            // FIXME: The locking is suboptimal
                            block_on(async {
                                if let Some(sender) = &mut *rtcp_sender_clone.lock().await {
                                    let buffer = sample.get_buffer_owned().unwrap();
                                    let buffer = buffer.into_mapped_buffer_readable().unwrap();
                                    let _ = sender
                                        .send_data(rtsp_server::body::Body::custom(buffer))
                                        .await;
                                }
                            });

                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );

                pipeline.add(&rtcp_src).unwrap();
                pipeline.add(&rtp_sink).unwrap();
                pipeline.add(&rtcp_sink).unwrap();

                let send_rtp_sink = rtpbin.get_request_pad("send_rtp_sink_%u").unwrap();
                let rtpsession_id = &send_rtp_sink.get_name()[14..];
                pay.link_pads(
                    Some("src"),
                    &rtpbin,
                    Some(&format!("send_rtp_sink_{}", rtpsession_id)),
                )
                .unwrap();
                rtpbin
                    .link_pads(
                        Some(&format!("send_rtp_src_{}", rtpsession_id)),
                        &rtp_sink,
                        Some("sink"),
                    )
                    .unwrap();
                rtpbin
                    .link_pads(
                        Some(&format!("send_rtcp_src_{}", rtpsession_id)),
                        &rtcp_sink,
                        Some("sink"),
                    )
                    .unwrap();
                rtcp_src
                    .link_pads(
                        Some("src"),
                        &rtpbin,
                        Some(&format!("recv_rtcp_sink_{}", rtpsession_id)),
                    )
                    .unwrap();

                Stream {
                    ssrc,
                    seqnum,
                    rtptime,
                    rtcp_src,
                    rtp_sink,
                    rtcp_sink,
                    rtp_sender,
                    rtcp_sender,
                }
            };

            // Create audio part
            let audio_stream = {
                use rand::prelude::*;

                let src = gst::ElementFactory::make("audiotestsrc", None).unwrap();
                let enc = gst::ElementFactory::make("opusenc", None).unwrap();
                let pay = gst::ElementFactory::make("rtpopuspay", None).unwrap();

                let mut rng = rand::thread_rng();
                let ssrc = rng.gen::<u32>();
                let rtptime = rng.gen::<u32>();
                let seqnum = rng.gen::<u16>();

                pay.set_property("pt", &97u32).unwrap();
                pay.set_property("ssrc", &ssrc).unwrap();
                pay.set_property("timestamp-offset", &rtptime).unwrap();
                pay.set_property("seqnum-offset", &(seqnum as i32)).unwrap();

                pipeline.add_many(&[&src, &enc, &pay]).unwrap();
                gst::Element::link_many(&[&src, &enc, &pay]).unwrap();

                // FIXME: Only create RTCP elements if requested
                // TODO: data senders, need to keep both alive
                let rtcp_src = gst::ElementFactory::make("appsrc", None)
                    .unwrap()
                    .downcast::<gst_app::AppSrc>()
                    .unwrap();

                rtcp_src.set_property("format", &gst::Format::Time).unwrap();
                rtcp_src.set_property("do-timestamp", &true).unwrap();
                rtcp_src.set_property("is-live", &true).unwrap();

                let rtp_sink = gst::ElementFactory::make("appsink", None)
                    .unwrap()
                    .downcast::<gst_app::AppSink>()
                    .unwrap();

                let rtp_sender = Arc::new(Mutex::new(None::<rtsp_server::client::DataSender>));
                let rtp_sender_clone = rtp_sender.clone();
                rtp_sink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |appsink| {
                            let sample = match appsink.pull_sample() {
                                Ok(sample) => sample,
                                Err(_) => return Err(gst::FlowError::Flushing),
                            };

                            // FIXME: Handle multiple senders, dropping of data when slow
                            // FIXME: The locking is suboptimal
                            block_on(async {
                                if let Some(sender) = &mut *rtp_sender_clone.lock().await {
                                    let buffer = sample.get_buffer_owned().unwrap();
                                    let buffer = buffer.into_mapped_buffer_readable().unwrap();
                                    let _ = sender
                                        .send_data(rtsp_server::body::Body::custom(buffer))
                                        .await;
                                }
                            });

                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );

                let rtcp_sink = gst::ElementFactory::make("appsink", None)
                    .unwrap()
                    .downcast::<gst_app::AppSink>()
                    .unwrap();
                rtcp_sink.set_property("async", &false).unwrap();
                rtcp_sink.set_property("sync", &false).unwrap();

                let rtcp_sender = Arc::new(Mutex::new(None::<rtsp_server::client::DataSender>));
                let rtcp_sender_clone = rtcp_sender.clone();
                rtcp_sink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |appsink| {
                            let sample = match appsink.pull_sample() {
                                Ok(sample) => sample,
                                Err(_) => return Err(gst::FlowError::Flushing),
                            };

                            // FIXME: Handle multiple senders, dropping of data when slow
                            // FIXME: The locking is suboptimal
                            block_on(async {
                                if let Some(sender) = &mut *rtcp_sender_clone.lock().await {
                                    let buffer = sample.get_buffer_owned().unwrap();
                                    let buffer = buffer.into_mapped_buffer_readable().unwrap();
                                    let _ = sender
                                        .send_data(rtsp_server::body::Body::custom(buffer))
                                        .await;
                                }
                            });

                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );

                pipeline.add(&rtcp_src).unwrap();
                pipeline.add(&rtp_sink).unwrap();
                pipeline.add(&rtcp_sink).unwrap();

                let send_rtp_sink = rtpbin.get_request_pad("send_rtp_sink_%u").unwrap();
                let rtpsession_id = &send_rtp_sink.get_name()[14..];
                pay.link_pads(
                    Some("src"),
                    &rtpbin,
                    Some(&format!("send_rtp_sink_{}", rtpsession_id)),
                )
                .unwrap();
                rtpbin
                    .link_pads(
                        Some(&format!("send_rtp_src_{}", rtpsession_id)),
                        &rtp_sink,
                        Some("sink"),
                    )
                    .unwrap();
                rtpbin
                    .link_pads(
                        Some(&format!("send_rtcp_src_{}", rtpsession_id)),
                        &rtcp_sink,
                        Some("sink"),
                    )
                    .unwrap();
                rtcp_src
                    .link_pads(
                        Some("src"),
                        &rtpbin,
                        Some(&format!("recv_rtcp_sink_{}", rtpsession_id)),
                    )
                    .unwrap();

                Stream {
                    ssrc,
                    seqnum,
                    rtptime,
                    rtcp_src,
                    rtp_sink,
                    rtcp_sink,
                    rtp_sender,
                    rtcp_sender,
                }
            };

            let media = Media {
                pipeline,
                rtpbin,
                state: MediaState::Init,
                audio_stream,
                video_stream,
            };

            let media = handle.spawn_media(None, media).await?;

            Ok((media, Default::default()))
        })
    }

    fn media_idle(
        &mut self,
        ctx: &mut rtsp_server::media_factory::Context<Self>,
        media_id: rtsp_server::media::Id,
        idle: bool,
        _extra_data: rtsp_server::typemap::TypeMap,
    ) -> Pin<Box<dyn Future<Output = Result<rtsp_server::typemap::TypeMap, Error>> + Send>> {
        let mut media = None;
        if idle {
            media = ctx.find_media(media_id);
        }

        Box::pin(async {
            if let Some(mut media) = media {
                let _ = media.shutdown().await;
            }

            Ok(Default::default())
        })
    }
}

fn main() {
    gst::init().unwrap();

    env_logger::init();

    let mounts = rtsp_server::server::Mounts::builder()
        .path("/test", |_id| MediaFactory {
            base_path: String::from("/test"),
        })
        .build();

    let server =
        Server::builder(|_client_id, _connection_info| Some(basic_client::Client::default()))
            .bind_tcp("0.0.0.0:8554".parse().unwrap())
            .mounts(mounts)
            .run();

    task::block_on(async move {
        task::sleep(std::time::Duration::from_secs(30)).await;

        server.shutdown().await;
    });
}

// TODO: Generate this on-the-fly
fn create_sdp() -> sdp_types::Session {
    sdp_types::Session {
        origin: sdp_types::Origin {
            username: None,
            sess_id: {
                use rand::prelude::*;

                let mut rng = rand::thread_rng();
                rng.gen::<u64>().to_string()
            },
            sess_version: 1,
            nettype: "IN".into(),
            addrtype: "IP4".into(),
            unicast_address: "127.0.0.1".into(),
        },
        session_name: "Session streamed with GStreamer".into(),
        session_description: Some("rtsp-server".into()),
        uri: None,
        emails: vec![],
        phones: vec![],
        connection: None,
        bandwidths: vec![],
        times: vec![sdp_types::Time {
            start_time: 0,
            stop_time: 0,
            repeats: vec![],
        }],
        time_zones: vec![],
        key: None,
        attributes: vec![
            sdp_types::Attribute {
                attribute: "tool".into(),
                value: Some("GStreamer".into()),
            },
            sdp_types::Attribute {
                attribute: "type".into(),
                value: Some("broadcast".into()),
            },
            sdp_types::Attribute {
                attribute: "control".into(),
                value: Some("*".into()),
            },
            sdp_types::Attribute {
                attribute: "range".into(),
                value: Some(
                    rtsp_types::headers::Range::Npt(rtsp_types::headers::NptRange::From(
                        rtsp_types::headers::NptTime::Seconds(0, None),
                    ))
                    .to_string(),
                ),
            },
        ],
        medias: vec![
            sdp_types::Media {
                media: "video".into(),
                port: 0,
                num_ports: None,
                proto: "RTP/AVP".into(),
                fmt: "96".into(),
                media_title: None,
                connections: vec![sdp_types::Connection {
                    nettype: "IN".into(),
                    addrtype: "IP4".into(),
                    connection_address: "0.0.0.0".into(),
                }],
                bandwidths: vec![],
                key: None,
                attributes: vec![
                    sdp_types::Attribute {
                        attribute: "rtpmap".into(),
                        value: Some("96 VP8/90000".into()),
                    },
                    sdp_types::Attribute {
                        attribute: "control".into(),
                        value: Some("video".into()),
                    },
                ],
            },
            sdp_types::Media {
                media: "audio".into(),
                port: 0,
                num_ports: None,
                proto: "RTP/AVP".into(),
                fmt: "97".into(),
                media_title: None,
                connections: vec![sdp_types::Connection {
                    nettype: "IN".into(),
                    addrtype: "IP4".into(),
                    connection_address: "0.0.0.0".into(),
                }],
                bandwidths: vec![],
                key: None,
                attributes: vec![
                    sdp_types::Attribute {
                        attribute: "rtpmap".into(),
                        value: Some("97 OPUS/48000/2".into()),
                    },
                    sdp_types::Attribute {
                        attribute: "fmtp".into(),
                        value: Some("97 sprop-maxcapturerate=48000;sprop-stereo=0".into()),
                    },
                    sdp_types::Attribute {
                        attribute: "control".into(),
                        value: Some("audio".into()),
                    },
                ],
            },
        ],
    }
}

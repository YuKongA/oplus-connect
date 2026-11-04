//! GStreamer WebRTC desktop streaming sender for tablet secondary display.
use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use gstreamer_video::prelude::*;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How often the sender logs capture and encode frame rates.
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalingMessage {
    Sdp {
        type_: String,
        sdp: String,
    },
    Ice {
        candidate: String,
        sdp_mline_index: u32,
        mid: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// Encoder output size, i.e. the tablet panel the peer renders into.
    pub width: u32,
    pub height: u32,
    /// Aspect-preserving content box inside `width`x`height`; the remainder is
    /// filled with black so a 16:9 desktop is not stretched onto a 3:2 panel.
    pub render_width: u32,
    pub render_height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Size to request from a compositor-created virtual output, so the desktop
    /// itself has the tablet's aspect ratio instead of being letterboxed.
    pub capture_width: Option<u32>,
    pub capture_height: Option<u32>,
    pub pipewire_fd: Option<i32>,
    pub pipewire_node: Option<u32>,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            width: 2016,
            height: 1344,
            render_width: 2016,
            render_height: 1344,
            fps: 30,
            bitrate_kbps: 6000,
            capture_width: None,
            capture_height: None,
            pipewire_fd: None,
            pipewire_node: None,
        }
    }
}

pub struct MediaSender {
    pub incoming: mpsc::Sender<SignalingMessage>,
    pub outgoing: mpsc::Receiver<SignalingMessage>,
    pub stop: Arc<AtomicBool>,
}

pub fn spawn_sender(config: SenderConfig) -> Result<MediaSender> {
    gst::init().context("initializing GStreamer for ExpandCast")?;

    let (incoming_tx, incoming_rx) = mpsc::channel(64);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(64);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();

    std::thread::Builder::new()
        .name("oplus-expand-cast-sender".into())
        .spawn(move || {
            let mut incoming_rx = incoming_rx;
            let mut last_error = None;
            // Pinning the capture format to the tablet's size needs compositor
            // support that this stack does not have: KWin 6.7 returns
            // "Element failed to change its state" and the portal's ScreenCast
            // v5 exposes no SetSize. Keep it behind an opt-in env for the day
            // that changes; the default path follows the compositor.
            let pin_requested =
                std::env::var("OPLUS_EXPAND_CAST_PIN_CAPTURE").as_deref() == Ok("1");
            let attempts: &[bool] = if pin_requested { &[true, false] } else { &[false] };
            for &pin_capture in attempts {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_pipeline(
                        &mut incoming_rx,
                        outgoing_tx.clone(),
                        &stop_thread,
                        &config,
                        pin_capture,
                    )
                }));
                match outcome {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => last_error = Some(error),
                    Err(_) => {
                        tracing::error!("ExpandCast sender thread panicked; screen sharing stopped");
                        return;
                    }
                }
                if stop_thread.load(Ordering::Acquire) {
                    return;
                }
                if pin_capture {
                    tracing::warn!(
                        "capture format pinning unavailable ({:#}); following the compositor instead",
                        last_error.as_ref().expect("error recorded")
                    );
                }
            }
            if let Some(error) = last_error {
                tracing::error!("ExpandCast sender pipeline error: {error:#}");
            }
        })
        .context("spawning ExpandCast sender thread")?;

    Ok(MediaSender {
        incoming: incoming_tx,
        outgoing: outgoing_rx,
        stop,
    })
}

fn run_pipeline(
    incoming_rx: &mut mpsc::Receiver<SignalingMessage>,
    outgoing_tx: mpsc::Sender<SignalingMessage>,
    stop: &AtomicBool,
    config: &SenderConfig,
    pin_capture: bool,
) -> Result<()> {
    let pipeline = gst::Pipeline::new();

    // Video Source: the portal-granted PipeWire node when available. `pipewiresrc`
    // selects a remote node by numeric id through `path`; `target-object` matches
    // the node *name* and fails with "target not found" for portal node ids.
    let (src, is_pipewire) = if let (Some(fd), Some(node)) = (config.pipewire_fd, config.pipewire_node) {
        // pipewiresrc hands the descriptor to pipewire, which owns and closes it.
        // Each attempt needs its own duplicate, or a retry would use a descriptor
        // that the failed attempt already released.
        let fd = duplicate_fd(fd)?;
        let src = gst::ElementFactory::make("pipewiresrc")
            .property("fd", fd)
            .property("path", node.to_string())
            .property("do-timestamp", true)
            // Re-send the last frame while the desktop is idle so the cadence
            // stays even instead of bursting only when damage occurs.
            // The property is a gint, not a guint.
            .property("keepalive-time", 33i32)
            .build()
            .context("creating pipewiresrc element")?;
        (src, true)
    } else if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland") {
        // On Wayland the XWayland root window is an empty black canvas, so an
        // X11 capture would only ever show the pointer. Surface the real cause.
        anyhow::bail!("未获得系统屏幕共享授权，无法采集 Wayland 桌面画面");
    } else if gst::ElementFactory::find("ximagesrc").is_some() && std::env::var("DISPLAY").is_ok() {
        tracing::info!("Using ximagesrc for direct desktop capture");
        let src = gst::ElementFactory::make("ximagesrc")
            .property("use-damage", false)
            .build()
            .context("creating ximagesrc element")?;
        (src, false)
    } else {
        tracing::info!("Using videotestsrc pattern for video stream");
        let src = gst::ElementFactory::make("videotestsrc")
            .property_from_str("pattern", "ball")
            .property("is-live", true)
            .build()
            .context("creating videotestsrc element")?;
        (src, false)
    };

    // Opt-in only: ask the capture node for the tablet's own size so a virtual
    // output would be 3:2 instead of the primary monitor's aspect. Verified
    // rejected on KWin 6.7 ("Element failed to change its state"), so the
    // default path letterboxes instead.
    let pinned_caps = if is_pipewire && pin_capture {
        let mut builder = gst::Caps::builder("video/x-raw");
        if let (Some(width), Some(height)) = (config.capture_width, config.capture_height) {
            builder = builder
                .field("width", (width & !1).max(2) as i32)
                .field("height", (height & !1).max(2) as i32);
        }
        let caps = builder.build();
        tracing::info!("ExpandCast: requesting capture caps {caps}");
        Some(
            gst::ElementFactory::make("capsfilter")
                .property("caps", &caps)
                .build()
                .context("creating capture caps filter")?,
        )
    } else {
        None
    };

    // A blocking sink anywhere downstream stalls `pipewiresrc` itself (measured:
    // 0.5 fps captured with webrtcbin attached versus 22 fps without). This leaky
    // queue absorbs that so the compositor keeps producing at its own rate.
    let capture_queue = gst::ElementFactory::make("queue")
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 2u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .build()
        .context("creating capture queue")?;

    // Scale and colour-convert on the GPU when VA-API post-processing exists:
    // 4K BGRA -> tablet-sized NV12 through videoconvert/videoscale costs far more
    // CPU than real-time (measured ~32 fps versus ~88 fps).
    let scalers: Vec<gst::Element> = if gst::ElementFactory::find("vapostproc").is_some() {
        tracing::info!("ExpandCast: scaling with VA-API post-processing");
        vec![
            gst::ElementFactory::make("vapostproc")
                .build()
                .context("creating vapostproc")?,
        ]
    } else {
        tracing::info!("ExpandCast: scaling on the CPU (videoconvert/videoscale)");
        vec![
            gst::ElementFactory::make("videoconvert")
                .build()
                .context("creating videoconvert")?,
            gst::ElementFactory::make("videoscale")
                .build()
                .context("creating videoscale")?,
        ]
    };
    // Keep a steady cadence even though the capture source reports 0/1.
    let rate = gst::ElementFactory::make("videorate").build().context("creating videorate")?;
    // Choose VA-API H.264 hardware encoder if available, otherwise x264enc
    let (enc, _is_va) = if gst::ElementFactory::find("vah264enc").is_some() {
        let enc = gst::ElementFactory::make("vah264enc")
            .property("bitrate", config.bitrate_kbps)
            // One keyframe per second keeps recovery time short on Wi-Fi loss.
            .property("key-int-max", config.fps.max(1) as u32)
            // A single reference frame removes reorder delay and encode cost;
            // remote-desktop content does not benefit from a deeper GOP.
            .property("ref-frames", 1u32)
            .property("target-usage", 7u32)
            .build()
            .context("creating vah264enc")?;
        (enc, true)
    } else {
        let enc = gst::ElementFactory::make("x264enc")
            .property("bitrate", config.bitrate_kbps)
            .property_from_str("tune", "zerolatency")
            .property_from_str("speed-preset", "ultrafast")
            .property("key-int-max", config.fps)
            .build()
            .context("creating x264enc")?;
        (enc, false)
    };

    let parse = gst::ElementFactory::make("h264parse")
        .property("config-interval", 1i32)
        .build()
        .context("creating h264parse")?;

    let pay = gst::ElementFactory::make("rtph264pay")
        .property("config-interval", 1i32)
        .property("pt", 100u32) // Tablet accepts PT 100
        .build()
        .context("creating rtph264pay")?;

    let webrtc = gst::ElementFactory::make("webrtcbin")
        .property_from_str("bundle-policy", "max-bundle")
        .property_from_str("latency", "0")
        .build()
        .context("creating webrtcbin")?;

    // When the compositor accepted our size request the captured desktop already
    // matches the panel, so there is nothing to letterbox.
    let pinned_size = if pin_capture {
        config
            .capture_width
            .zip(config.capture_height)
            .map(|(w, h)| ((w & !1).max(2), (h & !1).max(2)))
    } else {
        None
    };
    let render_width = pinned_size
        .map(|(w, _)| w)
        .unwrap_or(config.render_width & !1);
    let render_height = pinned_size
        .map(|(_, h)| h)
        .unwrap_or(config.render_height & !1);
    let (out_width, out_height) = if pinned_size.is_some() {
        (render_width, render_height)
    } else {
        (config.width, config.height)
    };
    let pad_x = (out_width.saturating_sub(render_width)) / 2;
    let pad_y = (out_height.saturating_sub(render_height)) / 2;
    let needs_pad = pad_x > 0 || pad_y > 0;

    // `videobox` cannot pad NV12, so letterboxed content runs through I420 and
    // is converted to the encoder's NV12 afterwards (on the GPU when available).
    let padding = if needs_pad {
        let boxx = gst::ElementFactory::make("videobox")
            .property("left", -(pad_x as i32))
            .property("right", -(pad_x as i32))
            .property("top", -(pad_y as i32))
            .property("bottom", -(pad_y as i32))
            .property_from_str("fill", "black")
            .property("autocrop", false)
            .build()
            .context("creating videobox")?;
        let converter = if gst::ElementFactory::find("vapostproc").is_some() {
            gst::ElementFactory::make("vapostproc")
                .build()
                .context("creating letterbox colour converter")?
        } else {
            gst::ElementFactory::make("videoconvert")
                .build()
                .context("creating letterbox colour converter")?
        };
        tracing::info!(
            "ExpandCast: content {render_width}x{render_height} letterboxed into {}x{}",
            out_width,
            out_height
        );
        Some((boxx, converter))
    } else {
        None
    };

    let mut elements: Vec<&gst::Element> = vec![&src];
    if let Some(filter) = &pinned_caps {
        elements.push(filter);
    }
    elements.push(&capture_queue);
    elements.extend(scalers.iter());
    elements.push(&rate);
    if let Some((boxx, converter)) = &padding {
        elements.push(boxx);
        elements.push(converter);
    }
    elements.extend([&enc, &parse, &pay, &webrtc]);
    pipeline
        .add_many(elements.iter().copied())
        .context("adding elements to pipeline")?;

    let framerate = gst::Fraction::new(config.fps as i32, 1);
    // Everything upstream of the encoder: aspect-preserving box, exact panel size.
    let boxed_caps = gst::Caps::builder("video/x-raw")
        .field("format", "I420")
        .field("width", render_width as i32)
        .field("height", render_height as i32)
        .field("framerate", framerate)
        .build();
    let captured_caps = gst::Caps::builder("video/x-raw")
        .field("format", "I420")
        .field("width", out_width as i32)
        .field("height", out_height as i32)
        .field("framerate", framerate)
        .build();
    let encoder_caps = gst::Caps::builder("video/x-raw")
        .field("format", "NV12")
        .field("width", out_width as i32)
        .field("height", out_height as i32)
        .field("framerate", framerate)
        .build();

    let mut tail: &gst::Element = &src;
    if let Some(filter) = &pinned_caps {
        tail.link(filter).context("linking source to capture format filter")?;
        tail = filter;
    }
    tail.link(&capture_queue).context("linking capture queue")?;
    tail = &capture_queue;
    for element in &scalers {
        tail.link(element)
            .with_context(|| format!("linking {} to {}", tail.name(), element.name()))?;
        tail = element;
    }
    tail.link(&rate).context("linking scaler to rate")?;
    match &padding {
        Some((boxx, converter)) => {
            rate.link_filtered(boxx, &boxed_caps)
                .context("linking rate to videobox")?;
            boxx.link_filtered(converter, &captured_caps)
                .context("linking videobox to colour converter")?;
            converter
                .link_filtered(&enc, &encoder_caps)
                .context("linking colour converter to encoder")?;
        }
        None => {
            rate.link_filtered(&enc, &encoder_caps)
                .context("linking rate to encoder")?;
        }
    }
    enc.link(&parse).context("linking encoder to parser")?;
    parse.link(&pay).context("linking parser to payloader")?;

    // Connect payload directly to webrtcbin sink pad with explicit H.264 RTP caps
    let rtp_caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "H264")
        .field("clock-rate", 90000i32)
        .field("payload", 100i32)
        .field("packetization-mode", "1")
        .field("profile-level-id", "42e01f")
        .build();
    let pay_src = pay.static_pad("src").context("getting payloader src pad")?;
    let webrtc_sink = webrtc.request_pad_simple("sink_%u").context("requesting webrtc sink pad")?;
    // Second stage of the same fix: webrtcbin still pushes back through the
    // payloader (measured 4 fps captured instead of 26), so stale RTP buffers
    // are dropped here rather than throttling the capture chain.
    let send_queue = gst::ElementFactory::make("queue")
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 3u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .build()
        .context("creating send queue")?;
    pipeline.add(&send_queue).context("adding send queue")?;
    pay_src
        .link(&send_queue.static_pad("sink").context("send queue sink pad")?)
        .context("linking payloader to send queue")?;
    send_queue
        .static_pad("src")
        .context("send queue src pad")?
        .link(&webrtc_sink)
        .context("linking send queue to webrtcbin")?;
    let _ = webrtc.emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
        "add-transceiver",
        &[&gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv, &Some(&rtp_caps)],
    );

    // Listen for reverse touch input data channel from tablet
    webrtc.connect("on-data-channel", false, move |values| {
        if let Ok(channel) = values[1].get::<gst_webrtc::WebRTCDataChannel>() {
            tracing::info!("Tablet opened WebRTC DataChannel: {:?}", channel.label());
        }
        None
    });

    // Monitor WebRTC connection states
    let webrtc_ice = webrtc.clone();
    webrtc.connect_notify(Some("ice-connection-state"), move |_, _| {
        let state = webrtc_ice.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
        tracing::info!("WebRTC ICE connection state: {:?}", state);
    });
    let webrtc_conn = webrtc.clone();
    webrtc.connect_notify(Some("connection-state"), move |_, _| {
        let state = webrtc_conn.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
        tracing::info!("WebRTC peer connection state: {:?}", state);
    });

    // Handle ICE candidates generated by local webrtcbin
    let outgoing_ice = outgoing_tx.clone();
    webrtc.connect("on-ice-candidate", false, move |values| {
        let mline_index = values[1].get::<u32>().unwrap_or_default();
        let candidate = values[2].get::<String>().unwrap_or_default();
        let wire_mline = if mline_index == 0 { 0 } else { 1 };
        tracing::info!("Local WebRTC candidate: mline={}, cand={}", wire_mline, candidate);
        let _ = outgoing_ice.try_send(SignalingMessage::Ice {
            candidate,
            sdp_mline_index: wire_mline,
            mid: Some(format!("{wire_mline}")),
        });
        None
    });

    // The offer is published only after the video caps are negotiated: an offer
    // built while the payloader has no caps contains just the data channel, and
    // the peer then waits forever for a first frame.
    let _ = pipeline.set_state(gst::State::Ready);
    let channel = webrtc.emit_by_name::<Option<gst_webrtc::WebRTCDataChannel>>(
        "create-data-channel",
        &[&"OdmfDataChannel", &None::<gst::Structure>],
    );
    tracing::info!("Created OdmfDataChannel for ExpandCast: {:?}", channel.is_some());

    // Incoming signaling loop
    let bus = pipeline.bus().context("getting pipeline bus")?;
    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(error).context("starting the desktop capture pipeline");
    }

    // A compositor that refuses the negotiated capture format leaves the pipeline
    // stuck in PAUSED, which reads as a peer that never gets a first frame.
    let deadline = Instant::now() + Duration::from_secs(4);
    while pipeline.current_state() != gst::State::Playing {
        if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(50)) {
            if let gst::MessageView::Error(err) = message.view() {
                let _ = pipeline.set_state(gst::State::Null);
                anyhow::bail!(
                    "capture pipeline error: {} ({})",
                    err.error(),
                    err.debug().unwrap_or_default()
                );
            }
        }
        if Instant::now() > deadline {
            let _ = pipeline.set_state(gst::State::Null);
            anyhow::bail!("capture pipeline did not start; the compositor rejected the format");
        }
    }

    // Wait for the video caps so the offer carries the m=video section.
    let caps_deadline = Instant::now() + Duration::from_secs(3);
    while webrtc_sink.current_caps().is_none() {
        if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(50)) {
            if let gst::MessageView::Error(err) = message.view() {
                let _ = pipeline.set_state(gst::State::Null);
                anyhow::bail!(
                    "video negotiation error: {} ({})",
                    err.error(),
                    err.debug().unwrap_or_default()
                );
            }
        }
        if Instant::now() > caps_deadline {
            tracing::warn!("no video caps after 3s; publishing the offer anyway");
            break;
        }
    }
    publish_offer(&webrtc, &outgoing_tx);

    let frames_out = Arc::new(AtomicU64::new(0));
    // Count coded frames on the encoder: the payloader emits one buffer per RTP
    // packet, which would overstate the frame rate.
    if let Some(pad) = enc.static_pad("src") {
        let counter = frames_out.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    // Capture rate as seen by videorate: distinguishes a slow compositor from
    // rate conversion that fails to fill frames up to the target cadence.
    let source_frames = Arc::new(AtomicU64::new(0));
    if let Some(pad) = rate.static_pad("sink") {
        let counter = source_frames.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    // Count frames straight off the capture element as well: if the compositor
    // only delivers a handful per second, rate conversion has nothing to work
    // with and the stream is mostly stale duplicates.
    let raw_frames = Arc::new(AtomicU64::new(0));
    if let Some(pad) = src.static_pad("src") {
        let counter = raw_frames.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    let mut report_at = Instant::now() + REPORT_INTERVAL;
    let mut reported_frames = 0u64;
    let mut reported_source_frames = 0u64;
    let mut reported_raw_frames = 0u64;
    let mut logged_capture_caps = false;
    while !stop.load(Ordering::Acquire) {
        // Poll for incoming signaling messages from daemon
        while let Ok(msg) = incoming_rx.try_recv() {
            match msg {
                SignalingMessage::Sdp { type_, sdp } if type_ == "answer" => {
                    tracing::info!("Received ExpandCast answer SDP from tablet ({} bytes)", sdp.len());
                    let local_sdp = parse_wire_answer(&sdp);
                    let sdp_msg = gst_sdp::SDPMessage::parse_buffer(local_sdp.as_bytes())
                        .map_err(|_| anyhow!("failed to parse answer SDP"))?;
                    let answer = gst_webrtc::WebRTCSessionDescription::new(
                        gst_webrtc::WebRTCSDPType::Answer,
                        sdp_msg,
                    );
                    let promise = gst::Promise::with_change_func(move |reply| {
                        match reply {
                            Ok(Some(s)) => tracing::info!("set-remote-description succeeded: {s}"),
                            Ok(None) => tracing::info!("set-remote-description finished with empty response"),
                            Err(e) => tracing::error!("set-remote-description failed: {e:?}"),
                        }
                    });
                    webrtc.emit_by_name::<()>("set-remote-description", &[&answer, &promise]);
                }
                SignalingMessage::Ice { candidate, sdp_mline_index, .. } => {
                    tracing::info!("Adding remote ICE candidate: mline={}, cand={}", sdp_mline_index, candidate);
                    webrtc.emit_by_name::<()>("add-ice-candidate", &[&sdp_mline_index, &candidate]);
                }
                _ => {}
            }
        }

        // Check pipeline bus messages
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(50)) {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    return Err(anyhow!(
                        "GStreamer error: {} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    ));
                }
                gst::MessageView::Eos(_) => {
                    tracing::info!("GStreamer pipeline reached EOS");
                    break;
                }
                _ => {}
            }
        }

        if Instant::now() >= report_at {
            if !logged_capture_caps {
                logged_capture_caps = true;
                if let Some(caps) = src.static_pad("src").and_then(|pad| pad.current_caps()) {
                    tracing::info!("ExpandCast capture caps: {caps}");
                }
            }
            let total = frames_out.load(Ordering::Relaxed);
            let source_total = source_frames.load(Ordering::Relaxed);
            let raw_total = raw_frames.load(Ordering::Relaxed);
            let elapsed = REPORT_INTERVAL.as_secs_f64();
            let fps = (total - reported_frames) as f64 / elapsed;
            let source_fps = (source_total - reported_source_frames) as f64 / elapsed;
            let raw_fps = (raw_total - reported_raw_frames) as f64 / elapsed;
            tracing::info!(
                "ExpandCast stream: compositor {raw_fps:.1} fps -> scaled {source_fps:.1} fps -> encoded {fps:.1} fps ({}x{}, {} kbps)",
                out_width,
                out_height,
                config.bitrate_kbps
            );
            reported_frames = total;
            reported_source_frames = source_total;
            reported_raw_frames = raw_total;
            report_at = Instant::now() + REPORT_INTERVAL;
        }
    }

    let _ = pipeline.set_state(gst::State::Null);
    Ok(())
}
/// Give each pipeline attempt its own descriptor for the portal's PipeWire node.
/// The returned descriptor is intentionally leaked: pipewire takes ownership.
fn duplicate_fd(fd: i32) -> Result<i32> {
    use std::os::fd::{BorrowedFd, IntoRawFd};
    // Safety: the caller keeps the portal guard (and thus `fd`) alive; we only
    // borrow it long enough to duplicate.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let owned = borrowed
        .try_clone_to_owned()
        .context("duplicating the PipeWire remote descriptor")?;
    Ok(owned.into_raw_fd())
}

/// Ask webrtcbin for an offer and forward the wire-normalised SDP.
fn publish_offer(webrtc: &gst::Element, outgoing: &mpsc::Sender<SignalingMessage>) {
    let webrtc_local = webrtc.clone();
    let outgoing = outgoing.clone();
    let promise = gst::Promise::with_change_func(move |reply| {
        let Ok(Some(structure)) = reply else {
            tracing::warn!("ExpandCast create-offer produced no description");
            return;
        };
        let Ok(offer) = structure.get::<gst_webrtc::WebRTCSessionDescription>("offer") else {
            tracing::warn!("ExpandCast create-offer reply has no offer");
            return;
        };
        let sdp = offer.sdp().as_text().unwrap_or_default();
        tracing::info!("Generated ExpandCast WebRTC offer ({} bytes)", sdp.len());
        webrtc_local.emit_by_name::<()>(
            "set-local-description",
            &[&offer, &gst::Promise::new()],
        );
        let _ = outgoing.try_send(SignalingMessage::Sdp {
            type_: "offer".into(),
            sdp: format_wire_offer(&sdp),
        });
    });
    webrtc.emit_by_name::<()>("create-offer", &[&None::<&gst::Structure>, &promise]);
}

fn format_wire_offer(sdp: &str) -> String {
    let mut lines = Vec::new();
    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');
        if line == "a=rtcp-mux-only" || line == "a=bundle-only" {
            continue;
        }
        if line.starts_with("a=group:BUNDLE") {
            lines.push("a=group:BUNDLE 0 1".to_string());
        } else if line == "a=mid:video0" {
            lines.push("a=mid:0".to_string());
        } else if line.starts_with("a=mid:application") || line.starts_with("a=mid:data") {
            lines.push("a=mid:1".to_string());
        } else if line.starts_with("m=application 0") {
            lines.push("m=application 9 UDP/DTLS/SCTP webrtc-datachannel".to_string());
        } else if line.starts_with("a=fmtp:100 ") {
            lines.push("a=fmtp:100 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_string());
        } else {
            lines.push(line.to_string());
        }
    }
    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    text
}

fn parse_wire_answer(sdp: &str) -> String {
    let mut lines = Vec::new();
    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("a=group:BUNDLE") {
            lines.push("a=group:BUNDLE video0 application1".to_string());
        } else if line == "a=mid:0" {
            lines.push("a=mid:video0".to_string());
        } else if line == "a=mid:1" {
            lines.push("a=mid:application1".to_string());
        } else {
            lines.push(line.to_string());
        }
    }
    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    text
}

//! GStreamer WebRTC desktop streaming sender for tablet secondary display.
use crate::persisted::{ExpandCastSettings, BITRATE_MAX, BITRATE_MIN};
use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_app::prelude::*;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

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
    pub adaptive_bitrate: bool,
    /// Size to request from a compositor-created virtual output, so the desktop
    /// itself has the tablet's aspect ratio instead of being letterboxed.
    pub capture_width: Option<u32>,
    pub capture_height: Option<u32>,
    pub pipewire_fd: Option<Arc<OwnedFd>>,
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
            adaptive_bitrate: true,
            capture_width: None,
            capture_height: None,
            pipewire_fd: None,
            pipewire_node: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BitrateMode {
    initial: u32,
    adaptive: bool,
}

fn bitrate_mode(config: &SenderConfig, override_value: Option<&str>) -> Result<BitrateMode> {
    let fixed = override_value
        .map(|value| {
            value
                .parse::<u32>()
                .context("invalid OPLUS_EXPAND_CAST_BITRATE")
        })
        .transpose()?;
    let initial = fixed.unwrap_or(config.bitrate_kbps);
    if !(BITRATE_MIN..=BITRATE_MAX).contains(&initial) {
        return Err(anyhow!(
            "ExpandCast bitrate must be between {BITRATE_MIN} and {BITRATE_MAX} kbps"
        ));
    }
    Ok(BitrateMode {
        initial,
        adaptive: fixed.is_none() && config.adaptive_bitrate,
    })
}

fn adjusted_bitrate(current: u32, loss: f64, initial: u32) -> u32 {
    let next = if loss > 0.03 {
        (f64::from(current) * 0.8) as u32
    } else if loss < 0.01 {
        (f64::from(current) * 1.1) as u32
    } else {
        current
    };
    next.clamp(1_200.min(initial), 6_500.max(initial))
}

pub struct MediaSender {
    pub incoming: mpsc::Sender<SignalingMessage>,
    pub outgoing: mpsc::Receiver<SignalingMessage>,
    pub finished: tokio::sync::oneshot::Receiver<Result<(), String>>,
    pub stop: Arc<AtomicBool>,
}

pub fn spawn_sender(config: SenderConfig) -> Result<MediaSender> {
    let settings = ExpandCastSettings {
        fps: config.fps,
        bitrate_kbps: config.bitrate_kbps,
        adaptive_bitrate: config.adaptive_bitrate,
    };
    if !settings.is_valid() {
        return Err(anyhow!("invalid ExpandCast sender settings"));
    }
    let override_value = std::env::var("OPLUS_EXPAND_CAST_BITRATE").ok();
    bitrate_mode(&config, override_value.as_deref())?;
    gst::init().context("initializing GStreamer for ExpandCast")?;

    let (incoming_tx, incoming_rx) = mpsc::channel(64);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(64);
    let (finished_tx, finished) = tokio::sync::oneshot::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();

    std::thread::Builder::new()
        .name("oplus-expand-cast-sender".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                let mut incoming_rx = incoming_rx;
                let mut last_error = None;
                // Older compositors may reject the requested virtual output size.
                let pin_supported =
                    config.capture_width.is_some() && config.capture_height.is_some();
                let pin_requested = pin_supported
                    && std::env::var("OPLUS_EXPAND_CAST_PIN_CAPTURE").as_deref() != Ok("0");
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
                        Ok(Ok(())) => return Ok(()),
                        Ok(Err(error)) => last_error = Some(error),
                        Err(_) => anyhow::bail!("ExpandCast sender thread panicked"),
                    }
                    if stop_thread.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if pin_capture {
                        tracing::warn!(
                            "compositor does not resize virtual outputs to the requested stream size ({:#}); \
                             falling back to the compositor size and letterboxing",
                            last_error.as_ref().expect("error recorded")
                        );
                    }
                }
                Err(last_error.unwrap_or_else(|| anyhow!("ExpandCast sender stopped")))
            })();
            let _ = finished_tx.send(result.map_err(|error| format!("{error:#}")));
        })
        .context("spawning ExpandCast sender thread")?;

    Ok(MediaSender {
        incoming: incoming_tx,
        outgoing: outgoing_rx,
        finished,
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
    let mut capture: Option<PortalCapture> = None;
    let (src, is_pipewire) = if let (Some(fd), Some(node)) =
        (config.pipewire_fd.as_ref(), config.pipewire_node)
    {
        // Copy frames before encoding so PipeWire buffers return to the compositor promptly.
        let pin = if pin_capture {
            config.capture_width.zip(config.capture_height)
        } else {
            None
        };
        let mut control = start_portal_capture(Arc::clone(fd), node, pin)?;
        let appsrc = create_capture_source()?;
        let pump_src = appsrc
            .clone()
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| anyhow!("appsrc is not an AppSrc"))?;
        let rx = control.rx.take().context("capture channel")?;
        std::thread::Builder::new()
            .name("expand-cast-pump".into())
            .spawn(move || pump_capture(pump_src, rx))
            .context("spawning capture pump thread")?;
        capture = Some(control);
        (appsrc, true)
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

    let pinned_size = if pin_capture && is_pipewire {
        config
            .capture_width
            .zip(config.capture_height)
            .map(|(w, h)| ((w & !1).max(2), (h & !1).max(2)))
    } else {
        None
    };

    // Drop stale frames rather than accumulating capture latency.
    let capture_queue = gst::ElementFactory::make("queue")
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 1u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .build()
        .context("creating capture queue")?;

    // Prefer GPU scaling and colour conversion when available.
    let scalers: Vec<gst::Element> = if gst::ElementFactory::find("vapostproc").is_some() {
        tracing::info!("ExpandCast: scaling with VA-API post-processing");
        vec![gst::ElementFactory::make("vapostproc")
            .build()
            .context("creating vapostproc")?]
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
    let rate = gst::ElementFactory::make("videorate")
        .build()
        .context("creating videorate")?;
    let override_value = std::env::var("OPLUS_EXPAND_CAST_BITRATE").ok();
    let mode = bitrate_mode(config, override_value.as_deref())?;
    let bitrate_kbps = mode.initial;
    if !mode.adaptive {
        tracing::info!("ExpandCast: fixed bitrate {bitrate_kbps} kbps");
    }
    let target_bitrate = Arc::new(AtomicU32::new(bitrate_kbps));
    // Choose VA-API H.264 hardware encoder if available, otherwise x264enc
    let (enc, _is_va) = if gst::ElementFactory::find("vah264enc").is_some() {
        let enc = gst::ElementFactory::make("vah264enc")
            .property("bitrate", bitrate_kbps)
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
            .property("bitrate", bitrate_kbps)
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
    // matches the panel, so there is nothing to letterbox (see `pinned_size`).
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

    // Drop stale frames when downstream processing falls behind.
    let make_leaky_queue = |name: &str| -> Result<gst::Element> {
        gst::ElementFactory::make("queue")
            .name(name)
            .property_from_str("leaky", "downstream")
            .property("max-size-buffers", 3u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .build()
            .with_context(|| format!("creating {name}"))
    };
    let post_scaler_queue = make_leaky_queue("post-scaler-queue")?;
    let post_rate_queue = make_leaky_queue("post-rate-queue")?;

    let mut elements: Vec<&gst::Element> = vec![&src];
    elements.push(&capture_queue);
    elements.extend(scalers.iter());
    elements.push(&post_scaler_queue);
    elements.push(&rate);
    elements.push(&post_rate_queue);
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

    src.link(&capture_queue).context("linking capture queue")?;
    let mut tail: &gst::Element = &capture_queue;
    for element in &scalers {
        tail.link(element)
            .with_context(|| format!("linking {} to {}", tail.name(), element.name()))?;
        tail = element;
    }
    tail.link(&post_scaler_queue)
        .context("linking scaler to post-scaler queue")?;
    tail = &post_scaler_queue;
    tail.link(&rate)
        .context("linking post-scaler queue to rate")?;
    tail = &rate;
    tail.link(&post_rate_queue)
        .context("linking rate to post-rate queue")?;
    match &padding {
        Some((boxx, converter)) => {
            post_rate_queue
                .link_filtered(boxx, &boxed_caps)
                .context("linking post-rate queue to videobox")?;
            boxx.link_filtered(converter, &captured_caps)
                .context("linking videobox to colour converter")?;
            converter
                .link_filtered(&enc, &encoder_caps)
                .context("linking colour converter to encoder")?;
        }
        None => {
            post_rate_queue
                .link_filtered(&enc, &encoder_caps)
                .context("linking post-rate queue to encoder")?;
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
    let webrtc_sink = webrtc
        .request_pad_simple("sink_%u")
        .context("requesting webrtc sink pad")?;
    // RTP fragments must not be dropped independently; shed raw frames upstream.
    let send_queue = gst::ElementFactory::make("queue")
        .property("max-size-buffers", 256u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .build()
        .context("creating send queue")?;
    pipeline.add(&send_queue).context("adding send queue")?;
    pay_src
        .link(
            &send_queue
                .static_pad("sink")
                .context("send queue sink pad")?,
        )
        .context("linking payloader to send queue")?;
    // Diagnostic mode discards RTP locally to isolate the network leg.
    let fake_sink = match std::env::var("OPLUS_EXPAND_CAST_FAKE_SINK").as_deref() {
        Ok("1") => {
            let sink = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .build()
                .context("creating diagnostic fakesink")?;
            pipeline.add(&sink).context("adding diagnostic fakesink")?;
            tracing::warn!(
                "OPLUS_EXPAND_CAST_FAKE_SINK=1: discarding RTP locally; the tablet gets no video"
            );
            Some(sink)
        }
        _ => None,
    };
    let send_queue_src = send_queue.static_pad("src").context("send queue src pad")?;
    match &fake_sink {
        Some(sink) => send_queue_src
            .link(&sink.static_pad("sink").context("fakesink sink pad")?)
            .context("linking send queue to diagnostic fakesink")?,
        None => send_queue_src
            .link(&webrtc_sink)
            .context("linking send queue to webrtcbin")?,
    };
    let _ = webrtc.emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
        "add-transceiver",
        &[
            &gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv,
            &Some(&rtp_caps),
        ],
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
        let state =
            webrtc_ice.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
        tracing::info!("WebRTC ICE connection state: {:?}", state);
    });
    let webrtc_conn = webrtc.clone();
    webrtc.connect_notify(Some("connection-state"), move |_, _| {
        let state =
            webrtc_conn.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
        tracing::info!("WebRTC peer connection state: {:?}", state);
    });

    // Handle ICE candidates generated by local webrtcbin
    let outgoing_ice = outgoing_tx.clone();
    webrtc.connect("on-ice-candidate", false, move |values| {
        let mline_index = values[1].get::<u32>().unwrap_or_default();
        let candidate = values[2].get::<String>().unwrap_or_default();
        let wire_mline = if mline_index == 0 { 0 } else { 1 };
        tracing::info!(
            "Local WebRTC candidate: mline={}, cand={}",
            wire_mline,
            candidate
        );
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
    tracing::info!(
        "Created OdmfDataChannel for ExpandCast: {:?}",
        channel.is_some()
    );

    // Allow latency negotiation to be overridden for diagnostics.
    match std::env::var("OPLUS_EXPAND_CAST_PIPELINE_LATENCY").as_deref() {
        Ok("auto") => {}
        Ok(value) => match value.parse::<u64>() {
            Ok(ms) => pipeline.set_latency(gst::ClockTime::from_mseconds(ms)),
            Err(_) => {
                tracing::warn!("ignoring invalid OPLUS_EXPAND_CAST_PIPELINE_LATENCY={value}")
            }
        },
        Err(_) => pipeline.set_latency(gst::ClockTime::ZERO),
    }
    tracing::info!(
        "ExpandCast pipeline latency: requested={:?} configured={:?}",
        pipeline.latency(),
        pipeline.configured_latency()
    );

    let bus = pipeline.bus().context("getting pipeline bus")?;
    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(error).context("starting the desktop capture pipeline");
    }

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
    let video_pad = if fake_sink.is_some() {
        &pay_src
    } else {
        &webrtc_sink
    };
    match wait_for_video_caps(
        video_pad,
        &bus,
        capture.as_ref(),
        stop,
        Duration::from_secs(3),
    ) {
        Ok(true) => {}
        Ok(false) => {
            let _ = pipeline.set_state(gst::State::Null);
            return Ok(());
        }
        Err(error) => {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(error);
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
    // Stage counters: where the rate drops tells us whether the compositor, the
    // capture queue or the WebRTC leg is the limiter.
    let count_pad = |element: &gst::Element, pad_name: &str| -> Arc<AtomicU64> {
        let counter = Arc::new(AtomicU64::new(0));
        if let Some(pad) = element.static_pad(pad_name) {
            let counter_inner = counter.clone();
            pad.add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                move |_, info| {
                    let count = info.buffer_list().map_or(1, |list| list.len() as u64);
                    counter_inner.fetch_add(count, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                },
            );
        }
        counter
    };
    let queue_frames = count_pad(&capture_queue, "src");
    let pay_frames = count_pad(&pay, "src");
    let send_frames = count_pad(&send_queue, "src");

    let mut report_at = Instant::now() + REPORT_INTERVAL;
    let mut reported_frames = 0u64;
    let mut reported_source_frames = 0u64;
    let mut reported_raw_frames = 0u64;
    let mut reported_queue_frames = 0u64;
    let mut reported_pay_frames = 0u64;
    let mut reported_send_frames = 0u64;
    let mut logged_capture_caps = false;
    while !stop.load(Ordering::Acquire) {
        if let Some(error) = capture.as_ref().and_then(PortalCapture::error) {
            let _ = pipeline.set_state(gst::State::Null);
            anyhow::bail!("capture pipeline failed: {error}");
        }
        // Poll for incoming signaling messages from daemon
        while let Ok(msg) = incoming_rx.try_recv() {
            match msg {
                SignalingMessage::Sdp { type_, sdp } if type_ == "answer" => {
                    tracing::info!(
                        "Received ExpandCast answer SDP from tablet ({} bytes)",
                        sdp.len()
                    );
                    let local_sdp = parse_wire_answer(&sdp);
                    let sdp_msg = gst_sdp::SDPMessage::parse_buffer(local_sdp.as_bytes())
                        .map_err(|_| anyhow!("failed to parse answer SDP"))?;
                    let answer = gst_webrtc::WebRTCSessionDescription::new(
                        gst_webrtc::WebRTCSDPType::Answer,
                        sdp_msg,
                    );
                    let promise = gst::Promise::with_change_func(move |reply| match reply {
                        Ok(Some(s)) => tracing::info!("set-remote-description succeeded: {s}"),
                        Ok(None) => {
                            tracing::info!("set-remote-description finished with empty response")
                        }
                        Err(e) => tracing::error!("set-remote-description failed: {e:?}"),
                    });
                    webrtc.emit_by_name::<()>("set-remote-description", &[&answer, &promise]);
                }
                SignalingMessage::Ice {
                    candidate,
                    sdp_mline_index,
                    ..
                } => {
                    tracing::info!(
                        "Adding remote ICE candidate: mline={}, cand={}",
                        sdp_mline_index,
                        candidate
                    );
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
                    if let Some(error) = capture.as_ref().and_then(PortalCapture::error) {
                        let _ = pipeline.set_state(gst::State::Null);
                        anyhow::bail!("capture pipeline failed: {error}");
                    }
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
            let queue_total = queue_frames.load(Ordering::Relaxed);
            let pay_total = pay_frames.load(Ordering::Relaxed);
            let send_total = send_frames.load(Ordering::Relaxed);
            let elapsed = REPORT_INTERVAL.as_secs_f64();
            let fps = (total - reported_frames) as f64 / elapsed;
            let source_fps = (source_total - reported_source_frames) as f64 / elapsed;
            let raw_fps = (raw_total - reported_raw_frames) as f64 / elapsed;
            let queue_fps = (queue_total - reported_queue_frames) as f64 / elapsed;
            let pay_fps = (pay_total - reported_pay_frames) as f64 / elapsed;
            let send_fps = (send_total - reported_send_frames) as f64 / elapsed;
            tracing::info!(
                "ExpandCast stream: input {raw_fps:.1} -> queue {queue_fps:.1} -> scaled {source_fps:.1} -> encoded {fps:.1} fps; RTP pay {pay_fps:.1} -> send {send_fps:.1} buffers/s ({}x{}, {} kbps)",
                out_width,
                out_height,
                target_bitrate.load(Ordering::Relaxed)
            );
            // WebRTC stats: packet/NACK/PLI counters and the remote estimate
            // distinguish a congested link from a local pipeline problem.
            let stats_webrtc = webrtc.clone();
            let target = target_bitrate.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                let Ok(Some(stats)) = reply else {
                    return;
                };
                if std::env::var("OPLUS_EXPAND_CAST_STATS").is_ok() {
                    tracing::info!("ExpandCast webrtc stats: {stats}");
                }
                if !mode.adaptive {
                    return;
                }
                let Some(loss) = parse_fraction_lost(stats) else {
                    return;
                };
                let current = target.load(Ordering::Relaxed);
                let next = adjusted_bitrate(current, loss, mode.initial);
                if next != current {
                    tracing::info!(
                        "ExpandCast: peer loss {:.1}% -> bitrate {current} -> {next} kbps",
                        loss * 100.0
                    );
                    target.store(next, Ordering::Relaxed);
                }
            });
            stats_webrtc.emit_by_name::<()>("get-stats", &[&None::<&gst::Pad>, &promise]);
            let wanted_bitrate = target_bitrate.load(Ordering::Relaxed);
            enc.set_property("bitrate", wanted_bitrate);
            reported_frames = total;
            reported_source_frames = source_total;
            reported_raw_frames = raw_total;
            reported_queue_frames = queue_total;
            reported_pay_frames = pay_total;
            reported_send_frames = send_total;
            report_at = Instant::now() + REPORT_INTERVAL;
        }
    }

    let _ = pipeline.set_state(gst::State::Null);
    Ok(())
}

fn wait_for_video_caps(
    pad: &gst::Pad,
    bus: &gst::Bus,
    capture: Option<&PortalCapture>,
    stop: &AtomicBool,
    timeout: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(false);
        }
        if pad.current_caps().is_some() {
            return Ok(true);
        }
        if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(50)) {
            if let gst::MessageView::Error(err) = message.view() {
                anyhow::bail!(
                    "video negotiation error: {} ({})",
                    err.error(),
                    err.debug().unwrap_or_default()
                );
            }
        }
        if let Some(error) = capture.and_then(PortalCapture::error) {
            anyhow::bail!("capture pipeline failed: {error}");
        }
        if Instant::now() > deadline {
            anyhow::bail!("video caps not negotiated before timeout");
        }
    }
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
        webrtc_local.emit_by_name::<()>("set-local-description", &[&offer, &gst::Promise::new()]);
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
            lines.push(
                "a=fmtp:100 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                    .to_string(),
            );
        } else {
            lines.push(line.to_string());
        }
    }
    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    text
}

fn parse_fraction_lost(stats: &gst::StructureRef) -> Option<f64> {
    stats.iter().find_map(|(_, value)| {
        let report = value.get::<&gst::StructureRef>().ok()?;
        if report.get::<gst_webrtc::WebRTCStatsType>("type").ok()?
            != gst_webrtc::WebRTCStatsType::RemoteInboundRtp
        {
            return None;
        }
        if report
            .get::<&str>("kind")
            .ok()
            .is_some_and(|kind| kind != "video")
        {
            return None;
        }
        report
            .get::<f64>("fraction-lost")
            .ok()
            .filter(|value| (0.0..=1.0).contains(value))
    })
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

fn create_capture_source() -> Result<gst::Element> {
    gst::ElementFactory::make("appsrc")
        .property("is-live", true)
        .property_from_str("format", "time")
        .property("max-bytes", 0u64)
        .property("max-buffers", 2u64)
        .property_from_str("leaky-type", "downstream")
        .property("block", false)
        .property("do-timestamp", true)
        .build()
        .context("creating appsrc element")
}

fn pump_capture(src: gst_app::AppSrc, rx: std::sync::mpsc::Receiver<CaptureItem>) {
    use std::sync::mpsc::RecvTimeoutError;

    let mut current_caps = None;
    let mut last_frame: Option<gst::Buffer> = None;
    loop {
        let buffer = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(item) => {
                if current_caps.as_ref() != Some(&item.caps) {
                    src.set_caps(Some(&item.caps));
                    current_caps = Some(item.caps);
                }
                last_frame = Some(item.buffer.clone());
                item.buffer
            }
            Err(RecvTimeoutError::Timeout) => match &last_frame {
                Some(buffer) => buffer.clone(),
                None => continue,
            },
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if src.push_buffer(buffer).is_err() {
            break;
        }
    }
    let _ = src.end_of_stream();
}

struct CaptureItem {
    caps: gst::Caps,
    buffer: gst::Buffer,
}

/// Stops the capture pipeline and joins its thread on drop.
struct PortalCapture {
    rx: Option<std::sync::mpsc::Receiver<CaptureItem>>,
    stop: Arc<AtomicBool>,
    error: Arc<std::sync::Mutex<Option<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PortalCapture {
    fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn start_portal_capture(
    fd: Arc<OwnedFd>,
    node: u32,
    pin: Option<(u32, u32)>,
) -> Result<PortalCapture> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<CaptureItem>(2);
    let stop = Arc::new(AtomicBool::new(false));
    let error = Arc::new(std::sync::Mutex::new(None));
    let thread_stop = stop.clone();
    let thread_error = error.clone();
    let thread = std::thread::Builder::new()
        .name("expand-cast-capture".into())
        .spawn(move || {
            // Keep the channel open until the error is visible to the sender.
            let keep_open = tx.clone();
            if let Err(err) = run_capture_pipeline(fd, node, pin, tx, &thread_stop) {
                *thread_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("{err:#}"));
            }
            drop(keep_open);
        })
        .context("spawning capture thread")?;
    Ok(PortalCapture {
        rx: Some(rx),
        stop,
        error,
        thread: Some(thread),
    })
}

fn run_capture_pipeline(
    fd: Arc<OwnedFd>,
    node: u32,
    pin: Option<(u32, u32)>,
    tx: std::sync::mpsc::SyncSender<CaptureItem>,
    stop: &AtomicBool,
) -> Result<()> {
    gst::init().ok();
    // pipewiresrc borrows this descriptor and duplicates it for its PipeWire core.
    let capture_fd = fd.try_clone().context("duplicating PipeWire remote fd")?;
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("pipewiresrc")
        .property("fd", capture_fd.as_raw_fd())
        .property("path", node.to_string())
        .property("do-timestamp", true)
        .property("keepalive-time", 0i32)
        .build()
        .context("creating capture pipewiresrc")?;
    let sink_element = gst::ElementFactory::make("appsink")
        .property("max-buffers", 1u32)
        .property("drop", true)
        .property("sync", false)
        .property("enable-last-sample", false)
        .build()
        .context("creating capture appsink")?;
    let sink = sink_element
        .clone()
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("appsink is not an AppSink"))?;
    pipeline.add(&src).context("adding capture source")?;
    pipeline
        .add(&sink_element)
        .context("adding capture appsink")?;
    // WirePlumber needs a media type to link the capture stream to a video node.
    let mut caps = gst::Caps::builder("video/x-raw");
    if let Some((width, height)) = pin {
        caps = caps
            .field("width", (width & !1).max(2) as i32)
            .field("height", (height & !1).max(2) as i32);
    }
    let caps = caps.build();
    tracing::info!("ExpandCast: requesting capture caps {caps}");
    let filter = gst::ElementFactory::make("capsfilter")
        .property("caps", &caps)
        .build()
        .context("creating capture capsfilter")?;
    pipeline.add(&filter).context("adding capture capsfilter")?;
    src.link(&filter).context("linking capture source")?;
    filter
        .link(&sink_element)
        .context("linking capture capsfilter to appsink")?;

    let tx_callback = tx.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                if let Ok(sample) = sink.pull_sample() {
                    if let (Some(caps), Some(buffer)) = (sample.caps(), sample.buffer()) {
                        if let Ok(copy) = buffer.copy_deep() {
                            let mut copy = copy;
                            // Timestamps belong to the capture pipeline's clock;
                            // let appsrc stamp them in the encode pipeline.
                            let copy_ref = copy.make_mut();
                            copy_ref.set_pts(gst::ClockTime::NONE);
                            copy_ref.set_dts(gst::ClockTime::NONE);
                            let _ = tx_callback.try_send(CaptureItem {
                                caps: caps.to_owned(),
                                buffer: copy,
                            });
                        }
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let bus = pipeline.bus().context("capture pipeline bus")?;
    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        let details = bus
            .pop_filtered(&[gst::MessageType::Error])
            .and_then(|message| {
                if let gst::MessageView::Error(err) = message.view() {
                    Some(format!(
                        "{} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    ))
                } else {
                    None
                }
            });
        let _ = pipeline.set_state(gst::State::Null);
        if let Some(details) = details {
            anyhow::bail!("starting capture pipeline: {details}");
        }
        return Err(error).context("starting capture pipeline");
    }
    while !stop.load(Ordering::Acquire) {
        if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
            match message.view() {
                gst::MessageView::Error(err) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    anyhow::bail!(
                        "capture pipeline error: {} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    );
                }
                gst::MessageView::Eos(_) => break,
                _ => {}
            }
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    Ok(())
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    #[test]
    fn bitrate_modes_respect_settings_and_override() {
        let mut config = SenderConfig::default();
        assert_eq!(
            bitrate_mode(&config, None).unwrap(),
            BitrateMode {
                initial: 6000,
                adaptive: true
            }
        );
        config.bitrate_kbps = 12_000;
        assert_eq!(
            bitrate_mode(&config, None).unwrap(),
            BitrateMode {
                initial: 12_000,
                adaptive: true
            }
        );
        assert_eq!(adjusted_bitrate(12_000, 0.02, 12_000), 12_000);
        assert_eq!(adjusted_bitrate(12_000, 0.1, 12_000), 9_600);
        assert_eq!(adjusted_bitrate(6000, 0.0, 6000), 6500);
        assert_eq!(adjusted_bitrate(1200, 0.1, 6000), 1200);
        assert_eq!(adjusted_bitrate(500, 0.02, 500), 500);
        config.adaptive_bitrate = false;
        assert_eq!(
            bitrate_mode(&config, None).unwrap(),
            BitrateMode {
                initial: 12_000,
                adaptive: false
            }
        );
        config.adaptive_bitrate = true;
        assert_eq!(
            bitrate_mode(&config, Some("7000")).unwrap(),
            BitrateMode {
                initial: 7000,
                adaptive: false
            }
        );
        assert!(bitrate_mode(&config, Some("50001")).is_err());
        assert!(bitrate_mode(&config, Some("invalid")).is_err());
    }

    #[test]
    fn missing_video_caps_fail_instead_of_proceeding() {
        gst::init().unwrap();
        let pad = gst::Pad::builder(gst::PadDirection::Sink).build();
        let bus = gst::Bus::new();
        let stop = AtomicBool::new(false);
        let error =
            wait_for_video_caps(&pad, &bus, None, &stop, Duration::from_millis(1)).unwrap_err();
        assert!(error.to_string().contains("video caps not negotiated"));
        stop.store(true, Ordering::Release);
        assert!(!wait_for_video_caps(&pad, &bus, None, &stop, Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn receiver_loss_uses_typed_video_report() {
        gst::init().unwrap();
        let audio = gst::Structure::builder("remote-inbound-rtp")
            .field("type", gst_webrtc::WebRTCStatsType::RemoteInboundRtp)
            .field("kind", "audio")
            .field("fraction-lost", 0.75f64)
            .build();
        let video = gst::Structure::builder("remote-inbound-rtp")
            .field("type", gst_webrtc::WebRTCStatsType::RemoteInboundRtp)
            .field("kind", "video")
            .field("fraction-lost", 0.125f64)
            .build();
        let stats = gst::Structure::builder("application/x-webrtc-stats")
            .field("audio", &audio)
            .field("video", &video)
            .build();
        assert_eq!(parse_fraction_lost(&stats), Some(0.125));
        let unrelated = gst::Structure::builder("application/x-webrtc-stats")
            .field("audio", &audio)
            .field("fraction-lost", 0.5f64)
            .build();
        assert_eq!(parse_fraction_lost(&unrelated), None);
    }

    #[test]
    fn capture_source_bounds_pending_frames() {
        gst::init().unwrap();
        let src = create_capture_source()
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        for _ in 0..100 {
            src.push_buffer(gst::Buffer::with_size(1024).unwrap())
                .unwrap();
        }
        assert_eq!(src.property::<u64>("current-level-buffers"), 2);
    }

    #[test]
    fn capture_pump_repeats_idle_frame_with_new_timestamp() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let src = create_capture_source().unwrap();
        let sink = gst::ElementFactory::make("appsink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add_many([&src, &sink]).unwrap();
        src.link(&sink).unwrap();
        let src = src.downcast::<gst_app::AppSrc>().unwrap();
        let sink = sink.downcast::<gst_app::AppSink>().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let thread = std::thread::spawn(move || pump_capture(src, rx));
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "GRAY8")
            .field("width", 2i32)
            .field("height", 2i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .build();
        tx.send(CaptureItem {
            caps,
            buffer: gst::Buffer::from_slice([1u8, 2, 3, 4]),
        })
        .unwrap();
        let first = sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .unwrap();
        let second = sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .unwrap();
        let first = first.buffer().unwrap();
        let second = second.buffer().unwrap();
        assert_eq!(
            first.map_readable().unwrap().as_slice(),
            second.map_readable().unwrap().as_slice()
        );
        assert!(second.pts().unwrap() > first.pts().unwrap());
        drop(tx);
        thread.join().unwrap();
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn capture_pump_forwards_changed_caps_with_frames() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let src = create_capture_source().unwrap();
        let sink = gst::ElementFactory::make("appsink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add_many([&src, &sink]).unwrap();
        src.link(&sink).unwrap();
        let src = src.downcast::<gst_app::AppSrc>().unwrap();
        let sink = sink.downcast::<gst_app::AppSink>().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let thread = std::thread::spawn(move || pump_capture(src, rx));
        for width in [2, 4] {
            let caps = gst::Caps::builder("video/x-raw")
                .field("format", "GRAY8")
                .field("width", width)
                .field("height", 2i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build();
            tx.send(CaptureItem {
                caps,
                buffer: gst::Buffer::with_size((width * 2) as usize).unwrap(),
            })
            .unwrap();
            let sample = sink
                .try_pull_sample(gst::ClockTime::from_seconds(3))
                .expect("capture frame");
            assert_eq!(
                sample
                    .caps()
                    .unwrap()
                    .structure(0)
                    .unwrap()
                    .get::<i32>("width")
                    .unwrap(),
                width
            );
        }
        drop(tx);
        thread.join().unwrap();
        pipeline.set_state(gst::State::Null).unwrap();
    }
}

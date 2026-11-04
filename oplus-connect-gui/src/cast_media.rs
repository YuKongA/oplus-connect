//! GUI-owned WebRTC video/audio receiver with bounded signaling.
//!
//! The daemon adapter owns signaling transport; no frames cross that boundary.
//! Video is decoded with VA-API (H.264 or H.265), scaled in GPU memory and then
//! copied once into BGRA buffers that the GPUI cast window paints; audio is
//! Opus decoded straight into the desktop audio sink.
use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer_sdp as gst_sdp;
use gstreamer_video::{self as gst_video, prelude::*};
use gstreamer_webrtc as gst_webrtc;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const MAX_PENDING_ICE: usize = 64;
const DESCRIPTION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalingMessage {
    Sdp {
        type_: String,
        sdp: String,
    },
    Ice {
        candidate: String,
        sdp_mline_index: u32,
        /// Actual `a=mid:` of that m-section. The phone requires `id` and uses
        /// it as the SDP mid, so the index alone is not enough.
        mid: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    pub incoming_capacity: usize,
    pub outgoing_capacity: usize,
    pub event_capacity: usize,
    pub audio_enabled: bool,
    pub h265_enabled: bool,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            incoming_capacity: 64,
            outgoing_capacity: 64,
            event_capacity: 32,
            audio_enabled: crate::cast_settings::audio(),
            h265_enabled: crate::cast_settings::h265(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaEvent {
    Error(String),
    Ended,
}

/// Frame counters are incremented on streaming threads without allocating.
/// Dimensions change on decoded CAPS events, not on each frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiverStatus {
    pub running: bool,
    pub remote_description_set: bool,
    pub input_ready: bool,
    pub frames_received: u64,
    pub frames_decoded: u64,
    pub frames_presented: u64,
    pub video_pad_seen: bool,
    pub width: u32,
    pub height: u32,
    pub last_error: Option<String>,
}

/// Square-pixel display size, bounded on both portrait and landscape long sides.
fn display_size(width: u32, height: u32, par: gst::Fraction) -> (u32, u32) {
    const MAX_SIDE: f64 = 1560.0;
    let ratio = if par.numer() > 0 && par.denom() > 0 {
        par.numer() as f64 / par.denom() as f64
    } else {
        1.0
    };
    let width = width.max(2) as f64 * ratio;
    let height = height.max(2) as f64;
    let scale = (MAX_SIDE / width.max(height)).min(1.0);
    (
        ((width * scale) as u32 & !1).max(2),
        ((height * scale) as u32 & !1).max(2),
    )
}

fn packed_bgra(plane: &[u8], stride: i32, width: u32, height: u32) -> Result<Vec<u8>> {
    let row_bytes = (width as usize)
        .checked_mul(4)
        .context("BGRA row size overflow")?;
    let stride = usize::try_from(stride).context("negative BGRA stride")?;
    let height = height as usize;
    if row_bytes == 0 || height == 0 || stride < row_bytes {
        bail!("invalid BGRA dimensions or stride");
    }
    let required = (height - 1)
        .checked_mul(stride)
        .and_then(|offset| offset.checked_add(row_bytes))
        .context("BGRA plane size overflow")?;
    if plane.len() < required {
        bail!("BGRA plane is smaller than its negotiated layout");
    }
    let mut data = Vec::with_capacity(
        row_bytes
            .checked_mul(height)
            .context("BGRA size overflow")?,
    );
    for row in 0..height {
        let offset = row * stride;
        data.extend_from_slice(&plane[offset..offset + row_bytes]);
    }
    Ok(data)
}

/// Latest tightly packed BGRA frame; slow repaints replace rather than queue it.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

#[derive(Default)]
struct SharedStatus {
    running: AtomicBool,
    remote_description_set: AtomicBool,
    frames_received: AtomicU64,
    frames_decoded: AtomicU64,
    frames_presented: AtomicU64,
    latest_frame: Mutex<Option<VideoFrame>>,
    display_width: AtomicU32,
    display_height: AtomicU32,
    input_ready: AtomicBool,
    video_pad_seen: AtomicBool,
    width: AtomicU32,
    height: AtomicU32,
    last_error: Mutex<Option<String>>,
}

impl SharedStatus {
    fn error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn fail(&self, error: impl ToString) {
        let mut slot = self.last_error.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(error.to_string());
        }
    }

    fn snapshot(&self) -> ReceiverStatus {
        ReceiverStatus {
            running: self.running.load(Ordering::Acquire),
            remote_description_set: self.remote_description_set.load(Ordering::Acquire),
            input_ready: self.input_ready.load(Ordering::Acquire),
            frames_received: self.frames_received.load(Ordering::Relaxed),
            frames_decoded: self.frames_decoded.load(Ordering::Relaxed),
            frames_presented: self.frames_presented.load(Ordering::Relaxed),
            video_pad_seen: self.video_pad_seen.load(Ordering::Acquire),
            width: self.width.load(Ordering::Relaxed),
            height: self.height.load(Ordering::Relaxed),
            last_error: self.error(),
        }
    }
}

/// One peer connection. A phone's audio-only peer legitimately decodes no video.
pub struct MediaReceiver {
    incoming: mpsc::Sender<SignalingMessage>,
    outgoing: mpsc::Receiver<SignalingMessage>,
    events: mpsc::Receiver<MediaEvent>,
    stop: Arc<AtomicBool>,
    status: Arc<SharedStatus>,
    inputs: mpsc::Sender<Vec<u8>>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

/// Check prerequisites without creating a window or starting a peer connection.
/// The VA decoder chooses its DRM device; no render node is hardcoded.
pub fn probe() -> Result<()> {
    probe_config(&ReceiverConfig::default())
}

pub fn probe_config(config: &ReceiverConfig) -> Result<()> {
    gst::init().context("initializing GStreamer")?;
    let required = [
        "webrtcbin",
        "vapostproc",
        "appsink",
        "queue",
        "capsfilter",
        "nicesrc",
        "nicesink",
        "dtlsenc",
        "dtlsdec",
        "srtpenc",
        "srtpdec",
        "sctpenc",
        "sctpdec",
    ];
    // Audio elements are only required when the cast audio switch is on.
    let mut required: Vec<&str> = required.to_vec();
    if config.audio_enabled {
        required.extend([
            "rtpopusdepay",
            "opusdec",
            "audioconvert",
            "audioresample",
            "autoaudiosink",
        ]);
    }
    if config.h265_enabled {
        required.extend(["rtph265depay", "h265parse", "vah265dec"]);
    } else {
        required.extend(["rtph264depay", "h264parse", "vah264dec"]);
    }
    let missing: Vec<_> = required
        .into_iter()
        .filter(|name| gst::ElementFactory::find(name).is_none())
        .collect();
    if !missing.is_empty() {
        bail!("required GStreamer factories unavailable: {}; install WebRTC/libnice, DTLS/SRTP/SCTP, VA video and selected audio plugins; software decoding is not supported", missing.join(", "));
    }
    Ok(())
}

pub fn spawn_receiver(config: ReceiverConfig) -> Result<MediaReceiver> {
    probe_config(&config)?;
    if [
        config.incoming_capacity,
        config.outgoing_capacity,
        config.event_capacity,
    ]
    .contains(&0)
    {
        bail!("cast media channel capacities must be non-zero");
    }
    let (incoming, incoming_rx) = mpsc::channel(config.incoming_capacity);
    let (outgoing_tx, outgoing) = mpsc::channel(config.outgoing_capacity);
    let (event_tx, events) = mpsc::channel(config.event_capacity);
    let (input_tx, input_rx) = mpsc::channel(config.incoming_capacity.max(16));
    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(SharedStatus::default());
    let worker_stop = stop.clone();
    let worker_status = status.clone();
    let thread = thread::Builder::new()
        .name("oplus-media-receiver".into())
        .spawn(move || {
            let result = run_pipeline(
                incoming_rx,
                input_rx,
                outgoing_tx,
                &worker_stop,
                &worker_status,
                &config,
            );
            if let Err(error) = result {
                if !worker_stop.load(Ordering::Acquire) {
                    let detail = format!("{error:#}");
                    worker_status.fail(&detail);
                    let _ = event_tx.try_send(MediaEvent::Error(detail));
                }
            }
            worker_status.input_ready.store(false, Ordering::Release);
            worker_status.running.store(false, Ordering::Release);
            let _ = event_tx.try_send(MediaEvent::Ended);
        })
        .context("spawning media receiver")?;
    Ok(MediaReceiver {
        incoming,
        outgoing,
        events,
        stop,
        status,
        inputs: input_tx,
        thread: Mutex::new(Some(thread)),
    })
}

impl MediaReceiver {
    pub fn send(&self, message: SignalingMessage) -> Result<()> {
        self.incoming
            .try_send(message)
            .map_err(|error| anyhow!("cast signaling input queue is full or closed: {error}"))
    }
    pub fn try_outgoing(&mut self) -> Option<SignalingMessage> {
        self.outgoing.try_recv().ok()
    }
    pub fn try_event(&mut self) -> Option<MediaEvent> {
        self.events.try_recv().ok()
    }
    pub fn status(&self) -> ReceiverStatus {
        self.status.snapshot()
    }

    /// Queue reverse-control bytes for the pipeline worker.
    pub fn send_input(&self, frame: &[u8]) -> Result<()> {
        if !self.status.input_ready.load(Ordering::Acquire) {
            bail!("cast data channel is not open");
        }
        let permit = self
            .inputs
            .try_reserve()
            .map_err(|_| anyhow!("cast input queue is closed or full"))?;
        let mut message = Vec::with_capacity(frame.len() + 8);
        message.extend_from_slice(frame);
        permit.send(message);
        Ok(())
    }

    /// Latest decoded frame, or `None` when the previous one was already taken.
    pub fn take_frame(&self) -> Option<VideoFrame> {
        self.status
            .latest_frame
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    /// Join from a background thread, never from a GUI event handler.
    pub fn stop(&self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            worker
                .join()
                .map_err(|_| anyhow!("media receiver worker panicked"))?;
        }
        Ok(())
    }
}

impl Drop for MediaReceiver {
    fn drop(&mut self) {
        // Explicit stop joins on the session's blocking worker. Cancellation may
        // drop us on the GUI/runtime thread, which must not wait for GStreamer.
        self.stop.store(true, Ordering::Release);
    }
}

/// Covers every early error, including invalid SDP and failed negotiation.
struct PipelineGuard(gst::Pipeline);
impl Drop for PipelineGuard {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

// Promise callbacks only copy small signaling structures. The worker waits in
// bounded slices, so neither malformed SDP nor a stalled promise blocks stop.
fn promise_call(
    stop: &AtomicBool,
    action: impl FnOnce(&gst::Promise),
) -> Result<Option<gst::Structure>> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let promise = gst::Promise::with_change_func(move |reply| {
        let result = reply
            .map(|value| value.map(|s| s.to_owned()))
            .map_err(|error| format!("GStreamer promise failed: {error:?}"));
        let _ = tx.try_send(result);
    });
    action(&promise);
    let deadline = Instant::now() + DESCRIPTION_TIMEOUT;
    loop {
        if stop.load(Ordering::Acquire) {
            promise.interrupt();
            bail!("cast media stopped");
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Ok(reply)) => {
                if let Some(error) = reply
                    .as_ref()
                    .and_then(|s| s.get::<gst::glib::Error>("error").ok())
                {
                    return Err(anyhow!(error.to_string()));
                }
                return Ok(reply);
            }
            Ok(Err(error)) => bail!("{error}"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("GStreamer promise disconnected")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                promise.interrupt();
                bail!("GStreamer description operation timed out");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// One-line `m=` summary so the offer/answer shape is visible in the log.
fn mline_summary(sdp: &str) -> String {
    let mut lines = Vec::new();
    let mut current: Option<String> = None;
    for line in sdp.lines().map(|line| line.trim_end_matches('\r')) {
        if let Some(section) = line.strip_prefix("m=") {
            if let Some(previous) = current.take() {
                lines.push(previous);
            }
            current = Some(
                section
                    .split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            continue;
        }
        if let Some(mid) = line.strip_prefix("a=mid:") {
            if let Some(section) = current.as_mut() {
                section.push_str(&format!(" mid={mid}"));
            }
            continue;
        }
        if let Some(section) = current.as_mut() {
            if line == "a=sendonly"
                || line == "a=recvonly"
                || line == "a=inactive"
                || line == "a=sendrecv"
            {
                section.push_str(&format!(" {}", &line[2..]));
            }
        }
    }
    if let Some(last) = current {
        lines.push(last);
    }
    lines.join(" | ")
}

fn publish_description(
    webrtc: &gst::Element,
    kind: &str,
    outgoing: &mpsc::Sender<SignalingMessage>,
    mids: &Arc<Mutex<Vec<String>>>,
    stop: &AtomicBool,
) -> Result<()> {
    let action = if kind == "offer" {
        "create-offer"
    } else {
        "create-answer"
    };
    let reply = promise_call(stop, |promise| {
        webrtc.emit_by_name::<()>(action, &[&None::<gst::Structure>, promise]);
    })?
    .ok_or_else(|| anyhow!("{action} returned no description"))?;
    let generated = reply
        .get::<gst_webrtc::WebRTCSessionDescription>(kind)
        .context("reading generated SDP")?;
    // gstwebrtcbin publishes bundled m-sections as `m=<kind> 0` +
    // `a=bundle-only`. The captured official offer instead uses port 9 with no
    // bundle-only attribute, and the phone closes the cast on the placeholder
    // form. Rewrite only those two wire details; directions and codecs stay.
    let sdp = if kind == "offer" {
        normalize_offer(
            &generated
                .sdp()
                .as_text()
                .context("serializing generated SDP")?,
        )
    } else {
        generated
            .sdp()
            .as_text()
            .context("serializing generated SDP")?
    };
    let description = if kind == "offer" {
        let parsed = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
            .context("parsing normalized offer")?;
        gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Offer, parsed)
    } else {
        generated
    };
    *mids.lock().unwrap_or_else(|error| error.into_inner()) = offer_mids(&sdp);
    promise_call(stop, |promise| {
        webrtc.emit_by_name::<()>("set-local-description", &[&description, promise]);
    })
    .context("setting local SDP")?;
    tracing::info!("cast media local {kind} [{}]", mline_summary(&sdp));
    tracing::debug!("cast media local {kind} SDP ({} bytes):\n{sdp}", sdp.len());
    outgoing
        .try_send(SignalingMessage::Sdp {
            type_: kind.into(),
            sdp,
        })
        .map_err(|_| anyhow!("outgoing SDP queue is full or closed"))?;
    tracing::info!("cast media published local {kind}");
    Ok(())
}

/// Queue the buffered candidates only after the offer occupies the head of the
/// channel, preserving the offer-then-candidate order the peer requires.
fn flush_pending_ice(
    outgoing: &mpsc::Sender<SignalingMessage>,
    pending: &Arc<Mutex<VecDeque<SignalingMessage>>>,
) -> Result<()> {
    let mut pending = pending.lock().unwrap_or_else(|error| error.into_inner());
    while !pending.is_empty() {
        match outgoing.try_reserve() {
            Ok(permit) => {
                permit.send(pending.pop_front().expect("pending ICE exists"));
            }
            Err(mpsc::error::TrySendError::Full(_)) => break,
            Err(mpsc::error::TrySendError::Closed(_)) => bail!("outgoing ICE queue is closed"),
        }
    }
    Ok(())
}

/// Match the captured official offer wire details that the phone depends on:
/// port 9 instead of RFC 8843 bundle-only placeholders, and no `rtcp-mux-only`.
/// The mids stay exactly as webrtcbin generated them; transceivers are matched
/// by mid when the local description is applied, and candidates carry that mid
/// as `id`.
fn normalize_offer(sdp: &str) -> String {
    let mut lines = Vec::new();
    let mut source = sdp.lines().map(|line| line.trim_end_matches('\r'));
    while let Some(line) = source.next() {
        if line == "a=bundle-only" || line == "a=rtcp-mux-only" {
            continue;
        }
        if line == "a=ice-options:trickle" {
            // Captured official offer advertises renomination as well.
            lines.push("a=ice-options:trickle renomination".to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("m=") {
            let mut parts = rest.split_whitespace();
            let bundled = source
                .clone()
                .take_while(|line| !line.starts_with("m="))
                .any(|line| line == "a=bundle-only");
            if let (Some(kind), Some("0"), true) = (parts.next(), parts.next(), bundled) {
                let mut rewritten = format!("m={kind} 9");
                for part in parts {
                    rewritten.push(' ');
                    rewritten.push_str(part);
                }
                lines.push(rewritten);
                continue;
            }
        }
        lines.push(line.to_string());
    }
    patch_audio_section(&mut lines);
    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    text
}

/// Match the jointly tested Opus parameters and extension set of the official offer.
fn patch_audio_section(lines: &mut Vec<String>) {
    const EXTMAPS: [&str; 6] = [
        "a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level",
        "a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
        "a=extmap:3 http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time",
        "a=extmap:4 http://www.webrtc.org/experiments/rtp-hdrext/odmf-audio-timing",
        "a=extmap:5 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
        "a=extmap:6 urn:ietf:params:rtp-hdrext:sdes:mid",
    ];
    const FMTP: &str = "a=fmtp:111 minptime=10;sr=48000;stereo=1;useinbandfec=0";

    let mut start = 0;
    while start < lines.len() {
        let mut end = start + 1;
        while end < lines.len() && !lines[end].starts_with("m=") {
            end += 1;
        }
        if lines[start].starts_with("m=audio ")
            && lines[start..end]
                .iter()
                .any(|line| line.starts_with("a=mid:"))
            && lines[start..end]
                .iter()
                .any(|line| line.starts_with("a=rtpmap:111 "))
        {
            for index in (start..end).rev() {
                if lines[index].starts_with("a=extmap:") || lines[index].starts_with("a=fmtp:111 ")
                {
                    lines.remove(index);
                    end -= 1;
                }
            }
            let rtpmap = start
                + lines[start..end]
                    .iter()
                    .position(|line| line.starts_with("a=rtpmap:111 "))
                    .unwrap();
            lines[rtpmap] = "a=rtpmap:111 opus/48000/2".to_string();
            lines.insert(rtpmap + 1, FMTP.to_string());
            end += 1;
            let mid = start
                + lines[start..end]
                    .iter()
                    .position(|line| line.starts_with("a=mid:"))
                    .unwrap();
            lines.splice(
                mid + 1..mid + 1,
                EXTMAPS.iter().map(|line| (*line).to_string()),
            );
            end += EXTMAPS.len();
        }
        start = end;
    }
}

fn audio_only_without_data(sdp: &gst_sdp::SDPMessageRef) -> bool {
    let session_direction = sdp
        .attributes()
        .map(|attribute| attribute.key())
        .find(|key| matches!(*key, "inactive" | "recvonly" | "sendonly" | "sendrecv"))
        .unwrap_or("sendrecv");
    let mut audio = false;
    for media in sdp.medias().filter(|media| media.port() != 0) {
        if media.media() == Some("application") {
            return false;
        }
        let direction = media
            .attributes()
            .map(|attribute| attribute.key())
            .find(|key| matches!(*key, "inactive" | "recvonly" | "sendonly" | "sendrecv"))
            .unwrap_or(session_direction);
        if matches!(direction, "inactive" | "recvonly") {
            continue;
        }
        match media.media() {
            Some("video") => return false,
            Some("audio") => audio = true,
            _ => {}
        }
    }
    audio
}

/// Ordered `a=mid:` values of the local offer, used to answer ICE candidates
/// with the mid the phone expects in `id`.
fn offer_mids(sdp: &str) -> Vec<String> {
    sdp.lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("a=mid:"))
        .map(|mid| mid.trim().to_string())
        .collect()
}

fn attach_video(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    caps: &gst::Caps,
    status: &Arc<SharedStatus>,
) -> Result<()> {
    if pad.direction() != gst::PadDirection::Src {
        return Ok(());
    }
    let structure = caps
        .structure(0)
        .ok_or_else(|| anyhow!("incoming WebRTC pad has no caps"))?;
    if structure.get::<&str>("media").ok() != Some("video") {
        return Ok(());
    }
    if status.video_pad_seen.swap(true, Ordering::AcqRel) {
        bail!("phone supplied multiple video tracks on one peer connection");
    }
    // Decode and scale on the GPU, then download square-pixel BGRA for GPUI.
    let (depay, parse, decoder) = match structure.get::<&str>("encoding-name").unwrap_or("") {
        "H264" => ("rtph264depay", "h264parse", "vah264dec"),
        "H265" => ("rtph265depay", "h265parse", "vah265dec"),
        other => bail!("phone negotiated an unsupported video codec: {other}"),
    };
    let description = format!(
        "{depay} name=depay request-keyframe=true wait-for-keyframe=true ! \
         {parse} name=parser ! {decoder} name=decoder ! vapostproc name=scale ! \
         capsfilter name=display ! appsink name=frames max-buffers=2 drop=true sync=false"
    );
    let bin =
        gst::parse::bin_from_description(&description, true).context("creating VA decode bin")?;
    let sink = bin
        .static_pad("sink")
        .ok_or_else(|| anyhow!("decode bin has no sink pad"))?;
    let encoded = bin
        .by_name("parser")
        .and_then(|e| e.static_pad("src"))
        .ok_or_else(|| anyhow!("video parser has no source pad"))?;
    let decoded = bin
        .by_name("decoder")
        .and_then(|e| e.static_pad("src"))
        .ok_or_else(|| anyhow!("VA decoder has no source pad"))?;
    let received_status = status.clone();
    encoded.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        received_status
            .frames_received
            .fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
    if let Some(scale_pad) = bin
        .by_name("scale")
        .and_then(|element| element.static_pad("src"))
    {
        let scale_status = status.clone();
        scale_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() {
                if let gst::EventView::Caps(event) = event.view() {
                    tracing::debug!("cast video scaled caps {:?}", event.caps());
                    if let Some(structure) = event.caps().structure(0) {
                        if let Ok(width) = structure.get::<i32>("width") {
                            scale_status
                                .width
                                .store(width.max(0) as u32, Ordering::Relaxed);
                        }
                        if let Ok(height) = structure.get::<i32>("height") {
                            scale_status
                                .height
                                .store(height.max(0) as u32, Ordering::Relaxed);
                        }
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
    let appsink = bin
        .by_name("frames")
        .and_then(|element| element.downcast::<gstreamer_app::AppSink>().ok())
        .ok_or_else(|| anyhow!("decode bin has no appsink"))?;
    appsink.set_max_buffers(2);
    appsink.set_drop(true);
    appsink.set_sync(false);
    let frame_status = status.clone();
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let frame = (|| -> Result<VideoFrame> {
                    let buffer = sample.buffer().context("video sample has no buffer")?;
                    let caps = sample.caps().context("video sample has no caps")?;
                    let info = gst_video::VideoInfo::from_caps(caps)?;
                    if info.format() != gst_video::VideoFormat::Bgra {
                        bail!("video sample is not BGRA");
                    }
                    let mapped = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)?;
                    let stride = mapped.plane_stride()[0];
                    if stride <= 0 || (stride as u32).checked_mul(mapped.height()).is_none() {
                        bail!("unsupported BGRA plane stride");
                    }
                    Ok(VideoFrame {
                        width: mapped.width(),
                        height: mapped.height(),
                        data: packed_bgra(
                            mapped.plane_data(0)?,
                            stride,
                            mapped.width(),
                            mapped.height(),
                        )?,
                    })
                })()
                .map_err(|error| {
                    frame_status.fail(format!("mapping decoded video: {error:#}"));
                    gst::FlowError::Error
                })?;
                *frame_status
                    .latest_frame
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(frame);
                frame_status
                    .frames_presented
                    .fetch_add(1, Ordering::Relaxed);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let display = bin
        .by_name("display")
        .ok_or_else(|| anyhow!("decode bin has no display capsfilter"))?;
    let decoded_status = status.clone();
    let display_for_caps = display.clone();
    decoded.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
        move |_, info| {
            match info.data.as_ref() {
                Some(gst::PadProbeData::Buffer(_)) => {
                    decoded_status
                        .frames_decoded
                        .fetch_add(1, Ordering::Relaxed);
                }
                Some(gst::PadProbeData::Event(event)) => {
                    if let gst::EventView::Caps(event) = event.view() {
                        if let Some(structure) = event.caps().structure(0) {
                            if let (Ok(width), Ok(height)) = (
                                structure.get::<i32>("width"),
                                structure.get::<i32>("height"),
                            ) {
                                if width > 0 && height > 0 {
                                    let par = structure
                                        .get::<gst::Fraction>("pixel-aspect-ratio")
                                        .unwrap_or_else(|_| gst::Fraction::new(1, 1));
                                    let (scaled_width, scaled_height) =
                                        display_size(width as u32, height as u32, par);
                                    if decoded_status.display_width.load(Ordering::Relaxed)
                                        == scaled_width
                                        && decoded_status.display_height.load(Ordering::Relaxed)
                                            == scaled_height
                                    {
                                        return gst::PadProbeReturn::Ok;
                                    }
                                    decoded_status
                                        .display_width
                                        .store(scaled_width, Ordering::Relaxed);
                                    decoded_status
                                        .display_height
                                        .store(scaled_height, Ordering::Relaxed);
                                    let caps = gst::Caps::builder("video/x-raw")
                                        .field("format", "BGRA")
                                        .field("width", scaled_width as i32)
                                        .field("height", scaled_height as i32)
                                        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                                        .build();
                                    display_for_caps.set_property("caps", caps);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            gst::PadProbeReturn::Ok
        },
    );
    pipeline.add(&bin).context("adding decode/display bin")?;
    pad.link(&sink).context("linking incoming video stream")?;
    bin.sync_state_with_parent()
        .context("starting VA decoder")?;
    let encoding = structure.get::<&str>("encoding-name").unwrap_or("unknown");
    tracing::info!("cast media {encoding} VA decode chain attached to incoming video pad");
    Ok(())
}

/// Opus from the phone's audio bus. The sink is picked by GStreamer
/// (`pipewiresink`/`pulsesink`) so the desktop's audio server does the mixing.
fn attach_audio(pipeline: &gst::Pipeline, pad: &gst::Pad) -> Result<()> {
    let bin = gst::parse::bin_from_description(
        "rtpopusdepay name=depay ! queue max-size-buffers=8 leaky=downstream ! \
         opusdec ! audioconvert ! audioresample ! autoaudiosink sync=true",
        true,
    )
    .context("creating Opus playout chain")?;
    let sink = bin
        .static_pad("sink")
        .ok_or_else(|| anyhow!("audio bin has no sink pad"))?;
    pipeline.add(&bin).context("adding audio playout bin")?;
    pad.link(&sink).context("linking incoming Opus stream")?;
    bin.sync_state_with_parent()
        .context("starting Opus playout")?;
    tracing::info!("cast media Opus audio chain attached to incoming audio pad");
    Ok(())
}

fn run_pipeline(
    mut incoming: mpsc::Receiver<SignalingMessage>,
    mut inputs: mpsc::Receiver<Vec<u8>>,
    outgoing: mpsc::Sender<SignalingMessage>,
    stop: &Arc<AtomicBool>,
    status: &Arc<SharedStatus>,
    config: &ReceiverConfig,
) -> Result<()> {
    let mids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let ice_pending: Arc<Mutex<VecDeque<SignalingMessage>>> = Arc::new(Mutex::new(VecDeque::new()));
    let pipeline = gst::Pipeline::with_name("oplus-media-receiver");
    let _guard = PipelineGuard(pipeline.clone());
    let webrtc = gst::ElementFactory::make("webrtcbin")
        .name("peer")
        .build()
        .context("creating WebRTC peer")?;
    webrtc.set_property("latency", 30u32);
    webrtc.set_property_from_str("bundle-policy", "max-bundle");
    pipeline.add(&webrtc)?;

    let h265 = config.h265_enabled;
    let mut video_caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("clock-rate", 90_000i32)
        .field("rtcp-fb-nack-pli", true)
        .field("rtcp-fb-nack", true);
    video_caps = if h265 {
        video_caps
            .field("encoding-name", "H265")
            .field("payload", 35i32)
            .field("profile-id", 1i32)
            .field("level-id", 120i32)
            .field("tier-flag", 0i32)
            .field("tx-mode", "SRST")
    } else {
        video_caps
            .field("encoding-name", "H264")
            .field("payload", 100i32)
            .field("packetization-mode", "1")
            .field("level-asymmetry-allowed", "1")
            .field("profile-level-id", "42e01f")
    };
    let video = video_caps.build();
    let _: gst_webrtc::WebRTCRTPTransceiver = webrtc.emit_by_name(
        "add-transceiver",
        &[&gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly, &video],
    );

    // The phone answers per connection: its audio bus activates `m=audio` and
    // leaves `m=video` inactive, its video bus does the opposite. Both sections
    // therefore belong in our offer.
    let audio_enabled = config.audio_enabled;
    if audio_enabled {
        let audio = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "OPUS")
            .field("clock-rate", 48_000i32)
            .field("payload", 111i32)
            .field("channels", 2i32)
            .build();
        let transceiver: Option<gst_webrtc::WebRTCRTPTransceiver> = webrtc.emit_by_name(
            "add-transceiver",
            &[&gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly, &audio],
        );
        tracing::info!("cast audio transceiver added: {}", transceiver.is_some());
    }

    let weak = pipeline.downgrade();
    let pad_status = status.clone();
    webrtc.connect_pad_added(move |_, pad| {
        // Attach on the first CAPS event, when the media type is known.
        let pipeline = weak.clone();
        let status = pad_status.clone();
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
            let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() else {
                return gst::PadProbeReturn::Ok;
            };
            let gst::EventView::Caps(event) = event.view() else {
                return gst::PadProbeReturn::Ok;
            };
            let caps = event.caps().to_owned();
            let media = caps
                .structure(0)
                .and_then(|structure| structure.get::<&str>("media").ok())
                .map(str::to_owned)
                .unwrap_or_default();
            tracing::debug!("cast media pad caps media={media}");
            if let Some(pipeline) = pipeline.upgrade() {
                let result = if media == "audio" {
                    attach_audio(&pipeline, pad)
                } else {
                    attach_video(&pipeline, pad, &caps, &status)
                };
                if let Err(error) = result {
                    status.fail(format!("{error:#}"));
                }
            }
            gst::PadProbeReturn::Remove
        });
    });
    let ice_status = status.clone();
    let ice_mids = mids.clone();
    let ice_pending_handler = ice_pending.clone();
    webrtc.connect("on-ice-candidate", false, move |values| {
        let candidate = values[2].get::<String>();
        let mline = values[1].get::<u32>();
        match (candidate, mline) {
            (Ok(candidate), Ok(sdp_mline_index)) => {
                let mid = ice_mids
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(sdp_mline_index as usize)
                    .cloned();
                // All candidates share one FIFO; only the worker flushes it,
                // after publishing SDP, so callbacks cannot overtake it.
                let mut pending = ice_pending_handler
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if pending.len() >= MAX_PENDING_ICE {
                    ice_status.fail("too many pending outgoing ICE candidates");
                } else {
                    pending.push_back(SignalingMessage::Ice {
                        candidate,
                        sdp_mline_index,
                        mid,
                    });
                }
            }
            _ => ice_status.fail("webrtcbin emitted an invalid ICE candidate"),
        }
        None
    });
    let connection_status = status.clone();
    let connection_stop = stop.clone();
    webrtc.connect_notify(Some("connection-state"), move |peer, _| {
        let state = peer.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
        tracing::info!("cast media peer connection state {state:?}");
        match state {
            gst_webrtc::WebRTCPeerConnectionState::Failed => {
                connection_status.fail("WebRTC peer connection failed")
            }
            gst_webrtc::WebRTCPeerConnectionState::Closed
                if !connection_stop.load(Ordering::Acquire) =>
            {
                connection_status.fail("WebRTC peer connection closed unexpectedly");
            }
            _ => {}
        }
    });

    pipeline
        .set_state(gst::State::Ready)
        .context("initializing WebRTC transports")?;
    let channel = webrtc
        .emit_by_name::<Option<gst_webrtc::WebRTCDataChannel>>(
            "create-data-channel",
            &[&"OdmfDataChannel", &None::<gst::Structure>],
        )
        .ok_or_else(|| {
            anyhow!("creating phone data channel failed; check GStreamer SCTP plugin")
        })?;
    // The worker serializes data-channel sends with receiver shutdown.
    tracing::info!(
        "cast data channel created id={} label={:?}",
        channel.id(),
        channel.label()
    );
    pipeline
        .set_state(gst::State::Playing)
        .context("starting WebRTC receiver")?;
    status.running.store(true, Ordering::Release);
    publish_description(&webrtc, "offer", &outgoing, &mids, stop)?;
    flush_pending_ice(&outgoing, &ice_pending)?;
    let bus = pipeline
        .bus()
        .ok_or_else(|| anyhow!("receiver pipeline has no bus"))?;
    let mut pending_ice = VecDeque::with_capacity(MAX_PENDING_ICE);
    let mut remote_set = false;
    let mut optional_sctp = false;
    while !stop.load(Ordering::Acquire) {
        let input_ready = channel.ready_state() == gst_webrtc::WebRTCDataChannelState::Open;
        status.input_ready.store(input_ready, Ordering::Release);
        for _ in 0..64 {
            let Ok(frame) = inputs.try_recv() else { break };
            if !input_ready {
                bail!("cast data channel closed with queued input");
            }
            if channel.buffered_amount() > 64 * 1024 {
                bail!("cast data channel input buffer is full");
            }
            // ODMF strips the trailing timestamp before parsing UIBC.
            let mut message = frame;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis() as u64)
                .unwrap_or(0);
            message.extend_from_slice(&now.to_be_bytes());
            tracing::debug!("reverse control send {} bytes", message.len());
            if let Err(error) = channel.send_data_full(Some(&gst::glib::Bytes::from_owned(message)))
            {
                bail!("reverse control send failed: {error}");
            }
        }
        if let Some(error) = status.error() {
            bail!("{error}");
        }
        // Bound each turn so signaling cannot starve errors or stop requests.
        for _ in 0..64 {
            let message = match incoming.try_recv() {
                Ok(message) => message,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return Ok(()),
            };
            match message {
                SignalingMessage::Ice {
                    candidate,
                    sdp_mline_index,
                    mid: _,
                } if !remote_set => {
                    if pending_ice.len() == MAX_PENDING_ICE {
                        bail!(
                            "too many ICE candidates before remote SDP (limit {MAX_PENDING_ICE})"
                        );
                    }
                    pending_ice.push_back((candidate, sdp_mline_index));
                }
                SignalingMessage::Ice {
                    candidate,
                    sdp_mline_index,
                    mid: _,
                } => {
                    webrtc.emit_by_name::<()>("add-ice-candidate", &[&sdp_mline_index, &candidate]);
                }
                SignalingMessage::Sdp { type_, sdp } => {
                    let kind = match type_.as_str() {
                        "answer" => gst_webrtc::WebRTCSDPType::Answer,
                        "offer" => gst_webrtc::WebRTCSDPType::Offer,
                        _ => bail!("unsupported SDP type `{type_}`"),
                    };
                    let parsed = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                        .context("parsing remote SDP")?;
                    let description = gst_webrtc::WebRTCSessionDescription::new(kind, parsed);
                    promise_call(stop, |promise| {
                        webrtc
                            .emit_by_name::<()>("set-remote-description", &[&description, promise]);
                    })
                    .context("applying remote SDP")?;
                    optional_sctp = kind == gst_webrtc::WebRTCSDPType::Answer
                        && audio_only_without_data(description.sdp());
                    remote_set = true;
                    status.remote_description_set.store(true, Ordering::Release);
                    tracing::info!(
                        "cast media applied remote SDP type={type_} [{}]",
                        mline_summary(&description.sdp().as_text().unwrap_or_default())
                    );
                    while let Some((candidate, mline)) = pending_ice.pop_front() {
                        webrtc.emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
                    }
                    if kind == gst_webrtc::WebRTCSDPType::Offer {
                        publish_description(&webrtc, "answer", &outgoing, &mids, stop)?;
                    }
                }
            }
        }
        flush_pending_ice(&outgoing, &ice_pending)?;
        if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(20)) {
            match message.view() {
                gst::MessageView::Error(error) => {
                    let path = error
                        .src()
                        .map(|source| source.path_string())
                        .unwrap_or_default();
                    let sctp_source = error
                        .src()
                        .and_then(|source| source.downcast_ref::<gst::Element>())
                        .and_then(|element| element.factory())
                        .is_some_and(|factory| {
                            matches!(factory.name().as_str(), "sctpenc" | "sctpdec")
                        });
                    if optional_sctp && sctp_source {
                        tracing::warn!(
                            "audio-only peer rejected its unused SCTP transport: {}",
                            error.error()
                        );
                        continue;
                    }
                    bail!(
                        "{}: {} ({})",
                        path,
                        error.error(),
                        error.debug().unwrap_or_default()
                    );
                }
                gst::MessageView::Eos(..) => return Ok(()),
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        audio_only_without_data, display_size, flush_pending_ice, normalize_offer, offer_mids,
        packed_bgra, SignalingMessage,
    };
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    #[test]
    fn display_size_limits_both_orientations_and_applies_par() {
        let square = gstreamer::Fraction::new(1, 1);
        assert_eq!(display_size(1080, 2344, square), (718, 1560));
        assert_eq!(display_size(2344, 1080, square), (1560, 718));
        assert_eq!(
            display_size(720, 576, gstreamer::Fraction::new(16, 15)),
            (768, 576)
        );
        assert_eq!(display_size(1, 1, square), (2, 2));
    }

    #[test]
    fn bgra_copy_removes_padding_and_rejects_invalid_planes() {
        let plane = [1, 2, 3, 4, 99, 99, 99, 99, 5, 6, 7, 8];
        assert_eq!(
            packed_bgra(&plane, 8, 1, 2).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert!(packed_bgra(&plane[..11], 8, 1, 2).is_err());
        assert!(packed_bgra(&plane, 3, 1, 2).is_err());
        assert!(packed_bgra(&plane, -8, 1, 2).is_err());
        assert!(packed_bgra(&plane, 8, 1, 0).is_err());
    }

    #[test]
    fn audio_patching_stays_in_each_section_and_is_idempotent() {
        for attributes in [
            "a=mid:audio0\r\na=rtpmap:111 opus/48000/2\r\n",
            "a=rtpmap:111 opus/48000/2\r\na=mid:audio0\r\n",
        ] {
            let input = format!("v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n{attributes}a=fmtp:111 old=1\r\na=extmap:1 old\r\nm=video 9 UDP/TLS/RTP/SAVPF 100\r\na=mid:video1\r\n");
            let output = normalize_offer(&input);
            let (audio, video) = output.split_once("m=video").unwrap();
            assert_eq!(audio.matches("a=fmtp:111 ").count(), 1);
            assert_eq!(audio.matches("a=extmap:").count(), 6);
            assert!(!video.contains("a=fmtp:111"));
            assert_eq!(normalize_offer(&output), output);
        }
        let two = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:a\r\na=rtpmap:111 opus/48000/2\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:b\r\na=rtpmap:111 opus/48000/2\r\n";
        assert_eq!(normalize_offer(two).matches("a=fmtp:111 ").count(), 2);
    }

    #[test]
    fn normalization_preserves_rejected_sections_and_missing_opus() {
        let input = "v=0\r\nm=audio 0 UDP/TLS/RTP/SAVPF 0\r\na=mid:audio0\r\n";
        assert_eq!(normalize_offer(input), input);
    }

    #[test]
    fn sctp_exception_requires_audio_without_sending_video_or_data() {
        gstreamer::init().unwrap();
        let audio = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n";
        let permits = |text: &str| {
            let sdp = gstreamer_sdp::SDPMessage::parse_buffer(text.as_bytes()).unwrap();
            audio_only_without_data(&sdp)
        };
        assert!(permits(audio));
        assert!(permits(&format!(
            "{audio}m=video 9 UDP/TLS/RTP/SAVPF 100\r\na=inactive\r\n"
        )));
        assert!(!permits(&format!(
            "{audio}m=video 9 UDP/TLS/RTP/SAVPF 100\r\na=sendonly\r\n"
        )));
        assert!(!permits(&format!(
            "{audio}m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n"
        )));
        assert!(!permits("v=0\r\nm=audio 0 UDP/TLS/RTP/SAVPF 111\r\n"));
    }

    #[tokio::test]
    async fn full_ice_queue_preserves_order_until_capacity_returns() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(SignalingMessage::Sdp {
            type_: "offer".into(),
            sdp: "v=0".into(),
        })
        .unwrap();
        let first = SignalingMessage::Ice {
            candidate: "first".into(),
            sdp_mline_index: 0,
            mid: Some("video0".into()),
        };
        let second = SignalingMessage::Ice {
            candidate: "second".into(),
            sdp_mline_index: 0,
            mid: Some("video0".into()),
        };
        let pending = Arc::new(Mutex::new(VecDeque::from([first.clone(), second.clone()])));
        flush_pending_ice(&tx, &pending).unwrap();
        assert_eq!(pending.lock().unwrap().len(), 2);
        rx.try_recv().unwrap();
        flush_pending_ice(&tx, &pending).unwrap();
        assert_eq!(rx.try_recv().unwrap(), first);
        flush_pending_ice(&tx, &pending).unwrap();
        assert_eq!(rx.try_recv().unwrap(), second);
        pending.lock().unwrap().push_back(first);
        drop(rx);
        assert!(flush_pending_ice(&tx, &pending).is_err());
    }

    #[tokio::test]
    async fn buffered_candidates_follow_the_offer_on_the_queue() {
        let (tx, mut rx) = mpsc::channel(8);
        let pending = Arc::new(Mutex::new(VecDeque::from([SignalingMessage::Ice {
            candidate: "candidate:1 1 udp 1 127.0.0.1 1 typ host".into(),
            sdp_mline_index: 1,
            mid: Some("video1".into()),
        }])));
        tx.try_send(SignalingMessage::Sdp {
            type_: "offer".into(),
            sdp: "v=0".into(),
        })
        .unwrap();
        flush_pending_ice(&tx, &pending).unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            SignalingMessage::Sdp { .. }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            SignalingMessage::Ice { .. }
        ));
        assert!(pending.lock().unwrap().is_empty());
    }

    const OFFER: &str = "v=0\r\n\
a=ice-options:trickle\r\n\
a=group:BUNDLE audio0 video1 application2\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:audio0\r\n\
a=rtcp-mux-only\r\n\
m=video 0 UDP/TLS/RTP/SAVPF 102\r\n\
a=bundle-only\r\n\
a=mid:video1\r\n\
m=application 0 UDP/DTLS/SCTP webrtc-datachannel\r\n\
a=bundle-only\r\n\
a=mid:application2\r\n";

    #[test]
    fn offer_normalization_matches_the_captured_wire_details() {
        let output = normalize_offer(OFFER);
        assert!(
            output.contains("m=video 9 UDP/TLS/RTP/SAVPF 102"),
            "{output}"
        );
        assert!(
            output.contains("m=application 9 UDP/DTLS/SCTP webrtc-datachannel"),
            "{output}"
        );
        assert!(!output.contains("bundle-only"), "{output}");
        assert!(!output.contains("rtcp-mux-only"), "{output}");
        // Mids and BUNDLE must stay exactly as webrtcbin generated them, because
        // set-local-description matches transceivers by mid.
        assert!(
            output.contains("a=group:BUNDLE audio0 video1 application2"),
            "{output}"
        );
        assert_eq!(offer_mids(&output), ["audio0", "video1", "application2"]);
    }
}

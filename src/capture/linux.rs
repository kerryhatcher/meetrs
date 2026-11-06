//! Linux system-audio + microphone capture over PipeWire.
//!
//! Output matches macOS: interleaved f32 at `SAMPLE_RATE`, three channels.
//! Index 0 is the microphone (mono), indices 1 and 2 are system audio (stereo).
//!
//! Two capture streams run on one PipeWire main loop, on a thread this module
//! owns:
//!
//!   - mic: the default source, plain capture, requested as 1 channel F32LE.
//!   - system: the default sink's monitor (`stream.capture.sink = true`),
//!     requested as 2 channels F32LE.
//!
//! Both are requested at 48 kHz. PipeWire's adapter converts the device format
//! to the requested one, so this module never resamples or remixes.
//!
//! Each stream's `process` callback pushes its samples into its own SPSC ring
//! (lock-free, no allocation). A 10 ms timer on the loop thread runs `Mixer`,
//! which pairs mic and system frames into the output ring.
//!
//! CLOCK SYNC, AND ITS LIMITS. This is NOT the single-IOProc guarantee macOS has.
//! The macOS aggregate device runs both legs from one IOProc, so Core Audio syncs
//! them sample-accurately. Here the two legs are separate streams. They are
//! aligned only by the PipeWire graph clock, which both follow after adapter
//! resampling. Consequences:
//!   - Pairing is by arrival order, not by timestamp. The legs may sit a few
//!     milliseconds apart, and that offset can vary.
//!   - A leg that runs ahead of the other is buffered up to `LEG_TOLERANCE`
//!     (50 ms). Past that, the lagging leg is padded with zeros for the whole
//!     backlog, which resyncs the two. So a stall or a steady clock drift shows
//!     up as a zero-filled gap of up to about 50 ms on the lagging leg. It does
//!     not show up as lost or misordered samples.
//!   - Default-device changes mid-session are not followed. The streams stay
//!     bound to the sink and source that were default at `start()`.
//!
//! CONTRACT (do not change):
//!   pub fn start(ring_samples: usize) -> Result<(CaptureInfo, rtrb::Consumer<f32>)>
//!   pub fn stop()

use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::properties::properties;
use pw::spa;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::pod::Pod;
use pw::stream::{Stream, StreamBox, StreamFlags, StreamState};
use pw::types::ObjectType;

use crate::types::{CaptureInfo, SAMPLE_RATE};

/// Output channels: mic (1) then system stereo (2).
const OUT_CHANNELS: usize = 3;
const MIC_CHANNELS: usize = 1;
const SYS_CHANNELS: usize = 2;

/// How far one leg may run ahead of the other before the lagging leg is padded
/// with zeros. This is also the latency bound added by the pairing.
const LEG_TOLERANCE_FRAMES: usize = (SAMPLE_RATE as usize) * 50 / 1000;
/// Per-leg ring capacity, in samples. One second of mono or stereo, which is far
/// more than the 10 ms timer ever lets build up.
const MIC_RING_SAMPLES: usize = SAMPLE_RATE as usize;
const SYS_RING_SAMPLES: usize = SAMPLE_RATE as usize * SYS_CHANNELS;
/// How often the loop thread pairs and emits frames.
const MIXER_TICK: Duration = Duration::from_millis(10);
/// How long to wait for the default sink/source to appear, and separately for
/// both streams to reach streaming, before `start()` gives up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Samples dropped because a ring was full, on either the output or a leg. The
/// writer polls this and surfaces `Status::Overrun` on increase.
static DROPPED_SAMPLES: AtomicU64 = AtomicU64::new(0);
// Diagnostics for `io_stats()`, mirroring the macOS counters. Relaxed throughout.
static CALLBACKS: AtomicU64 = AtomicU64::new(0);
static SHAPE_MISMATCH: AtomicU64 = AtomicU64::new(0);
static EMPTY_CALLBACKS: AtomicU64 = AtomicU64::new(0);
static LAST_N_DATAS: AtomicU64 = AtomicU64::new(0);

/// Mirrors the macOS `IoStats`, with these meanings on PipeWire:
/// - `calls`: process callbacks across both legs.
/// - `shape_mismatch`: callbacks with no data block, or a byte count that is not
///   a whole number of frames.
/// - `empty`: callbacks that delivered zero bytes.
/// - `last_n_buffers`: data blocks in the most recent callback.
/// - `expected_n_buffers`: 1 per leg (2 in total).
#[derive(Debug, Clone, Copy)]
pub struct IoStats {
    pub calls: u64,
    pub shape_mismatch: u64,
    pub empty: u64,
    pub last_n_buffers: u64,
    pub expected_n_buffers: u64,
}

pub fn dropped_samples() -> u64 {
    DROPPED_SAMPLES.load(Ordering::Relaxed)
}

pub fn io_stats() -> IoStats {
    IoStats {
        calls: CALLBACKS.load(Ordering::Relaxed),
        shape_mismatch: SHAPE_MISMATCH.load(Ordering::Relaxed),
        empty: EMPTY_CALLBACKS.load(Ordering::Relaxed),
        last_n_buffers: LAST_N_DATAS.load(Ordering::Relaxed),
        expected_n_buffers: 2,
    }
}

// ---------------------------------------------------------------------------
// Pure pairing logic. No PipeWire types here, so it is unit-tested directly.
// ---------------------------------------------------------------------------

/// One input leg as the mixer sees it. `queued` counts samples, not frames.
pub(crate) trait Leg {
    fn queued(&self) -> usize;
    fn pop(&mut self) -> Option<f32>;
}

/// The output ring as the mixer sees it. `free` counts samples.
pub(crate) trait Out {
    fn free(&self) -> usize;
    fn push(&mut self, sample: f32);
}

impl Leg for rtrb::Consumer<f32> {
    fn queued(&self) -> usize {
        self.slots()
    }
    fn pop(&mut self) -> Option<f32> {
        rtrb::Consumer::pop(self).ok()
    }
}

impl Out for rtrb::Producer<f32> {
    fn free(&self) -> usize {
        self.slots()
    }
    fn push(&mut self, sample: f32) {
        // Callers check `free()` first, so this never fails.
        let _ = rtrb::Producer::push(self, sample);
    }
}

pub(crate) struct Mixer<M, S, O> {
    mic: M,
    sys: S,
    out: O,
    tolerance_frames: usize,
}

impl<M: Leg, S: Leg, O: Out> Mixer<M, S, O> {
    pub(crate) fn new(mic: M, sys: S, out: O, tolerance_frames: usize) -> Self {
        Self {
            mic,
            sys,
            out,
            tolerance_frames,
        }
    }

    /// Emit every frame both legs can supply. If one leg is still missing data and
    /// the other is more than `tolerance_frames` ahead, pad the missing leg with
    /// zeros for the whole backlog. Otherwise leave the backlog for the next tick.
    ///
    /// Returns the samples discarded because the output ring was full. A whole
    /// frame is discarded at once, so each one adds `OUT_CHANNELS`.
    pub(crate) fn tick(&mut self) -> u64 {
        let mut overrun = 0;

        while self.mic.queued() >= MIC_CHANNELS && self.sys.queued() >= SYS_CHANNELS {
            let m = self.mic.pop().unwrap_or(0.0);
            let l = self.sys.pop().unwrap_or(0.0);
            let r = self.sys.pop().unwrap_or(0.0);
            overrun += self.emit(m, l, r);
        }

        // After pairing, at most one leg has whole frames left.
        let mic_ahead = self.mic.queued() / MIC_CHANNELS;
        let sys_ahead = self.sys.queued() / SYS_CHANNELS;
        if mic_ahead > self.tolerance_frames && sys_ahead == 0 {
            while self.mic.queued() >= MIC_CHANNELS {
                let m = self.mic.pop().unwrap_or(0.0);
                overrun += self.emit(m, 0.0, 0.0);
            }
        } else if sys_ahead > self.tolerance_frames && mic_ahead == 0 {
            while self.sys.queued() >= SYS_CHANNELS {
                let l = self.sys.pop().unwrap_or(0.0);
                let r = self.sys.pop().unwrap_or(0.0);
                overrun += self.emit(0.0, l, r);
            }
        }
        overrun
    }

    /// Write one frame, or return `OUT_CHANNELS` if the ring has no room for it.
    /// A partial frame would shift every later channel, so it is all or nothing.
    fn emit(&mut self, m: f32, l: f32, r: f32) -> u64 {
        if self.out.free() < OUT_CHANNELS {
            return OUT_CHANNELS as u64;
        }
        self.out.push(m);
        self.out.push(l);
        self.out.push(r);
        0
    }
}

// ---------------------------------------------------------------------------
// PipeWire backend.
// ---------------------------------------------------------------------------

/// The per-leg state the process callback needs. Moved into the stream listener,
/// so the callback reaches it without any lock.
struct LegData {
    name: &'static str,
    channels: usize,
    producer: rtrb::Producer<f32>,
}

/// Resolved default sink and source node names, filled in by the metadata
/// listener on the loop thread.
#[derive(Default)]
struct Defaults {
    sink: Option<String>,
    source: Option<String>,
}

/// Startup handshake. `start()` waits on the channel this holds until both
/// streams are streaming, or until one fails. It is consumed once.
struct Startup {
    ready: RefCell<Option<mpsc::Sender<Result<(), String>>>>,
    streaming: Cell<u8>,
}

const MIC_STREAMING: u8 = 1;
const SYS_STREAMING: u8 = 2;

impl Startup {
    fn fail(&self, msg: String) {
        if let Some(tx) = self.ready.borrow_mut().take() {
            let _ = tx.send(Err(msg));
        }
    }

    fn on_state(&self, leg: &str, bit: u8, state: &StreamState) {
        match state {
            StreamState::Error(e) => self.fail(format!("{leg} stream failed: {e}")),
            StreamState::Streaming => {
                self.streaming.set(self.streaming.get() | bit);
                if self.streaming.get() == MIC_STREAMING | SYS_STREAMING
                    && let Some(tx) = self.ready.borrow_mut().take()
                {
                    let _ = tx.send(Ok(()));
                }
            }
            _ => {}
        }
    }
}

/// Parse `"name"` out of a default-device metadata value, e.g.
/// `{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}`.
fn node_name(value: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(value).ok()?;
    v.get("name")?.as_str().map(str::to_owned)
}

/// The F32LE format request for one leg. Rate and channel count are fixed, so
/// PipeWire's adapter converts whatever the device runs at.
fn format_param(channels: u32) -> Result<Vec<u8>> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(channels);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let (cursor, _) = spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("serializing the format request: {e:?}"))?;
    Ok(cursor.into_inner())
}

/// Process callback for one leg. Runs on PipeWire's realtime thread. It does not
/// allocate or lock. It pushes whole frames into the leg's ring, or counts them
/// as dropped if the ring is full.
fn on_process(stream: &Stream, leg: &mut LegData) {
    CALLBACKS.fetch_add(1, Ordering::Relaxed);
    let Some(mut buffer) = stream.dequeue_buffer() else {
        SHAPE_MISMATCH.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let datas = buffer.datas_mut();
    LAST_N_DATAS.store(datas.len() as u64, Ordering::Relaxed);
    let Some(data) = datas.first_mut() else {
        SHAPE_MISMATCH.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let (offset, size) = {
        let chunk = data.chunk();
        (chunk.offset() as usize, chunk.size() as usize)
    };
    let Some(bytes) = data.data() else {
        SHAPE_MISMATCH.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let end = offset.saturating_add(size).min(bytes.len());
    let bytes = &bytes[offset.min(end)..end];
    if bytes.is_empty() {
        EMPTY_CALLBACKS.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let frame_bytes = leg.channels * size_of::<f32>();
    if bytes.len() % frame_bytes != 0 {
        SHAPE_MISMATCH.fetch_add(1, Ordering::Relaxed);
    }
    for frame in bytes.chunks_exact(frame_bytes) {
        if leg.producer.slots() < leg.channels {
            DROPPED_SAMPLES.fetch_add(leg.channels as u64, Ordering::Relaxed);
            continue;
        }
        for sample in frame.as_chunks::<4>().0 {
            let _ = leg.producer.push(f32::from_le_bytes(*sample));
        }
    }
}

/// Everything that lives on the PipeWire thread. Built and dropped there, since
/// PipeWire objects are not `Send`.
fn run_loop(
    ready: mpsc::Sender<Result<(), String>>,
    quit: pw::channel::Receiver<()>,
    out: rtrb::Producer<f32>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| anyhow!("creating the PipeWire main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| anyhow!("creating the PipeWire context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| anyhow!("PipeWire is not reachable ({e}); is the PipeWire daemon running?"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| anyhow!("getting the PipeWire registry: {e}"))?;

    // --- phase 1: resolve the default sink and source -------------------------
    // Both names come from the "default" metadata object. Its listener keeps them
    // current, but the streams bind to whatever was default at start().
    let defaults = Rc::new(RefCell::new(Defaults::default()));
    let metadata: Rc<RefCell<Option<(Metadata, MetadataListener)>>> = Rc::new(RefCell::new(None));
    let registry_weak = registry.downgrade();
    let _registry_listener = {
        let defaults = defaults.clone();
        let metadata = metadata.clone();
        registry
            .add_listener_local()
            .global(move |obj| {
                if obj.type_ != ObjectType::Metadata {
                    return;
                }
                let Some(props) = obj.props else { return };
                if props.get("metadata.name") != Some("default") {
                    return;
                }
                let Some(reg) = registry_weak.upgrade() else {
                    return;
                };
                let Ok(md): Result<Metadata, _> = reg.bind(obj) else {
                    return;
                };
                let defaults = defaults.clone();
                let listener = md
                    .add_listener_local()
                    .property(move |_subject, key, _type, value| {
                        let slot = match key {
                            Some("default.audio.sink") => Some(true),
                            Some("default.audio.source") => Some(false),
                            _ => None,
                        };
                        if let Some(is_sink) = slot {
                            let name = value.and_then(node_name);
                            let mut d = defaults.borrow_mut();
                            if is_sink {
                                d.sink = name;
                            } else {
                                d.source = name;
                            }
                        }
                        0
                    })
                    .register();
                *metadata.borrow_mut() = Some((md, listener));
            })
            .register()
    };

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        {
            let d = defaults.borrow();
            if d.sink.is_some() && d.source.is_some() {
                break;
            }
        }
        if Instant::now() >= deadline {
            let d = defaults.borrow();
            bail!(
                "no default audio sink and source found within {}s (sink: {:?}, source: {:?}). \
                 Is a session manager such as WirePlumber running?",
                STARTUP_TIMEOUT.as_secs(),
                d.sink,
                d.source
            );
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(20)));
    }
    let (sink_name, source_name) = {
        let d = defaults.borrow();
        (
            d.sink.clone().unwrap_or_default(),
            d.source.clone().unwrap_or_default(),
        )
    };

    // --- phase 2: the two streams and the mixer -------------------------------
    let (mic_prod, mic_cons) = rtrb::RingBuffer::<f32>::new(MIC_RING_SAMPLES);
    let (sys_prod, sys_cons) = rtrb::RingBuffer::<f32>::new(SYS_RING_SAMPLES);

    let mic_props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::TARGET_OBJECT => source_name.as_str(),
    };
    let sys_props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::TARGET_OBJECT => sink_name.as_str(),
        *pw::keys::STREAM_CAPTURE_SINK => "true",
    };
    let mic = StreamBox::new(&core, "meetrs-mic", mic_props)
        .map_err(|e| anyhow!("creating the microphone stream: {e}"))?;
    let sys = StreamBox::new(&core, "meetrs-system", sys_props)
        .map_err(|e| anyhow!("creating the system-audio stream: {e}"))?;

    let startup = Rc::new(Startup {
        ready: RefCell::new(Some(ready)),
        streaming: Cell::new(0),
    });

    // Listeners are declared after the streams, so they drop first.
    let s1 = startup.clone();
    let _mic_listener = mic
        .add_local_listener_with_user_data(LegData {
            name: "microphone",
            channels: MIC_CHANNELS,
            producer: mic_prod,
        })
        .state_changed(move |_, _, _, new| s1.on_state("microphone", MIC_STREAMING, &new))
        .param_changed({
            let s = startup.clone();
            move |_, leg, id, param| check_format(&s, leg, id, param)
        })
        .process(on_process)
        .register()
        .map_err(|e| anyhow!("registering the microphone stream listener: {e}"))?;

    let s2 = startup.clone();
    let _sys_listener = sys
        .add_local_listener_with_user_data(LegData {
            name: "system",
            channels: SYS_CHANNELS,
            producer: sys_prod,
        })
        .state_changed(move |_, _, _, new| s2.on_state("system", SYS_STREAMING, &new))
        .param_changed({
            let s = startup.clone();
            move |_, leg, id, param| check_format(&s, leg, id, param)
        })
        .process(on_process)
        .register()
        .map_err(|e| anyhow!("registering the system-audio stream listener: {e}"))?;

    let mic_bytes = format_param(MIC_CHANNELS as u32)?;
    let sys_bytes = format_param(SYS_CHANNELS as u32)?;
    let mic_pod = Pod::from_bytes(&mic_bytes).ok_or_else(|| anyhow!("bad mic format pod"))?;
    let sys_pod = Pod::from_bytes(&sys_bytes).ok_or_else(|| anyhow!("bad system format pod"))?;
    let flags = StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS;
    mic.connect(spa::utils::Direction::Input, None, flags, &mut [mic_pod])
        .map_err(|e| anyhow!("connecting the microphone stream to {source_name:?}: {e}"))?;
    sys.connect(spa::utils::Direction::Input, None, flags, &mut [sys_pod])
        .map_err(|e| anyhow!("connecting the system-audio stream to {sink_name:?}: {e}"))?;

    // --- phase 3: pair the legs on a timer, quit on request --------------------
    let mixer = RefCell::new(Mixer::new(mic_cons, sys_cons, out, LEG_TOLERANCE_FRAMES));
    let timer = mainloop.loop_().add_timer(move |_| {
        let overrun = mixer.borrow_mut().tick();
        if overrun > 0 {
            DROPPED_SAMPLES.fetch_add(overrun, Ordering::Relaxed);
        }
    });
    let _ = timer.update_timer(Some(MIXER_TICK), Some(MIXER_TICK));

    let weak = mainloop.downgrade();
    let _quit = quit.attach(mainloop.loop_(), move |()| {
        if let Some(ml) = weak.upgrade() {
            ml.quit();
        }
    });

    // Locals drop in reverse declaration order: the quit receiver and timer, then
    // the listeners, then the streams. That is the order PipeWire needs.
    mainloop.run();
    Ok(())
}

/// `param_changed` for one leg. Fails startup if PipeWire negotiated anything
/// other than the 48 kHz, channel-count format we asked for, or if the format
/// pod cannot be read at all: an unreadable format is not one we can vouch for,
/// so the stream must not run unchecked.
fn check_format(startup: &Startup, leg: &mut LegData, id: u32, param: Option<&Pod>) {
    if id != spa::param::ParamType::Format.as_raw() {
        return;
    }
    let Some(param) = param else { return };
    let (media_type, media_subtype) = match format_utils::parse_format(param) {
        Ok(parsed) => parsed,
        Err(e) => {
            startup.fail(format!(
                "could not read the PipeWire format for the {} leg: {e}",
                leg.name
            ));
            return;
        }
    };
    if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
        return;
    }
    let mut info = AudioInfoRaw::new();
    if let Err(e) = info.parse(param) {
        startup.fail(format!(
            "could not read the PipeWire audio format for the {} leg: {e}",
            leg.name
        ));
        return;
    }
    if info.rate() != SAMPLE_RATE || info.channels() as usize != leg.channels {
        startup.fail(format!(
            "PipeWire negotiated {} Hz / {} ch for the {} leg; meetrs needs {} Hz / {} ch",
            info.rate(),
            info.channels(),
            leg.name,
            SAMPLE_RATE,
            leg.channels
        ));
    }
}

struct Running {
    quit: pw::channel::Sender<()>,
    handle: JoinHandle<()>,
}

static STATE: OnceLock<Mutex<Option<Running>>> = OnceLock::new();

/// Stop the loop thread and wait for it. Used by `stop()` and by a failed start.
fn teardown(running: Running) {
    let _ = running.quit.send(());
    let _ = running.handle.join();
}

pub fn start(ring_samples: usize) -> Result<(CaptureInfo, rtrb::Consumer<f32>)> {
    let state_cell = STATE.get_or_init(|| Mutex::new(None));
    let mut state_guard = state_cell.lock().unwrap();
    if state_guard.is_some() {
        bail!("capture already started; call stop() first");
    }

    let (out_prod, out_cons) = rtrb::RingBuffer::<f32>::new(ring_samples);
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let fail_tx = ready_tx.clone();
    let handle = std::thread::Builder::new()
        .name("meetrs-pipewire".into())
        .spawn(move || {
            // If the loop fails before it reports readiness, report that instead.
            if let Err(e) = run_loop(ready_tx, quit_rx, out_prod) {
                let _ = fail_tx.send(Err(format!("{e:#}")));
            }
        })
        .map_err(|e| anyhow!("spawning the PipeWire thread: {e}"))?;

    // Both the defaults wait and the stream-startup wait sit inside this window.
    let wait = STARTUP_TIMEOUT * 2;
    let running = Running {
        quit: quit_tx,
        handle,
    };
    match ready_rx.recv_timeout(wait) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => {
            teardown(running);
            bail!("starting PipeWire capture: {msg}");
        }
        Err(_) => {
            teardown(running);
            bail!(
                "starting PipeWire capture: streams did not reach the streaming state within {}s. \
                 The default sink or source may not be linkable; check `wpctl status`.",
                wait.as_secs()
            );
        }
    }

    let info = CaptureInfo {
        channels: OUT_CHANNELS as u16,
        sample_rate: SAMPLE_RATE,
        mic_channels: (0, 0),
        system_channels: (1, 2),
    };
    *state_guard = Some(running);
    Ok((info, out_cons))
}

pub fn stop() {
    let Some(cell) = STATE.get() else { return };
    let running = cell.lock().unwrap().take();
    if let Some(running) = running {
        teardown(running);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Test double for a leg: a plain queue.
    impl Leg for VecDeque<f32> {
        fn queued(&self) -> usize {
            self.len()
        }
        fn pop(&mut self) -> Option<f32> {
            self.pop_front()
        }
    }

    /// Test double for the output ring: a capacity-limited vector.
    struct VecOut {
        samples: Vec<f32>,
        cap: usize,
    }
    impl Out for VecOut {
        fn free(&self) -> usize {
            self.cap - self.samples.len()
        }
        fn push(&mut self, sample: f32) {
            self.samples.push(sample);
        }
    }

    fn out(cap: usize) -> VecOut {
        VecOut {
            samples: Vec::new(),
            cap,
        }
    }

    fn legs(mic: &[f32], sys: &[f32]) -> (VecDeque<f32>, VecDeque<f32>) {
        (mic.iter().copied().collect(), sys.iter().copied().collect())
    }

    const TOL: usize = 4;

    #[test]
    fn paired_frames_come_out_interleaved_in_channel_order() {
        // Mic frame 1 with system frame 1, then mic 2 with system 2.
        let (mic, sys) = legs(&[1.0, 2.0], &[10.0, 11.0, 20.0, 21.0]);
        let mut m = Mixer::new(mic, sys, out(64), TOL);
        assert_eq!(m.tick(), 0);
        assert_eq!(m.out.samples, vec![1.0, 10.0, 11.0, 2.0, 20.0, 21.0]);
    }

    #[test]
    fn a_partial_system_frame_waits_for_its_second_sample() {
        // One stereo sample has arrived, so no whole frame exists yet.
        let (mic, sys) = legs(&[1.0], &[5.0]);
        let mut m = Mixer::new(mic, sys, out(64), TOL);
        assert_eq!(m.tick(), 0);
        assert!(m.out.samples.is_empty());
        // The second sample arrives, and the frame completes.
        m.sys.push_back(6.0);
        m.tick();
        assert_eq!(m.out.samples, vec![1.0, 5.0, 6.0]);
    }

    #[test]
    fn a_short_lead_within_tolerance_is_not_padded() {
        // Mic is 3 frames ahead, under the 4-frame tolerance, and system has
        // nothing. Those frames wait.
        let (mic, sys) = legs(&[1.0, 2.0, 3.0], &[]);
        let mut m = Mixer::new(mic, sys, out(64), TOL);
        assert_eq!(m.tick(), 0);
        assert!(m.out.samples.is_empty());
        assert_eq!(m.mic.len(), 3);
    }

    #[test]
    fn a_stalled_system_leg_is_padded_once_mic_is_past_tolerance() {
        // Mic is 5 frames ahead of an empty system leg, past the 4-frame tolerance.
        // The whole backlog is emitted with zeros on the system channels.
        let (mic, sys) = legs(&[1.0, 2.0, 3.0, 4.0, 5.0], &[]);
        let mut m = Mixer::new(mic, sys, out(64), TOL);
        assert_eq!(m.tick(), 0);
        assert_eq!(
            m.out.samples,
            vec![
                1.0, 0.0, 0.0, 2.0, 0.0, 0.0, 3.0, 0.0, 0.0, 4.0, 0.0, 0.0, 5.0, 0.0, 0.0
            ]
        );
        assert!(m.mic.is_empty());
    }

    #[test]
    fn a_stalled_mic_leg_is_padded_once_system_is_past_tolerance() {
        // System runs 5 frames ahead of an empty mic leg.
        let (mic, sys) = legs(&[], &[1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0, 5.0, 5.0]);
        let mut m = Mixer::new(mic, sys, out(64), TOL);
        assert_eq!(m.tick(), 0);
        assert_eq!(
            m.out.samples,
            vec![
                0.0, 1.0, 1.0, 0.0, 2.0, 2.0, 0.0, 3.0, 3.0, 0.0, 4.0, 4.0, 0.0, 5.0, 5.0
            ]
        );
        assert!(m.sys.is_empty());
    }

    #[test]
    fn a_late_leg_within_tolerance_resumes_pairing_without_padding() {
        // Mic runs 3 frames ahead, then system catches up. Nothing is padded.
        let (mic, sys) = legs(&[1.0, 2.0, 3.0], &[]);
        let mut m = Mixer::new(mic, sys, out(64), TOL);
        m.tick();
        for s in [10.0, 10.0, 20.0, 20.0, 30.0, 30.0] {
            m.sys.push_back(s);
        }
        assert_eq!(m.tick(), 0);
        assert_eq!(
            m.out.samples,
            vec![1.0, 10.0, 10.0, 2.0, 20.0, 20.0, 3.0, 30.0, 30.0]
        );
    }

    #[test]
    fn a_full_output_ring_drops_whole_frames_and_counts_samples() {
        // Room for one frame plus two spare samples, so the second frame cannot
        // fit whole.
        let (mic, sys) = legs(&[1.0, 2.0], &[10.0, 10.0, 20.0, 20.0]);
        let mut m = Mixer::new(mic, sys, out(OUT_CHANNELS + 2), TOL);
        assert_eq!(m.tick(), OUT_CHANNELS as u64);
        assert_eq!(m.out.samples, vec![1.0, 10.0, 10.0]);
        // Both frames were consumed from the legs, and the second was discarded.
        assert!(m.mic.is_empty());
        assert!(m.sys.is_empty());
    }

    #[test]
    fn each_tick_reports_only_its_own_overruns() {
        // The output ring stays full across two ticks, so each one loses a frame.
        // Adding the two reports gives the total the caller accumulates.
        let (mic, sys) = legs(&[1.0, 2.0], &[10.0, 10.0, 20.0, 20.0]);
        let mut m = Mixer::new(mic, sys, out(OUT_CHANNELS), TOL);
        assert_eq!(m.tick(), OUT_CHANNELS as u64);
        m.mic.push_back(3.0);
        m.sys.extend([30.0, 30.0]);
        assert_eq!(m.tick(), OUT_CHANNELS as u64);
        assert_eq!(m.out.samples, vec![1.0, 10.0, 10.0]);
    }

    #[test]
    fn an_empty_tick_does_nothing() {
        let (mic, sys) = legs(&[], &[]);
        let mut m = Mixer::new(mic, sys, out(64), TOL);
        assert_eq!(m.tick(), 0);
        assert!(m.out.samples.is_empty());
    }

    #[test]
    fn node_name_reads_the_name_field_of_default_metadata() {
        assert_eq!(
            node_name(r#"{"name":"alsa_output.test"}"#).as_deref(),
            Some("alsa_output.test")
        );
        assert_eq!(node_name("not json"), None);
        assert_eq!(node_name(r#"{"other":1}"#), None);
    }

    /// A format pod that cannot be parsed must fail startup, not let the leg run
    /// unchecked. An all-zero pod is a None pod, not a format object.
    #[test]
    fn an_unreadable_format_pod_fails_startup() {
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let startup = Startup {
            ready: RefCell::new(Some(tx)),
            streaming: Cell::new(0),
        };
        let (producer, _consumer) = rtrb::RingBuffer::<f32>::new(4);
        let mut leg = LegData {
            name: "mic",
            channels: 1,
            producer,
        };
        let bytes = [0u8; 8];
        let pod = Pod::from_bytes(&bytes).expect("header-sized pod");

        check_format(
            &startup,
            &mut leg,
            spa::param::ParamType::Format.as_raw(),
            Some(pod),
        );

        let msg = rx
            .try_recv()
            .expect("startup should have been failed")
            .expect_err("startup should have been failed");
        assert!(msg.contains("mic leg"), "{msg}");
    }
}

//! Continuous capture of one Wayland output through
//! `ext-image-copy-capture-v1`. The compositor writes each frame into one of
//! a few buffers, which is lent to the encoder as is (`shm_pool.rs`).
//!
//! The buffers are GPU buffers (dmabufs) when the encoder takes them
//! ([`CaptureOptions::gpu`], the VA-API encoder): they are allocated with
//! gbm on the render node the compositor announced, in a format and modifier
//! it accepts, and become `wl_buffer`s through `zwp_linux_dmabuf_v1`. The
//! encoder imports them without the CPU touching a pixel. Otherwise, or when
//! anything on that path fails, they are `wl_shm` buffers, mapped once and
//! read by the colour conversion. A capture that falls back to shared memory
//! stays there for the rest of its life.

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use super::dispatch_with_timeout;
use super::shm_pool::{Lease, Mapping, Recycler};
use crate::backend::capture::CapturePath;
use crate::backend::{
    CaptureEvent, CaptureOptions, Dmabuf, Frame, FrameBuf, FrameSink, GpuFrames, PixelFormat,
    Pixels,
};
use crate::dmabuf::{GbmBuffer, GbmDevice, drm_fourcc, importable_modifier, modifier_name};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, event_created_child};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1,
    ext_image_copy_capture_session_v1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: wl_shm::Format,
    /// Compositor presentation time of this frame, if reported.
    pub presentation_ns: Option<u128>,
    /// When the compositor said the frame was ready, before we copy it out.
    pub ready_at: Instant,
}

/// A captured frame as [`capture`] hands it out.
pub enum Captured {
    /// In a `wl_shm` buffer, lent out until the lease is dropped.
    Shm { info: FrameInfo, lease: Lease },
    /// In a GPU buffer, lent out until every clone of `buf.hold` is dropped.
    Dmabuf {
        buf: Dmabuf,
        presentation_ns: Option<u128>,
        ready_at: Instant,
    },
}

/// Buffers the compositor may be writing, lent out or free at once: one
/// attached, one waiting in the encoder's latest-wins slot, one being
/// encoded and the last one kept for an IDR replay on a static screen.
pub(super) const MAX_BUFFERS: usize = 4;

/// One mapped `wl_shm` buffer (shared with `screencopy.rs`).
pub(super) struct ShmBuffer {
    pub(super) map: Arc<Mapping>,
    pool: wl_shm_pool::WlShmPool,
    pub(super) buffer: wl_buffer::WlBuffer,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stride: u32,
    pub(super) format: wl_shm::Format,
}

impl ShmBuffer {
    pub(super) fn new<D>(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<D>,
        width: u32,
        height: u32,
        stride: u32,
        format: wl_shm::Format,
    ) -> Result<Self>
    where
        D: Dispatch<wl_shm_pool::WlShmPool, ()> + Dispatch<wl_buffer::WlBuffer, ()> + 'static,
    {
        let size = (stride * height) as u64;
        let path = format!(
            "/dev/shm/od-capture-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        );
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let _ = std::fs::remove_file(&path);
        file.set_len(size)?;
        let map = Arc::new(Mapping::new(&file, size as usize)?);
        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        Ok(Self {
            map,
            pool,
            buffer,
            width,
            height,
            stride,
            format,
        })
    }
}

impl Drop for ShmBuffer {
    /// A lease may still read the mapping; it holds its own reference.
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// Source of [`Dmabuf::id`]: unique per allocated buffer for the process.
static NEXT_GPU_BUFFER_ID: AtomicU64 = AtomicU64::new(1);

/// One GPU buffer the compositor renders into.
struct GpuBuffer {
    buffer: wl_buffer::WlBuffer,
    /// The dmabufs; the encoder gets clones of the fds, which keep the
    /// memory alive after this is dropped.
    gbm: GbmBuffer,
    id: u64,
}

impl Drop for GpuBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

enum Buffer {
    Shm(ShmBuffer),
    Gpu(GpuBuffer),
}

impl Buffer {
    fn wl(&self) -> &wl_buffer::WlBuffer {
        match self {
            Buffer::Shm(b) => &b.buffer,
            Buffer::Gpu(b) => &b.buffer,
        }
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Buffer::Shm(b) => (b.width, b.height),
            Buffer::Gpu(b) => (b.gbm.width, b.gbm.height),
        }
    }

    fn is_gpu(&self) -> bool {
        matches!(self, Buffer::Gpu(_))
    }
}

/// The buffer constraints of one batch of session events (up to `done`).
#[derive(Debug, Default, Clone, PartialEq)]
struct Constraints {
    width: u32,
    height: u32,
    shm_formats: Vec<wl_shm::Format>,
    /// `dev_t` of the GPU the compositor wants dmabufs from.
    dmabuf_device: Option<u64>,
    /// DRM fourcc and the modifiers accepted with it.
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
}

impl Constraints {
    fn add_dmabuf_format(&mut self, fourcc: u32, modifiers: Vec<u64>) {
        match self.dmabuf_formats.iter_mut().find(|(f, _)| *f == fourcc) {
            Some((_, m)) => m.extend(modifiers),
            None => self.dmabuf_formats.push((fourcc, modifiers)),
        }
    }
}

#[derive(Default)]
struct SessionInfo {
    /// The last complete batch of constraints.
    current: Constraints,
    /// A batch being received: the compositor may send the constraints
    /// again (followed by `done`), and a new batch replaces the old one.
    building: Option<Constraints>,
    done: bool,
    stopped: bool,
}

impl SessionInfo {
    /// The batch a constraint event goes into; the first event after a
    /// `done` starts an empty one.
    fn batch(&mut self) -> &mut Constraints {
        self.building.get_or_insert_with(Constraints::default)
    }

    fn finish_batch(&mut self) {
        if let Some(b) = self.building.take() {
            self.current = b;
        }
        self.done = true;
    }
}

/// What the async `zwp_linux_buffer_params_v1.create` came back with.
enum ParamsOutcome {
    Created(wl_buffer::WlBuffer),
    Failed,
}

struct State {
    shm: Option<wl_shm::WlShm>,
    outputs: HashMap<u32, (wl_output::WlOutput, String)>,
    source_mgr:
        Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    capture_mgr: Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,
    /// Bound only when GPU buffers are asked for; with its version.
    bind_dmabuf: bool,
    linux_dmabuf: Option<(zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, u32)>,
    session: SessionInfo,
    /// Events of sessions and frames destroyed before their events were
    /// dispatched carry an older serial and are ignored.
    session_serial: u64,
    buffers: Vec<Buffer>,
    /// Index of the buffer attached to the frame in flight.
    attached: Option<usize>,
    frame: Option<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1>,
    frame_serial: u64,
    frame_presentation: Option<u128>,
    ready: bool,
    failed: Option<WEnum<ext_image_copy_capture_frame_v1::FailureReason>>,
    frames_ready: u64,
    params: Option<ParamsOutcome>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_output" if version >= 4 => {
                    let o: wl_output::WlOutput = registry.bind(name, 4, qh, name);
                    state.outputs.insert(name, (o, String::new()));
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.source_mgr = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.capture_mgr = Some(registry.bind(name, 1, qh, ()))
                }
                "zwp_linux_dmabuf_v1" if state.bind_dmabuf => {
                    let v = version.min(4);
                    state.linux_dmabuf = Some((registry.bind(name, v, qh, ()), v));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(o) = state.outputs.get_mut(global) {
                o.1 = name;
            }
        }
    }
}

impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, u64> for State {
    fn event(
        state: &mut Self,
        _: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        serial: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        if *serial != state.session_serial {
            return;
        }
        let s = &mut state.session;
        match event {
            Event::BufferSize { width, height } => {
                let b = s.batch();
                b.width = width;
                b.height = height;
            }
            Event::ShmFormat {
                format: WEnum::Value(f),
            } => s.batch().shm_formats.push(f),
            Event::DmabufDevice { device } => {
                s.batch().dmabuf_device = crate::dmabuf::dev_from_bytes(&device)
            }
            Event::DmabufFormat { format, modifiers } => s
                .batch()
                .add_dmabuf_format(format, modifiers_from_bytes(&modifiers)),
            Event::Done => s.finish_batch(),
            Event::Stopped => s.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, u64> for State {
    fn event(
        state: &mut Self,
        _: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        serial: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        if *serial != state.frame_serial {
            return;
        }
        match event {
            Event::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let secs = ((tv_sec_hi as u128) << 32) | tv_sec_lo as u128;
                state.frame_presentation = Some(secs * 1_000_000_000 + tv_nsec as u128);
            }
            Event::Ready => {
                state.ready = true;
                state.frames_ready += 1;
            }
            Event::Failed { reason } => state.failed = Some(reason),
            _ => {}
        }
    }
}

impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_linux_buffer_params_v1::Event;
        match event {
            Event::Created { buffer } => state.params = Some(ParamsOutcome::Created(buffer)),
            Event::Failed => state.params = Some(ParamsOutcome::Failed),
            _ => {}
        }
    }

    event_created_child!(State, zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, ()),
    ]);
}

macro_rules! noop_dispatch {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
noop_dispatch!(
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1
);

/// A capture of one output on its own thread. [`stop`](Self::stop) (or drop)
/// joins the thread, so once it returns no frame is in flight.
pub struct CaptureThread {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl CaptureThread {
    pub fn spawn(
        output: &str,
        opts: CaptureOptions,
        path: CapturePath,
        mut sink: FrameSink,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let output = output.to_string();
        let handle = std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                // A panic in here (or in the sink) must still tell the session
                // that capture is gone, so it rebuilds instead of idling.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut warned_format = false;
                    let forever = Duration::from_secs(u64::MAX / 4);
                    match path {
                        CapturePath::Ext => {
                            capture(Some(&output), &opts, forever, thread_stop.clone(), |c| {
                                let frame = match c {
                                    Captured::Shm { info, lease } => {
                                        cpu_frame(&info, lease, &mut warned_format)
                                    }
                                    Captured::Dmabuf {
                                        buf,
                                        presentation_ns,
                                        ready_at,
                                    } => Frame {
                                        captured_at: ready_at,
                                        presentation_ns,
                                        buf: FrameBuf::Dmabuf(buf),
                                    },
                                };
                                sink(CaptureEvent::Frame(frame));
                            })
                        }
                        CapturePath::Wlr => {
                            if opts.gpu.is_some() {
                                debug!("wlr-screencopy captures into shared memory only");
                            }
                            super::screencopy::capture_wlr(
                                &output,
                                opts.cursor,
                                forever,
                                thread_stop.clone(),
                                |info, lease| {
                                    let frame = cpu_frame(info, lease, &mut warned_format);
                                    sink(CaptureEvent::Frame(frame));
                                },
                            )
                        }
                    }
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("capture thread panicked")));
                if thread_stop.load(Ordering::Relaxed) {
                    if let Err(e) = r {
                        debug!("capture ended after stop: {e:#}");
                    }
                    return;
                }
                let why = match r {
                    Ok(_) => format!("capture of {output} stopped by the compositor"),
                    Err(e) => format!("capture of {output} failed: {e:#}"),
                };
                warn!("{why}");
                sink(CaptureEvent::Ended(why));
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    /// Stop and join. Idempotent.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for CaptureThread {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A shared-memory frame for the encoder; warns once about a format the
/// encoders do not convert from.
fn cpu_frame(info: &FrameInfo, lease: Lease, warned_format: &mut bool) -> Frame {
    if !*warned_format && !is_known_format(info.format) {
        warn!(
            "compositor captures as {:?}; frames are read as XRGB8888, colours may be off",
            info.format
        );
        *warned_format = true;
    }
    Frame {
        captured_at: info.ready_at,
        presentation_ns: info.presentation_ns,
        buf: FrameBuf::Cpu {
            pixels: Pixels::Leased(Box::new(lease)),
            width: info.width,
            height: info.height,
            stride: info.stride,
            format: pixel_format(info.format),
        },
    }
}

/// The formats the encoders convert from; anything else is read as XRGB8888
/// by [`pixel_format`], and the capture thread warns once.
fn is_known_format(f: wl_shm::Format) -> bool {
    matches!(
        f,
        wl_shm::Format::Xrgb8888
            | wl_shm::Format::Argb8888
            | wl_shm::Format::Xbgr8888
            | wl_shm::Format::Abgr8888
    )
}

pub(super) fn pixel_format(f: wl_shm::Format) -> PixelFormat {
    match f {
        wl_shm::Format::Argb8888 => PixelFormat::Argb8888,
        wl_shm::Format::Xbgr8888 => PixelFormat::Xbgr8888,
        wl_shm::Format::Abgr8888 => PixelFormat::Abgr8888,
        _ => PixelFormat::Xrgb8888,
    }
}

/// The GPU-buffer side of one capture: whether to use it, the gbm device,
/// and the failures that make it give up.
struct Gpu {
    frames: GpuFrames,
    /// Set when GPU buffers failed here; this capture stays on shared
    /// memory (the encoder's [`GpuFrames`] is left alone).
    unusable: bool,
    device: Option<(u64, PathBuf, GbmDevice)>,
    failures: GpuFailures,
    /// The last buffer kind logged, so it is logged once per change.
    logged: Option<String>,
}

impl Gpu {
    fn active(&self) -> bool {
        !self.unusable && self.frames.wanted()
    }

    fn log_mode(&mut self, line: String) {
        if self.logged.as_ref() != Some(&line) {
            info!("{line}");
            self.logged = Some(line);
        }
    }
}

/// How a GPU buffer allocation went.
enum GpuAlloc {
    Done(GpuBuffer),
    /// The session's constraints or the compositor rule it out for now.
    NotOffered(String),
    /// It was offered but does not work here: stop trying for this capture.
    Broken(String),
    /// Stopped (by the caller or the compositor) while waiting.
    Interrupted,
}

/// Capture `output_name` (or the first output) for `duration`, calling
/// `on_frame` with each ready frame, lent out from the buffer it was
/// captured into. The buffer is reused once the frame is dropped; holding
/// [`MAX_BUFFERS`] frames pauses capture. GPU buffers are used when
/// `opts.gpu` asks for them and the compositor and gbm can provide them.
/// Returns the number of frames delivered.
pub fn capture(
    output_name: Option<&str>,
    opts: &CaptureOptions,
    duration: Duration,
    stop: Arc<AtomicBool>,
    mut on_frame: impl FnMut(Captured),
) -> Result<u64> {
    let conn = Connection::connect_to_env().context("connecting to Wayland")?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut st = State {
        shm: None,
        outputs: HashMap::new(),
        source_mgr: None,
        capture_mgr: None,
        bind_dmabuf: opts.gpu.is_some(),
        linux_dmabuf: None,
        session: SessionInfo::default(),
        session_serial: 0,
        buffers: Vec::new(),
        attached: None,
        frame: None,
        frame_serial: 0,
        frame_presentation: None,
        ready: false,
        failed: None,
        frames_ready: 0,
        params: None,
    };
    queue.roundtrip(&mut st)?;
    queue.roundtrip(&mut st)?;
    let shm = st.shm.clone().context("compositor has no wl_shm")?;
    let source_mgr = st
        .source_mgr
        .clone()
        .context("compositor lacks ext_output_image_capture_source_manager_v1")?;
    let capture_mgr = st
        .capture_mgr
        .clone()
        .context("compositor lacks ext_image_copy_capture_manager_v1")?;
    let (output, name) = match output_name {
        Some(n) => st
            .outputs
            .values()
            .find(|(_, name)| name == n)
            .cloned()
            .with_context(|| {
                format!(
                    "no output {n}; have {:?}",
                    st.outputs
                        .values()
                        .map(|(_, n)| n.clone())
                        .collect::<Vec<_>>()
                )
            })?,
        None => st.outputs.values().next().cloned().context("no outputs")?,
    };
    info!("capturing output {name}");
    let mut gpu = opts.gpu.clone().map(|frames| Gpu {
        frames,
        unusable: false,
        device: None,
        failures: GpuFailures::default(),
        logged: None,
    });

    let source = source_mgr.create_source(&output, &qh, ());
    let options = if opts.cursor {
        ext_image_copy_capture_manager_v1::Options::PaintCursors
    } else {
        ext_image_copy_capture_manager_v1::Options::empty()
    };
    let new_session = |conn: &Connection,
                       queue: &mut wayland_client::EventQueue<State>,
                       st: &mut State|
     -> Result<
        Option<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1>,
    > {
        st.session = SessionInfo::default();
        st.session_serial += 1;
        let session = capture_mgr.create_session(&source, options, &qh, st.session_serial);
        // Bounded and watching `stop`, so a joiner is never stuck behind a
        // compositor that does not describe the session.
        let setup_deadline = Instant::now() + Duration::from_secs(5);
        while !st.session.done {
            if stop.load(Ordering::Relaxed) {
                session.destroy();
                return Ok(None);
            }
            if Instant::now() >= setup_deadline {
                bail!("the compositor did not describe the capture session within 5 s");
            }
            dispatch_with_timeout(conn, queue, st, Duration::from_millis(100))?;
            if st.session.stopped {
                bail!("capture session stopped by the compositor");
            }
        }
        let c = &st.session.current;
        info!(
            "session: {}x{} shm formats {:?}",
            c.width, c.height, c.shm_formats
        );
        if st.bind_dmabuf {
            debug!(
                "session: dmabuf device {:?}, formats {}",
                c.dmabuf_device,
                describe_dmabuf_formats(&c.dmabuf_formats)
            );
        }
        Ok(Some(session))
    };
    let Some(mut session) = new_session(&conn, &mut queue, &mut st)? else {
        source.destroy();
        return Ok(0);
    };

    let recycler = Recycler::default();
    let start = Instant::now();
    let mut first_frame: Option<Duration> = None;
    let mut last_ready = start;
    let mut intervals_ms: Vec<f64> = Vec::new();
    // GPU buffers are given up (declined by the encoder, or failing here)
    // while some are in use: start over on shared memory with a new session,
    // whose first frame comes without waiting for damage.
    macro_rules! restart_on_shm {
        () => {{
            if let Some(f) = st.frame.take() {
                f.destroy();
            }
            st.attached = None;
            recycler.reset();
            st.buffers.clear();
            session.destroy();
            match new_session(&conn, &mut queue, &mut st)? {
                Some(s) => session = s,
                None => break,
            }
            continue;
        }};
    }
    while start.elapsed() < duration && !st.session.stopped && !stop.load(Ordering::Relaxed) {
        let on_gpu = st.buffers.first().map(Buffer::is_gpu);
        if on_gpu == Some(true) {
            if let Some(g) = gpu.as_mut().filter(|g| !g.frames.wanted()) {
                g.log_mode("capture: shared memory (the encoder declined GPU buffers)".into());
                restart_on_shm!();
            }
        }
        // Reallocate every buffer when the session's size changes.
        let (width, height) = (st.session.current.width, st.session.current.height);
        if st
            .buffers
            .first()
            .is_some_and(|b| b.size() != (width, height))
        {
            recycler.reset();
            st.buffers.clear();
        }
        if st.frame.is_none() {
            let index = match recycler.take() {
                Some(i) => i,
                None if st.buffers.len() < MAX_BUFFERS => {
                    let on_gpu = st.buffers.first().map(Buffer::is_gpu);
                    let mut buffer = None;
                    if let Some(g) = gpu.as_mut().filter(|g| g.active() && on_gpu != Some(false)) {
                        match alloc_gpu(&conn, &mut queue, &mut st, g, &qh, &stop)? {
                            GpuAlloc::Done(b) => {
                                let line = format!(
                                    "capture: GPU buffers {} {} on {}",
                                    fourcc_name(drm_fourcc(b.gbm.format)),
                                    modifier_name(b.gbm.modifier),
                                    g.device
                                        .as_ref()
                                        .map_or("?".into(), |d| d.1.display().to_string())
                                );
                                g.log_mode(line);
                                if on_gpu.is_none() {
                                    g.failures.new_buffers();
                                }
                                buffer = Some(Buffer::Gpu(b));
                            }
                            GpuAlloc::NotOffered(why) => {
                                g.log_mode(format!("capture: shared memory ({why})"));
                                if on_gpu == Some(true) {
                                    // The constraints changed under the GPU
                                    // buffers: reallocate them all.
                                    recycler.reset();
                                    st.buffers.clear();
                                    continue;
                                }
                            }
                            GpuAlloc::Broken(why) => {
                                warn!(
                                    "capture: GPU buffers do not work ({why}); shared memory from now on"
                                );
                                g.unusable = true;
                                g.logged = Some(format!("capture: shared memory ({why})"));
                                if on_gpu == Some(true) {
                                    restart_on_shm!();
                                }
                            }
                            GpuAlloc::Interrupted => continue,
                        }
                    }
                    let buffer = match buffer {
                        Some(b) => b,
                        None => {
                            let format = pick_format(&st.session.current.shm_formats)?;
                            Buffer::Shm(ShmBuffer::new(
                                &shm,
                                &qh,
                                width,
                                height,
                                width * 4,
                                format,
                            )?)
                        }
                    };
                    st.buffers.push(buffer);
                    debug!(
                        "allocated capture buffer {} at {width}x{height} ({})",
                        st.buffers.len(),
                        if st.buffers[st.buffers.len() - 1].is_gpu() {
                            "GPU"
                        } else {
                            "shared memory"
                        }
                    );
                    st.buffers.len() - 1
                }
                None => {
                    // Every buffer is lent out: the encoder is behind.
                    recycler.wait(Duration::from_millis(250));
                    continue;
                }
            };
            // Hyprland 0.56.2 segfaults on a create_frame for an output whose
            // monitor is already gone
            // (research/linux/hyprland-screenshare-transform.md): read what
            // the compositor sent without blocking, and never create a frame
            // on a stopped session.
            dispatch_with_timeout(&conn, &mut queue, &mut st, Duration::ZERO)?;
            if !may_create_frame(&st.session, stop.load(Ordering::Relaxed)) {
                recycler.release(index);
                break;
            }
            if st.buffers[index].size() != (st.session.current.width, st.session.current.height) {
                // Resized meanwhile: the top of the loop reallocates.
                recycler.release(index);
                continue;
            }
            st.frame_serial += 1;
            let frame = session.create_frame(&qh, st.frame_serial);
            let b = &st.buffers[index];
            let (w, h) = b.size();
            frame.attach_buffer(b.wl());
            frame.damage_buffer(0, 0, w as i32, h as i32);
            frame.capture();
            st.frame = Some(frame);
            st.attached = Some(index);
            st.ready = false;
            st.failed = None;
            st.frame_presentation = None;
        }
        dispatch_with_timeout(&conn, &mut queue, &mut st, Duration::from_millis(250))?;
        if st.ready {
            let now = Instant::now();
            first_frame.get_or_insert(now - start);
            intervals_ms.push((now - last_ready).as_secs_f64() * 1000.0);
            last_ready = now;
            let index = st.attached.take().expect("a frame in flight has a buffer");
            if let Some(f) = st.frame.take() {
                f.destroy();
            }
            match &st.buffers[index] {
                Buffer::Shm(b) => {
                    let lease =
                        recycler.lease(index, b.map.clone(), (b.stride * b.height) as usize);
                    let info = FrameInfo {
                        width: b.width,
                        height: b.height,
                        stride: b.stride,
                        format: b.format,
                        presentation_ns: st.frame_presentation,
                        ready_at: now,
                    };
                    on_frame(Captured::Shm { info, lease });
                }
                Buffer::Gpu(b) => {
                    if let Some(g) = gpu.as_mut() {
                        g.failures.ready();
                    }
                    let buf = Dmabuf {
                        width: b.gbm.width,
                        height: b.gbm.height,
                        format: b.gbm.format,
                        modifier: b.gbm.modifier,
                        planes: b.gbm.planes.clone(),
                        id: b.id,
                        hold: Arc::new(recycler.token(index)),
                    };
                    on_frame(Captured::Dmabuf {
                        buf,
                        presentation_ns: st.frame_presentation,
                        ready_at: now,
                    });
                }
            }
        } else if let Some(reason) = st.failed.take() {
            warn!("frame failed: {reason:?}");
            if let Some(f) = st.frame.take() {
                f.destroy();
            }
            let on_gpu = st
                .attached
                .and_then(|i| st.buffers.get(i))
                .is_some_and(Buffer::is_gpu);
            let after = after_failure(reason);
            if let (true, Some(g)) = (on_gpu, gpu.as_mut()) {
                if g.failures.failed(&after) {
                    warn!(
                        "capture: {GPU_FAILURES_BEFORE_SHM} frames in a row failed in GPU buffers; shared memory from now on"
                    );
                    g.unusable = true;
                    g.logged = Some("capture: shared memory (GPU frames fail)".into());
                    restart_on_shm!();
                }
            }
            match after {
                AfterFailure::Reallocate => {
                    recycler.reset();
                    st.buffers.clear();
                    st.attached = None;
                }
                AfterFailure::Retry => {
                    if let Some(index) = st.attached.take() {
                        recycler.release(index);
                    }
                }
                AfterFailure::EndSession => {
                    // The session is done even if its own `stopped` event has
                    // not been dispatched yet.
                    st.session.stopped = true;
                }
            }
        }
    }
    if let Some(f) = st.frame.take() {
        f.destroy();
    }
    session.destroy();
    source.destroy();
    let _ = queue.roundtrip(&mut st);
    let n = intervals_ms.len();
    if n > 1 {
        intervals_ms.sort_by(f64::total_cmp);
        info!(
            "{n} frames in {:.1}s: first after {:.0} ms; interval p50 {:.1} ms, p95 {:.1} ms, min {:.1} ms",
            start.elapsed().as_secs_f64(),
            first_frame.map_or(0.0, |d| d.as_secs_f64() * 1000.0),
            intervals_ms[n / 2],
            intervals_ms[(n as f64 * 0.95) as usize],
            intervals_ms[0]
        );
    }
    Ok(st.frames_ready)
}

/// Allocates one GPU buffer for the session's current constraints and makes
/// a `wl_buffer` of it. Errors only when the connection fails.
fn alloc_gpu(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    st: &mut State,
    gpu: &mut Gpu,
    qh: &QueueHandle<State>,
    stop: &AtomicBool,
) -> Result<GpuAlloc> {
    let choice = match pick_dmabuf(&st.session.current) {
        Ok(c) => c,
        Err(why) => return Ok(GpuAlloc::NotOffered(why)),
    };
    let Some((linux_dmabuf, version)) = st.linux_dmabuf.clone() else {
        return Ok(GpuAlloc::NotOffered(
            "the compositor lacks zwp_linux_dmabuf_v1".into(),
        ));
    };
    if version < 3 {
        return Ok(GpuAlloc::NotOffered(format!(
            "zwp_linux_dmabuf_v1 version {version} has no modifiers"
        )));
    }
    if gpu.device.as_ref().map(|d| d.0) != Some(choice.device) {
        let opened = crate::dmabuf::render_node_for(choice.device).and_then(|node| {
            let dev = GbmDevice::open(&node)?;
            Ok((node, dev))
        });
        match opened {
            Ok((node, dev)) => gpu.device = Some((choice.device, node, dev)),
            Err(e) => return Ok(GpuAlloc::Broken(format!("{e:#}"))),
        }
    }
    let (_, node, dev) = gpu.device.as_ref().expect("opened above");
    let c = &st.session.current;
    let gbm = match dev.alloc(c.width, c.height, choice.format, &choice.modifiers) {
        Ok(b) => b,
        Err(e) => return Ok(GpuAlloc::Broken(format!("{e:#} on {}", node.display()))),
    };
    let params = linux_dmabuf.create_params(qh, ());
    for (i, p) in gbm.planes.iter().enumerate() {
        params.add(
            p.fd.as_fd(),
            i as u32,
            p.offset,
            p.stride,
            (gbm.modifier >> 32) as u32,
            gbm.modifier as u32,
        );
    }
    // The asynchronous create: a buffer the compositor cannot import comes
    // back as `failed`, where create_immed may be a protocol error that
    // ends the connection.
    st.params = None;
    params.create(
        gbm.width as i32,
        gbm.height as i32,
        drm_fourcc(gbm.format),
        zwp_linux_buffer_params_v1::Flags::empty(),
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    let outcome = loop {
        if let Some(o) = st.params.take() {
            break Some(o);
        }
        if stop.load(Ordering::Relaxed) || st.session.stopped {
            params.destroy();
            return Ok(GpuAlloc::Interrupted);
        }
        if Instant::now() >= deadline {
            break None;
        }
        dispatch_with_timeout(conn, queue, st, Duration::from_millis(50))?;
    };
    params.destroy();
    let modifier = modifier_name(gbm.modifier);
    match outcome {
        Some(ParamsOutcome::Created(buffer)) => Ok(GpuAlloc::Done(GpuBuffer {
            buffer,
            gbm,
            id: NEXT_GPU_BUFFER_ID.fetch_add(1, Ordering::Relaxed),
        })),
        Some(ParamsOutcome::Failed) => Ok(GpuAlloc::Broken(format!(
            "the compositor refused a {} {modifier} dmabuf",
            fourcc_name(drm_fourcc(gbm.format))
        ))),
        None => Ok(GpuAlloc::Broken(format!(
            "the compositor did not answer for a {modifier} dmabuf within 1 s"
        ))),
    }
}

/// Whether the loop may send another `create_frame`: never on a stopped
/// session or after a stop request. One frame is in flight at a time (the
/// loop only creates one when the previous is done).
fn may_create_frame(session: &SessionInfo, stop_requested: bool) -> bool {
    !session.stopped && !stop_requested
}

#[derive(Debug, PartialEq, Eq)]
enum AfterFailure {
    /// Same buffers, next frame.
    Retry,
    /// The buffers no longer fit the session's constraints.
    Reallocate,
    /// The session is over (output gone): no further frame on it.
    EndSession,
}

fn after_failure(reason: WEnum<ext_image_copy_capture_frame_v1::FailureReason>) -> AfterFailure {
    use ext_image_copy_capture_frame_v1::FailureReason;
    match reason {
        WEnum::Value(FailureReason::BufferConstraints) => AfterFailure::Reallocate,
        WEnum::Value(FailureReason::Stopped) => AfterFailure::EndSession,
        _ => AfterFailure::Retry,
    }
}

/// Frames failing in a row in new GPU buffers before capture gives up on them.
const GPU_FAILURES_BEFORE_SHM: u32 = 3;

/// Counts frames failing in GPU buffers that never produced a frame: the
/// compositor accepted them but cannot render into them.
#[derive(Debug, Default)]
struct GpuFailures {
    failures: u32,
    /// A frame was ready in the current buffers: they work.
    proven: bool,
}

impl GpuFailures {
    fn new_buffers(&mut self) {
        *self = Self::default();
    }

    fn ready(&mut self) {
        self.proven = true;
        self.failures = 0;
    }

    /// Whether to give up on GPU buffers after a frame failed in one.
    /// Failures handled otherwise (new constraints, session over) do not
    /// count.
    fn failed(&mut self, after: &AfterFailure) -> bool {
        match after {
            AfterFailure::Retry if !self.proven => {
                self.failures += 1;
                self.failures >= GPU_FAILURES_BEFORE_SHM
            }
            AfterFailure::Reallocate => {
                self.new_buffers();
                false
            }
            _ => false,
        }
    }
}

fn pick_format(formats: &[wl_shm::Format]) -> Result<wl_shm::Format> {
    for pref in [
        wl_shm::Format::Xrgb8888,
        wl_shm::Format::Argb8888,
        wl_shm::Format::Xbgr8888,
        wl_shm::Format::Abgr8888,
    ] {
        if formats.contains(&pref) {
            return Ok(pref);
        }
    }
    formats
        .first()
        .copied()
        .context("session offered no shm formats")
}

/// The format, modifiers and device to allocate GPU buffers with.
#[derive(Debug, PartialEq)]
struct DmabufChoice {
    device: u64,
    format: PixelFormat,
    /// For gbm: the compositor's modifiers the encoder can import.
    modifiers: Vec<u64>,
}

/// Picks GPU buffers from the session's constraints: the device, the first
/// of the formats the encoders read (in the shm order) with a modifier the
/// encoder imports. `Err` says why there is none.
fn pick_dmabuf(c: &Constraints) -> Result<DmabufChoice, String> {
    let Some(device) = c.dmabuf_device else {
        return Err("the compositor offered no dmabuf device".into());
    };
    if c.dmabuf_formats.is_empty() {
        return Err("the compositor offered no dmabuf formats".into());
    }
    let mut known = false;
    for format in [
        PixelFormat::Xrgb8888,
        PixelFormat::Argb8888,
        PixelFormat::Xbgr8888,
        PixelFormat::Abgr8888,
    ] {
        let fourcc = drm_fourcc(format);
        let Some((_, offered)) = c.dmabuf_formats.iter().find(|(f, _)| *f == fourcc) else {
            continue;
        };
        known = true;
        let modifiers = importable_modifiers(offered);
        if !modifiers.is_empty() {
            return Ok(DmabufChoice {
                device,
                format,
                modifiers,
            });
        }
    }
    if known {
        Err("no modifier the encoder imports".into())
    } else {
        Err(format!(
            "no dmabuf format the encoder reads among {}",
            describe_dmabuf_formats(&c.dmabuf_formats)
        ))
    }
}

/// The offered modifiers the VA-API encoder imports correctly
/// ([`importable_modifier`]), without duplicates. `INVALID` (the driver's
/// implicit choice) is dropped, as it may be a compressed layout.
fn importable_modifiers(offered: &[u64]) -> Vec<u64> {
    // Development hook (`od-sender dev capture-test --gpu-linear`): only
    // LINEAR buffers, which the CPU can read back to compare pixels.
    let linear_only = std::env::var_os("OD_DEV_LINEAR_CAPTURE").is_some_and(|v| v == "1");
    let mut out: Vec<u64> = Vec::new();
    for &m in offered {
        if linear_only && m != 0 {
            continue;
        }
        if importable_modifier(m) && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

/// A `wl_array` of modifiers: u64 in native (little) endianness.
fn modifiers_from_bytes(bytes: &[u8]) -> Vec<u64> {
    bytes
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")))
        .collect()
}

/// `XR24` and the like.
fn fourcc_name(fourcc: u32) -> String {
    fourcc
        .to_le_bytes()
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect()
}

fn describe_dmabuf_formats(formats: &[(u32, Vec<u64>)]) -> String {
    formats
        .iter()
        .map(|(f, mods)| {
            format!(
                "{} [{}]",
                fourcc_name(*f),
                mods.iter()
                    .map(|&m| modifier_name(m))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dmabuf::{DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR};
    use ext_image_copy_capture_frame_v1::FailureReason;

    const X_TILED: u64 = 0x0100_0000_0000_0001;
    const TILED_4: u64 = 0x0100_0000_0000_0009;
    const LNL_CCS: u64 = 0x0100_0000_0000_0010;
    const AMD_TILED: u64 = 0x0200_0000_0000_1234;

    fn xr24() -> u32 {
        drm_fourcc(PixelFormat::Xrgb8888)
    }

    fn ar24() -> u32 {
        drm_fourcc(PixelFormat::Argb8888)
    }

    /// What Hyprland 0.57 sends for eDP-1 on Lunar Lake.
    fn hyprland() -> Constraints {
        let mods = vec![
            DRM_FORMAT_MOD_LINEAR,
            X_TILED,
            TILED_4,
            LNL_CCS,
            DRM_FORMAT_MOD_INVALID,
        ];
        let mut c = Constraints {
            width: 2880,
            height: 1800,
            shm_formats: vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
            dmabuf_device: Some((226 << 8) | 128),
            ..Default::default()
        };
        c.add_dmabuf_format(ar24(), mods.clone());
        c.add_dmabuf_format(xr24(), mods);
        c
    }

    #[test]
    fn no_frame_on_a_stopped_session_or_after_stop() {
        let mut s = SessionInfo::default();
        assert!(may_create_frame(&s, false));
        assert!(!may_create_frame(&s, true));
        s.stopped = true;
        assert!(!may_create_frame(&s, false));
    }

    #[test]
    fn a_stopped_frame_ends_the_session() {
        assert_eq!(
            after_failure(WEnum::Value(FailureReason::Stopped)),
            AfterFailure::EndSession
        );
        assert_eq!(
            after_failure(WEnum::Value(FailureReason::BufferConstraints)),
            AfterFailure::Reallocate
        );
        assert_eq!(
            after_failure(WEnum::Value(FailureReason::Unknown)),
            AfterFailure::Retry
        );
        assert_eq!(after_failure(WEnum::Unknown(99)), AfterFailure::Retry);
    }

    #[test]
    fn xrgb_with_uncompressed_modifiers_is_chosen() {
        let choice = pick_dmabuf(&hyprland()).unwrap();
        assert_eq!(choice.device, (226 << 8) | 128);
        assert_eq!(choice.format, PixelFormat::Xrgb8888);
        assert_eq!(
            choice.modifiers,
            vec![DRM_FORMAT_MOD_LINEAR, X_TILED, TILED_4]
        );
    }

    #[test]
    fn compressed_implicit_and_foreign_modifiers_are_dropped() {
        assert_eq!(
            importable_modifiers(&[LNL_CCS, DRM_FORMAT_MOD_INVALID, AMD_TILED]),
            Vec::<u64>::new()
        );
        assert_eq!(
            importable_modifiers(&[AMD_TILED, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_MOD_LINEAR]),
            vec![DRM_FORMAT_MOD_LINEAR]
        );
        // Y_TILED_GEN12_RC_CCS: compressed.
        assert!(importable_modifiers(&[0x0100_0000_0000_0006]).is_empty());
    }

    #[test]
    fn no_gpu_buffers_without_a_device_or_an_importable_modifier() {
        let mut c = hyprland();
        c.dmabuf_device = None;
        assert!(pick_dmabuf(&c).unwrap_err().contains("device"));

        let mut c = hyprland();
        c.dmabuf_formats = vec![(xr24(), vec![LNL_CCS, DRM_FORMAT_MOD_INVALID])];
        assert_eq!(
            pick_dmabuf(&c).unwrap_err(),
            "no modifier the encoder imports"
        );

        let mut c = hyprland();
        c.dmabuf_formats = vec![(u32::from_le_bytes(*b"NV12"), vec![0])];
        assert!(pick_dmabuf(&c).unwrap_err().contains("NV12"));

        // A later format with a usable modifier wins over an unusable one.
        let mut c = hyprland();
        c.dmabuf_formats = vec![(xr24(), vec![LNL_CCS]), (ar24(), vec![TILED_4])];
        let choice = pick_dmabuf(&c).unwrap();
        assert_eq!(choice.format, PixelFormat::Argb8888);
        assert_eq!(choice.modifiers, vec![TILED_4]);
    }

    #[test]
    fn modifiers_parse_from_the_wire() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&TILED_4.to_le_bytes());
        bytes.extend_from_slice(&DRM_FORMAT_MOD_INVALID.to_le_bytes());
        assert_eq!(
            modifiers_from_bytes(&bytes),
            vec![TILED_4, DRM_FORMAT_MOD_INVALID]
        );
        assert_eq!(fourcc_name(xr24()), "XR24");
    }

    #[test]
    fn a_new_batch_after_done_replaces_the_constraints() {
        let mut s = SessionInfo::default();
        s.batch().shm_formats.push(wl_shm::Format::Xrgb8888);
        s.batch().add_dmabuf_format(xr24(), vec![0]);
        s.batch().dmabuf_device = Some(1);
        s.batch().width = 100;
        s.batch().height = 50;
        assert!(!s.done);
        assert_eq!(s.current, Constraints::default());
        s.finish_batch();
        assert!(s.done);
        assert_eq!(s.current.shm_formats, vec![wl_shm::Format::Xrgb8888]);

        // Re-sent constraints: the old ones hold until the batch is done,
        // then are replaced, not appended to.
        s.batch().shm_formats.push(wl_shm::Format::Argb8888);
        s.batch().width = 200;
        assert_eq!(s.current.width, 100);
        s.finish_batch();
        assert_eq!(s.current.shm_formats, vec![wl_shm::Format::Argb8888]);
        assert_eq!(s.current.width, 200);
        assert_eq!(s.current.dmabuf_device, None);
        assert!(s.current.dmabuf_formats.is_empty());

        // A done with nothing before it keeps them.
        s.finish_batch();
        assert_eq!(s.current.width, 200);
    }

    #[test]
    fn gpu_buffers_are_given_up_after_failures_before_any_frame() {
        let mut f = GpuFailures::default();
        assert!(!f.failed(&AfterFailure::Retry));
        assert!(!f.failed(&AfterFailure::Retry));
        assert!(f.failed(&AfterFailure::Retry));

        // New constraints and the session ending do not count, and new
        // constraints start the count over.
        let mut f = GpuFailures::default();
        assert!(!f.failed(&AfterFailure::Retry));
        assert!(!f.failed(&AfterFailure::Retry));
        assert!(!f.failed(&AfterFailure::Reallocate));
        assert!(!f.failed(&AfterFailure::EndSession));
        assert!(!f.failed(&AfterFailure::Retry));
        assert!(!f.failed(&AfterFailure::Retry));
        assert!(f.failed(&AfterFailure::Retry));

        // Buffers that produced a frame work; later failures are retried.
        let mut f = GpuFailures::default();
        assert!(!f.failed(&AfterFailure::Retry));
        f.ready();
        for _ in 0..10 {
            assert!(!f.failed(&AfterFailure::Retry));
        }
        f.new_buffers();
        assert!(!f.failed(&AfterFailure::Retry));
        assert!(!f.failed(&AfterFailure::Retry));
        assert!(f.failed(&AfterFailure::Retry));
    }
}

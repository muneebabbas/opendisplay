//! H.264 through the system FFmpeg: libx264 in software, a V4L2 mem2mem
//! hardware encoder, or VA-API (Intel/AMD), which also encodes HEVC
//! (`hevc_vaapi`, same settings, Main profile). Capture hands us 4-byte RGB
//! pixels in the compositor's byte order ([`PixelFormat`]); swscale converts
//! them to the encoder's 4:2:0 format with BT.709 limited-range
//! coefficients, which the stream's VUI announces so the receiver converts
//! back the same way ([`Converter`], in parallel bands).
//!
//! VA-API takes GPU surfaces, not system memory. Where the driver has a
//! video-processing pipeline the capture pixels are copied into a surface
//! and converted to NV12 by the GPU straight into a surface of the encoder's
//! own pool ([`HwFrames`], [`GpuConvert`]): the one copy a dmabuf capture
//! path would remove later. Otherwise swscale converts as above and the NV12
//! frame is uploaded into such a surface before it is sent.

use std::collections::VecDeque;
use std::sync::Once;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use ff::format::Pixel;
use ff::util::color;
use ffmpeg_next as ff;
use opendisplay_proto::video::{AccessUnit, Codec, NalType, access_units_for, nal_units};
use tracing::{info, warn};

use super::convert::{Converter, band_count};
use super::stage::{YuvFrame, new_video, tag_bt709};
use super::steps::Steps;
use super::vpp::{GpuConvert, hw_surface, new_pool};
use super::{Encoded, EncoderSettings};
use crate::backend::PixelFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    X264,
    V4l2m2m,
    Vaapi,
}

/// The VA-API device and the surface pool the encoder draws from. Owns one
/// reference to each; the codec context holds its own.
struct HwFrames {
    device: *mut ff::ffi::AVBufferRef,
    frames: *mut ff::ffi::AVBufferRef,
}

// SAFETY: the buffers are reference counted by FFmpeg and only used from the
// thread that owns the encoder; moving them between threads is fine.
unsafe impl Send for HwFrames {}

impl HwFrames {
    /// Open the default VA-API device (libva picks the render node) and a
    /// pool of NV12 surfaces at the encode size.
    fn new(width: u32, height: u32) -> Result<Self> {
        use ff::ffi::*;
        let mut hw = Self {
            device: std::ptr::null_mut(),
            frames: std::ptr::null_mut(),
        };
        // SAFETY: out-pointers are valid; on failure FFmpeg leaves them null
        // and Drop unrefs only what was created. `av_hwframe_ctx_alloc`
        // returned non-null, so its `data` is an `AVHWFramesContext` we may
        // fill in until `av_hwframe_ctx_init`.
        unsafe {
            let err = av_hwdevice_ctx_create(
                &mut hw.device,
                AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if err < 0 {
                bail!("no VA-API device: {}", ff::Error::from(err));
            }
        }
        hw.frames = new_pool(hw.device, AVPixelFormat::AV_PIX_FMT_NV12, width, height)?;
        Ok(hw)
    }

    /// Copy a system-memory NV12 frame into a fresh surface, carrying pts
    /// and picture type over.
    fn upload(&self, sw: &ff::frame::Video) -> Result<ff::frame::Video> {
        use ff::ffi::*;
        let mut hw = hw_surface(self.frames)?;
        // SAFETY: `hw` is a surface we own; `sw` is valid.
        unsafe {
            let err = av_hwframe_transfer_data(hw.as_mut_ptr(), sw.as_ptr(), 0);
            if err < 0 {
                bail!("VA-API upload: {}", ff::Error::from(err));
            }
            let err = av_frame_copy_props(hw.as_mut_ptr(), sw.as_ptr());
            if err < 0 {
                bail!("av_frame_copy_props: {}", ff::Error::from(err));
            }
        }
        Ok(hw)
    }
}

impl Drop for HwFrames {
    fn drop(&mut self) {
        // SAFETY: each is null or a reference we own.
        unsafe {
            ff::ffi::av_buffer_unref(&mut self.frames);
            ff::ffi::av_buffer_unref(&mut self.device);
        }
    }
}

/// A frame the encoder has and has not returned a packet for yet.
struct InFlight {
    pts: i64,
    captured_at: Instant,
    /// When the encode call for it started.
    started: Instant,
    /// The GPU conversion's source surface, held until the packet is out so
    /// the pool cannot reuse it while the GPU may still read it.
    _source: Option<ff::frame::Video>,
}

pub struct FfmpegEncoder {
    codec: Codec,
    enc: ff::encoder::Video,
    convert: Converter,
    yuv: ff::frame::Video,
    pts: i64,
    /// Frames handed to the encoder whose packet has not come out yet, in
    /// pts order. A hardware encoder may return a packet one call late.
    in_flight: VecDeque<InFlight>,
    /// Parameter sets from the last IDR (SPS and PPS; VPS too for HEVC),
    /// prepended to any IDR that arrives without them (§5.1). x264 repeats
    /// them itself; V4L2 drivers vary.
    param_sets: Vec<u8>,
    /// VA-API only: where converted frames are uploaded before encoding.
    /// Declared after `enc` so the codec drops its references first.
    hw: Option<HwFrames>,
    /// VA-API only: converts the capture's RGB to NV12 on the GPU, when the
    /// driver can. `None` means swscale does it, on the CPU.
    gpu: Option<GpuConvert>,
    /// The GPU conversion failed once and is not used again. Its context is
    /// kept until the encoder closes: with iHD, destroying a VPP context
    /// makes later CPU uploads to the same device fail (EIO).
    gpu_failed: bool,
    /// Where the encode calls' time goes, logged at debug level.
    steps: Steps,
}

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let _ = ff::init();
        // FFmpeg's own log is noise next to ours (v4l2m2m probing in
        // particular logs every device it rejects).
        ff::util::log::set_level(ff::util::log::Level::Fatal);
    });
}

impl FfmpegEncoder {
    /// `width`/`height` are already even. HEVC is VA-API only.
    pub fn new(kind: Kind, width: u32, height: u32, s: EncoderSettings) -> Result<Self> {
        init();
        let name = match (kind, s.codec) {
            (Kind::X264, Codec::H264) => "libx264",
            (Kind::V4l2m2m, Codec::H264) => "h264_v4l2m2m",
            (Kind::Vaapi, Codec::H264) => "h264_vaapi",
            (Kind::Vaapi, Codec::Hevc) => "hevc_vaapi",
            (_, Codec::Hevc) => bail!("{kind:?} does not encode HEVC"),
        };
        let codec = ff::encoder::find_by_name(name)
            .ok_or_else(|| anyhow!("this FFmpeg build has no {name}"))?;
        let format = pick_format(&codec, kind);
        let hw = match kind {
            Kind::Vaapi => Some(HwFrames::new(width, height)?),
            _ => None,
        };

        let mut v = ff::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;
        v.set_width(width);
        v.set_height(height);
        match &hw {
            Some(hw) => {
                v.set_format(Pixel::VAAPI);
                // SAFETY: field write on a context we own and have not
                // opened; the context takes its own reference.
                unsafe {
                    let r = ff::ffi::av_buffer_ref(hw.frames);
                    if r.is_null() {
                        bail!("av_buffer_ref failed");
                    }
                    (*v.as_mut_ptr()).hw_frames_ctx = r;
                }
            }
            None => v.set_format(format),
        }
        v.set_time_base((1, 1_000_000));
        let fps = s.max_fps.round().max(1.0) as i32;
        v.set_frame_rate(Some((fps, 1)));
        v.set_max_b_frames(0);
        v.set_colorspace(color::Space::BT709);
        v.set_color_primaries(color::Primaries::BT709);
        v.set_color_transfer_characteristic(color::TransferCharacteristic::BT709);
        v.set_color_range(color::Range::MPEG);

        let mut opts = ff::Dictionary::new();
        match kind {
            Kind::X264 => {
                // Quality-targeted with a VBV cap: a static desktop costs
                // almost nothing, motion is held to the bitrate, and a
                // quarter-second buffer keeps one frame from stalling the link.
                let kbps = (s.bitrate_bps / 1000).max(500);
                v.set_max_bit_rate(s.bitrate_bps as usize);
                // SAFETY: plain field write on a context we own and have not opened.
                unsafe { (*v.as_mut_ptr()).rc_buffer_size = (s.bitrate_bps / 4) as i32 };
                opts.set("preset", "superfast");
                opts.set("tune", "zerolatency");
                opts.set("crf", "21");
                // A pict_type of I becomes an IDR, not a recovery-point I frame.
                opts.set("forced-idr", "1");
                // §5.3: keyframes only on demand.
                opts.set(
                    "x264-params",
                    &format!(
                        "keyint=infinite:scenecut=0:vbv-maxrate={kbps}:vbv-bufsize={}",
                        kbps / 4
                    ),
                );
                if s.threads > 0 {
                    opts.set("threads", &s.threads.to_string());
                }
            }
            Kind::V4l2m2m => {
                v.set_bit_rate(s.bitrate_bps as usize);
                // The longest GOP drivers accept without complaint; IDRs are
                // forced on demand anyway.
                v.set_gop(32_767);
            }
            Kind::Vaapi => {
                // The same targets as x264: quality 21 under a cap of the
                // bitrate with a quarter-second buffer. No rc_mode is forced,
                // so FFmpeg picks one the driver supports: QVBR on Intel iHD
                // (tested); other drivers are untested.
                v.set_bit_rate(s.bitrate_bps as usize);
                v.set_max_bit_rate(s.bitrate_bps as usize);
                // SAFETY: plain field writes on a context we own and have not opened.
                unsafe {
                    (*v.as_mut_ptr()).rc_buffer_size = (s.bitrate_bps / 4) as i32;
                    (*v.as_mut_ptr()).global_quality = 21;
                }
                // §5.3: keyframes only on demand; a forced I picture is an IDR.
                v.set_gop(i32::MAX as u32);
                // One frame in flight: the default of two adds a frame of latency.
                opts.set("async_depth", "1");
                // No encoder-identification SEI in every IDR.
                opts.set("sei", "0");
                if s.codec == Codec::Hevc {
                    // 8-bit 4:2:0: what the receivers decode into NV12.
                    opts.set("profile", "main");
                }
            }
        }
        let enc = v
            .open_with(opts)
            .with_context(|| format!("opening {name} at {width}x{height}"))?;

        let convert = Converter::new(width, height, format, band_count(height, s.threads))?;

        let yuv = new_video(format, width, height);

        let gpu = hw.as_ref().and_then(gpu_convert);

        Ok(Self {
            codec: s.codec,
            enc,
            convert,
            yuv,
            pts: 0,
            in_flight: VecDeque::new(),
            param_sets: Vec::new(),
            hw,
            gpu,
            gpu_failed: false,
            steps: Steps::new(name),
        })
    }

    /// The 4:2:0 format the encoder was opened with.
    pub fn format(&self) -> Pixel {
        self.yuv.format()
    }

    /// Whether the GPU converts colours: the encoder then takes the capture's
    /// RGB frames itself ([`Self::encode`]) and none are pre-converted.
    pub fn gpu_convert(&self) -> bool {
        self.gpu.is_some() && !self.gpu_failed
    }

    pub fn encode(
        &mut self,
        pixels: &[u8],
        stride: u32,
        format: PixelFormat,
        force_idr: bool,
        captured_at: Instant,
    ) -> Result<Vec<Encoded>> {
        let t0 = Instant::now();
        self.steps.start();
        if let (Some(gpu), false) = (&mut self.gpu, self.gpu_failed) {
            match Self::convert_on_gpu(
                gpu,
                self.hw.as_ref(),
                pixels,
                stride,
                format,
                &mut self.steps,
            ) {
                Ok((mut surface, source)) => {
                    return self.send(&mut surface, Some(source), force_idr, captured_at, t0);
                }
                Err(e) => {
                    // A driver that failed once will fail again: keep
                    // streaming on the CPU path instead.
                    warn!(
                        "VA-API colour conversion failed, converting on the CPU from now on: {e:#}"
                    );
                    self.gpu_failed = true;
                }
            }
        }
        self.convert
            .convert(pixels, stride, format, &mut self.yuv)?;
        self.steps.lap("convert");
        let mut yuv = std::mem::replace(&mut self.yuv, ff::frame::Video::empty());
        let out = self.submit(&mut yuv, force_idr, captured_at, t0);
        self.yuv = yuv;
        out
    }

    /// Encode a frame the convert stage already converted. The encoder takes
    /// a reference to its buffers, no copy; `encode_ms` then excludes the
    /// conversion.
    pub fn encode_yuv(
        &mut self,
        frame: &YuvFrame,
        force_idr: bool,
        captured_at: Instant,
    ) -> Result<Vec<Encoded>> {
        let t0 = Instant::now();
        self.steps.start();
        let v = frame.video();
        if (v.format(), v.width(), v.height())
            != (self.yuv.format(), self.yuv.width(), self.yuv.height())
        {
            bail!(
                "converted frame is {:?} {}x{}, the encoder takes {:?} {}x{}",
                v.format(),
                v.width(),
                v.height(),
                self.yuv.format(),
                self.yuv.width(),
                self.yuv.height()
            );
        }
        let mut f = ff::frame::Video::empty();
        // SAFETY: both frames are valid; av_frame_ref shares the buffers by
        // reference count (pts and picture type are then set on our copy).
        if unsafe { ff::ffi::av_frame_ref(f.as_mut_ptr(), v.as_ptr()) } < 0 {
            bail!("av_frame_ref failed");
        }
        self.submit(&mut f, force_idr, captured_at, t0)
    }

    /// RGB pixels to a fresh NV12 surface of the encoder's pool, on the GPU.
    /// Returns that surface and the source surface the GPU reads, which must
    /// be kept until the encoder is done ([`GpuConvert::convert`]).
    fn convert_on_gpu(
        gpu: &mut GpuConvert,
        hw: Option<&HwFrames>,
        pixels: &[u8],
        stride: u32,
        format: PixelFormat,
        steps: &mut Steps,
    ) -> Result<(ff::frame::Video, ff::frame::Video)> {
        let hw = hw.ok_or_else(|| anyhow!("no VA-API surfaces"))?;
        let mut surface = hw_surface(hw.frames)?;
        let source = gpu.convert(&surface, pixels, stride, format, steps)?;
        tag_bt709(&mut surface);
        Ok((surface, source))
    }

    fn submit(
        &mut self,
        frame: &mut ff::frame::Video,
        force_idr: bool,
        captured_at: Instant,
        t0: Instant,
    ) -> Result<Vec<Encoded>> {
        match &self.hw {
            Some(hw) => {
                let mut surface = hw.upload(frame)?;
                self.steps.lap("upload");
                self.send(&mut surface, None, force_idr, captured_at, t0)
            }
            None => self.send(frame, None, force_idr, captured_at, t0),
        }
    }

    /// Encode a frame the encoder can take as it is: system memory, or a
    /// VA-API surface. `source` is kept until the frame's packet is out.
    fn send(
        &mut self,
        frame: &mut ff::frame::Video,
        source: Option<ff::frame::Video>,
        force_idr: bool,
        captured_at: Instant,
        t0: Instant,
    ) -> Result<Vec<Encoded>> {
        frame.set_pts(Some(self.pts));
        frame.set_kind(if force_idr {
            ff::picture::Type::I
        } else {
            ff::picture::Type::None
        });
        if let Err(e) = self.enc.send_frame(frame) {
            if source.is_some() {
                // The GPU may still be reading the source surface: park it
                // in the queue until a later packet drains this entry,
                // rather than handing it back to the pool now. The pts is
                // spent so the next frame's timings stay its own.
                self.in_flight.push_back(InFlight {
                    pts: self.pts,
                    captured_at,
                    started: t0,
                    _source: source,
                });
                self.pts += 16_667;
            }
            return Err(e).context("send_frame");
        }
        self.steps.lap("send_frame");
        self.in_flight.push_back(InFlight {
            pts: self.pts,
            captured_at,
            started: t0,
            _source: source,
        });
        // 1/60 s apart in the time base; the encoder only needs monotonic pts.
        self.pts += 16_667;

        // Usually one packet per call; a hardware encoder can lag by one and
        // then return two. Every picture goes out: dropping a P frame would
        // corrupt the ones that reference it.
        let mut out = Vec::new();
        let mut pkt = ff::Packet::empty();
        loop {
            match self.enc.receive_packet(&mut pkt) {
                Ok(()) => {}
                Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => break,
                Err(e) => return Err(e).context("receive_packet"),
            }
            let Some(data) = pkt.data() else { continue };
            let (captured_at, started) = self.take_in_flight(pkt.pts());
            if let Some(unit) = self.access_unit(data) {
                out.push(Encoded {
                    unit,
                    captured_at,
                    encode_ms: started.elapsed().as_secs_f64() * 1000.0,
                });
            }
        }
        self.steps.lap("receive_packet");
        self.steps.finish();
        Ok(out)
    }

    /// The capture and start instants of the frame with `pts`. Entries
    /// before it are dropped: their packets are not coming.
    fn take_in_flight(&mut self, pts: Option<i64>) -> (Instant, Instant) {
        while let Some(f) = self.in_flight.pop_front() {
            if pts.is_none_or(|t| t <= f.pts) {
                return (f.captured_at, f.started);
            }
        }
        let now = Instant::now();
        (now, now)
    }

    /// One packet is one picture; normalize it to an access unit and make
    /// sure an IDR carries its parameter sets (SPS and PPS; VPS too for HEVC).
    fn access_unit(&mut self, data: &[u8]) -> Option<AccessUnit> {
        let mut unit = access_units_for(self.codec, data).pop()?;
        if unit.is_idr {
            let mut sets = Vec::new();
            for nal in nal_units(&unit.annexb) {
                if NalType::of_codec(self.codec, nal).is_some_and(NalType::is_parameter_set) {
                    sets.extend_from_slice(&[0, 0, 0, 1]);
                    sets.extend_from_slice(nal);
                }
            }
            if sets.is_empty() {
                if self.param_sets.is_empty() {
                    warn!("encoder: dropped an IDR without SPS/PPS (none seen yet)");
                    return None;
                }
                let mut with = self.param_sets.clone();
                with.extend_from_slice(&unit.annexb);
                unit.annexb = with;
            } else {
                self.param_sets = sets;
            }
        }
        Some(unit)
    }
}

/// Set up GPU colour conversion for a VA-API encoder, or say why not and
/// leave it to swscale.
fn gpu_convert(hw: &HwFrames) -> Option<GpuConvert> {
    // Development knob for A/B benchmarks: force the CPU path.
    if std::env::var_os("OD_VAAPI_CPU_CONVERT").is_some_and(|v| v == "1") {
        info!("VA-API: colour conversion on the CPU (OD_VAAPI_CPU_CONVERT=1)");
        return None;
    }
    match GpuConvert::new(hw.device, hw.frames) {
        Ok(g) => Some(g),
        Err(e) => {
            warn!("VA-API: GPU colour conversion unavailable, converting on the CPU: {e:#}");
            None
        }
    }
}

/// x264 wants planar I420; V4L2 encoders usually take NV12 and sometimes
/// only that. VA-API surfaces are NV12, whatever the codec lists (it lists
/// only the opaque `VAAPI` format).
fn pick_format(codec: &ff::Codec, kind: Kind) -> Pixel {
    let prefs: &[Pixel] = match kind {
        Kind::X264 => &[Pixel::YUV420P, Pixel::NV12],
        Kind::V4l2m2m => &[Pixel::NV12, Pixel::YUV420P],
        Kind::Vaapi => return Pixel::NV12,
    };
    let listed: Vec<Pixel> = codec
        .video()
        .ok()
        .and_then(|v| v.formats())
        .map(|f| f.collect())
        .unwrap_or_default();
    prefs
        .iter()
        .copied()
        .find(|p| listed.is_empty() || listed.contains(p))
        .unwrap_or(prefs[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::Backend;
    use crate::encoder::vpp::{download, sample};

    /// Opens VA-API, or explains why the test is skipped (no GPU in CI).
    fn vaapi(w: u32, h: u32) -> Option<FfmpegEncoder> {
        vaapi_codec(Codec::H264, w, h)
    }

    fn vaapi_codec(codec: Codec, w: u32, h: u32) -> Option<FfmpegEncoder> {
        let s = EncoderSettings {
            backend: Backend::Vaapi,
            codec,
            bitrate_bps: 8_000_000,
            max_fps: 60.0,
            threads: 1,
        };
        match FfmpegEncoder::new(Kind::Vaapi, w, h, s) {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("skipped: VA-API unavailable here ({e:#})");
                None
            }
        }
    }

    /// A moving gradient, so every frame differs.
    fn frame(w: u32, h: u32, i: u32) -> (Vec<u8>, u32) {
        let stride = w * 4 + 32;
        let mut v = vec![0u8; (stride * h) as usize];
        for (y, row) in v.chunks_exact_mut(stride as usize).enumerate() {
            for (x, p) in row[..(w * 4) as usize].chunks_exact_mut(4).enumerate() {
                let c = ((x as u32 + y as u32 + i * 7) & 0xff) as u8;
                p.copy_from_slice(&[c, 255 - c, c / 2, 255]);
            }
        }
        (v, stride)
    }

    /// Twelve frames with IDRs on demand only, decoded by FFmpeg's H.264
    /// decoder, with the colour conversion on the GPU or, `gpu` false, the CPU.
    fn idrs_only_on_demand_and_decode_as_bt709(gpu: bool) {
        let (w, h) = (320, 240);
        let Some(mut e) = vaapi(w, h) else { return };
        if gpu && !e.gpu_convert() {
            eprintln!("skipped: no GPU colour conversion here");
            return;
        }
        if !gpu {
            e.gpu_failed = true;
        }
        let Some(codec) = ff::decoder::find(ff::codec::Id::H264) else {
            eprintln!("skipped: this FFmpeg has no H.264 decoder");
            return;
        };
        let mut dec = ff::codec::context::Context::new_with_codec(codec)
            .decoder()
            .video()
            .unwrap();
        let mut idrs = Vec::new();
        let mut decoded = 0;
        for i in 0..12 {
            let (px, stride) = frame(w, h, i);
            let force = i == 0 || i == 7;
            let units = e
                .encode(&px, stride, PixelFormat::Xrgb8888, force, Instant::now())
                .unwrap();
            // One frame in flight at most: each call returns its own picture.
            assert_eq!(units.len(), 1, "frame {i}");
            for u in units {
                if u.unit.is_idr {
                    idrs.push(i);
                }
                dec.send_packet(&ff::Packet::copy(&u.unit.annexb)).unwrap();
                let mut out = ff::frame::Video::empty();
                while dec.receive_frame(&mut out).is_ok() {
                    assert_eq!((out.width(), out.height()), (w, h));
                    assert_eq!(out.color_primaries(), color::Primaries::BT709);
                    assert_eq!(out.color_range(), color::Range::MPEG);
                    decoded += 1;
                }
            }
        }
        assert_eq!(idrs, [0, 7]);
        dec.send_eof().unwrap();
        let mut out = ff::frame::Video::empty();
        while dec.receive_frame(&mut out).is_ok() {
            decoded += 1;
        }
        assert_eq!(decoded, 12);
    }

    #[test]
    fn vaapi_idrs_only_on_demand_and_decode_as_bt709() {
        idrs_only_on_demand_and_decode_as_bt709(false);
    }

    #[test]
    fn vaapi_gpu_idrs_only_on_demand_and_decode_as_bt709() {
        idrs_only_on_demand_and_decode_as_bt709(true);
    }

    /// FFmpeg's software decoder for `wire`, or why the test skips.
    fn decoder(wire: Codec) -> Option<ff::Codec> {
        let id = match wire {
            Codec::H264 => ff::codec::Id::H264,
            Codec::Hevc => ff::codec::Id::HEVC,
        };
        let found = ff::decoder::find(id);
        if found.is_none() {
            eprintln!("skipped: this FFmpeg has no {wire} decoder");
        }
        found
    }

    fn hevc_types(annexb: &[u8]) -> Vec<u8> {
        nal_units(annexb).map(|n| (n[0] >> 1) & 0x3f).collect()
    }

    /// Twelve HEVC frames with IDRs on demand only (frames 0 and 7), decoded
    /// by FFmpeg's HEVC decoder: every IDR is type 19/20 and starts with
    /// VPS, SPS, PPS; the others are plain pictures.
    fn hevc_idrs_only_on_demand_with_parameter_sets(gpu: bool) {
        let (w, h) = (320, 240);
        let Some(mut e) = vaapi_codec(Codec::Hevc, w, h) else {
            return;
        };
        if gpu && !e.gpu_convert() {
            eprintln!("skipped: no GPU colour conversion here");
            return;
        }
        if !gpu {
            e.gpu_failed = true;
        }
        let Some(codec) = decoder(Codec::Hevc) else {
            return;
        };
        let mut dec = ff::codec::context::Context::new_with_codec(codec)
            .decoder()
            .video()
            .unwrap();
        let mut idrs = Vec::new();
        let mut decoded = 0;
        for i in 0..12 {
            let (px, stride) = frame(w, h, i);
            let force = i == 0 || i == 7;
            let units = e
                .encode(&px, stride, PixelFormat::Xrgb8888, force, Instant::now())
                .unwrap();
            assert_eq!(units.len(), 1, "frame {i}");
            for u in units {
                let types = hevc_types(&u.unit.annexb);
                if u.unit.is_idr {
                    idrs.push(i);
                    assert!(
                        types.starts_with(&[32, 33, 34]),
                        "frame {i}: IDR starts with {types:?}"
                    );
                    assert!(
                        types[3..].iter().all(|t| matches!(t, 19 | 20)),
                        "frame {i}: {types:?}"
                    );
                } else {
                    assert!(
                        types.iter().all(|t| *t < 16),
                        "frame {i}: not an IDR but {types:?}"
                    );
                }
                dec.send_packet(&ff::Packet::copy(&u.unit.annexb)).unwrap();
                let mut out = ff::frame::Video::empty();
                while dec.receive_frame(&mut out).is_ok() {
                    assert_eq!((out.width(), out.height()), (w, h));
                    assert_eq!(out.color_primaries(), color::Primaries::BT709);
                    assert_eq!(out.color_range(), color::Range::MPEG);
                    decoded += 1;
                }
            }
        }
        assert_eq!(idrs, [0, 7]);
        dec.send_eof().unwrap();
        let mut out = ff::frame::Video::empty();
        while dec.receive_frame(&mut out).is_ok() {
            decoded += 1;
        }
        assert_eq!(decoded, 12);
    }

    #[test]
    fn vaapi_hevc_idrs_only_on_demand_with_parameter_sets() {
        hevc_idrs_only_on_demand_with_parameter_sets(false);
    }

    #[test]
    fn vaapi_hevc_gpu_idrs_only_on_demand_with_parameter_sets() {
        hevc_idrs_only_on_demand_with_parameter_sets(true);
    }

    #[test]
    fn only_vaapi_encodes_hevc() {
        let s = EncoderSettings {
            backend: Backend::X264,
            codec: Codec::Hevc,
            bitrate_bps: 2_000_000,
            max_fps: 60.0,
            threads: 1,
        };
        for kind in [Kind::X264, Kind::V4l2m2m] {
            let e = FfmpegEncoder::new(kind, 320, 240, s).err().unwrap();
            assert!(format!("{e:#}").contains("does not encode HEVC"), "{e:#}");
        }
    }

    #[test]
    fn vaapi_hevc_decodes_at_the_encoded_size() {
        // HEVC codes in CTB multiples and crops: the decoder must still see
        // the size asked for, including sizes that are not 16 or 64 aligned.
        for (w, h) in [(320, 240), (240, 320), (1194, 834), (1000, 776)] {
            let Some(mut e) = vaapi_codec(Codec::Hevc, w, h) else {
                return;
            };
            let Some(codec) = decoder(Codec::Hevc) else {
                return;
            };
            let mut dec = ff::codec::context::Context::new_with_codec(codec)
                .decoder()
                .video()
                .unwrap();
            let mut sizes = Vec::new();
            for i in 0..3 {
                let (px, stride) = frame(w, h, i);
                let units = e
                    .encode(&px, stride, PixelFormat::Xrgb8888, i == 0, Instant::now())
                    .unwrap();
                assert_eq!(units.first().is_some_and(|u| u.unit.is_idr), i == 0);
                for u in units {
                    dec.send_packet(&ff::Packet::copy(&u.unit.annexb)).unwrap();
                    let mut out = ff::frame::Video::empty();
                    while dec.receive_frame(&mut out).is_ok() {
                        sizes.push((out.width(), out.height()));
                    }
                }
            }
            dec.send_eof().unwrap();
            let mut out = ff::frame::Video::empty();
            while dec.receive_frame(&mut out).is_ok() {
                sizes.push((out.width(), out.height()));
            }
            assert_eq!(sizes, vec![(w, h); 3], "{w}x{h}");
        }
    }

    #[test]
    fn vaapi_reopens_at_a_new_size() {
        // A receiver rotation or resize reopens the encoder (session.rs).
        for (w, h) in [(320, 240), (240, 320), (1194, 834)] {
            let Some(mut e) = vaapi(w, h) else { return };
            let (px, stride) = frame(w, h, 0);
            let units = e
                .encode(&px, stride, PixelFormat::Xrgb8888, true, Instant::now())
                .unwrap();
            assert!(units.first().is_some_and(|u| u.unit.is_idr), "{w}x{h}");
        }
    }

    /// Six flat colours, as (r, g, b).
    const PATCHES: [[u8; 3]; 6] = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 255],
        [0, 0, 0],
        [128, 128, 128],
    ];
    const BLOCK: usize = 64;

    /// `PATCHES` side by side in `BLOCK` x `BLOCK` squares, in the byte
    /// order of `format`, with a padding byte that must not matter.
    fn patches(format: PixelFormat) -> (Vec<u8>, u32) {
        let stride = PATCHES.len() * BLOCK * 4;
        let mut v = vec![0u8; stride * BLOCK];
        for row in v.chunks_exact_mut(stride) {
            for (i, [r, g, b]) in PATCHES.iter().copied().enumerate() {
                let px = match format {
                    PixelFormat::Xrgb8888 | PixelFormat::Argb8888 => [b, g, r, 0x7f],
                    PixelFormat::Xbgr8888 | PixelFormat::Abgr8888 => [r, g, b, 0x7f],
                };
                for p in row[i * BLOCK * 4..(i + 1) * BLOCK * 4].chunks_exact_mut(4) {
                    p.copy_from_slice(&px);
                }
            }
        }
        (v, stride as u32)
    }

    /// The VA-API device and a GPU converter on it, or why the test skips.
    fn gpu(w: u32, h: u32) -> Option<(HwFrames, GpuConvert)> {
        let hw = match HwFrames::new(w, h) {
            Ok(hw) => hw,
            Err(e) => {
                eprintln!("skipped: VA-API unavailable here ({e:#})");
                return None;
            }
        };
        match GpuConvert::new(hw.device, hw.frames) {
            Ok(g) => Some((hw, g)),
            Err(e) => {
                let why = format!("{e:#}");
                // A driver that converts, but wrongly, is a failure.
                assert!(!why.contains("BT.709"), "{why}");
                eprintln!("skipped: no GPU colour conversion here ({why})");
                None
            }
        }
    }

    #[test]
    fn vaapi_gpu_convert_is_bt709_limited() {
        let (w, h) = (PATCHES.len() * BLOCK, BLOCK);
        let Some((hw, mut gpu)) = gpu(w as u32, h as u32) else {
            return;
        };
        // Both upload paths: the surface mapping, then FFmpeg's transfer.
        let mapped = gpu.direct_upload();
        for (direct, format) in [
            (mapped, PixelFormat::Xrgb8888),
            (mapped, PixelFormat::Xbgr8888),
            (false, PixelFormat::Xrgb8888),
            (false, PixelFormat::Xbgr8888),
        ] {
            gpu.set_direct_upload(direct);
            let (px, stride) = patches(format);
            let dst = hw_surface(hw.frames).unwrap();
            let _src = gpu
                .convert(&dst, &px, stride, format, &mut Steps::new("test"))
                .unwrap();
            let got = download(&dst).unwrap();
            // swscale is the reference the receiver's colours are defined by.
            let mut want = new_video(Pixel::NV12, w as u32, h as u32);
            Converter::new(w as u32, h as u32, Pixel::NV12, 1)
                .unwrap()
                .convert(&px, stride, format, &mut want)
                .unwrap();
            for (i, rgb) in PATCHES.iter().enumerate() {
                let at = (i * BLOCK + BLOCK / 2, BLOCK / 2);
                let (g, wnt) = (sample(&got, at.0, at.1), sample(&want, at.0, at.1));
                assert!(
                    g.iter().zip(wnt).all(|(g, w)| g.abs_diff(w) <= 1),
                    "{format:?} {rgb:?} (mapped upload {direct}): GPU {g:?}, swscale {wnt:?}"
                );
            }
            // Known values for red and white, so the reference itself is checked.
            let red = sample(&got, BLOCK / 2, BLOCK / 2);
            assert!(
                red.iter()
                    .zip([63, 102, 240])
                    .all(|(g, w): (&u8, u8)| g.abs_diff(w) <= 1)
            );
            let white = sample(&got, 3 * BLOCK + BLOCK / 2, BLOCK / 2);
            assert!(
                white
                    .iter()
                    .zip([235, 128, 128])
                    .all(|(g, w): (&u8, u8)| g.abs_diff(w) <= 1)
            );
        }
    }

    #[test]
    fn vaapi_gpu_encodes_the_right_colours() {
        gpu_encodes_the_right_colours(Codec::H264);
    }

    #[test]
    fn vaapi_hevc_gpu_encodes_the_right_colours() {
        gpu_encodes_the_right_colours(Codec::Hevc);
    }

    fn gpu_encodes_the_right_colours(wire: Codec) {
        // Every row of patches repeated: the HEVC encoder refuses a picture
        // as short as one block (Intel iHD fails to open it at 64 rows).
        let rows = 3;
        let (w, h) = ((PATCHES.len() * BLOCK) as u32, (rows * BLOCK) as u32);
        let Some(mut e) = vaapi_codec(wire, w, h) else {
            return;
        };
        if !e.gpu_convert() {
            eprintln!("skipped: no GPU colour conversion here");
            return;
        }
        let Some(codec) = decoder(wire) else { return };
        for format in [PixelFormat::Xrgb8888, PixelFormat::Xbgr8888] {
            let mut dec = ff::codec::context::Context::new_with_codec(codec)
                .decoder()
                .video()
                .unwrap();
            let (row, stride) = patches(format);
            let px = row.repeat(rows);
            let mut last = None;
            for i in 0..3 {
                for u in e
                    .encode(&px, stride, format, i == 0, Instant::now())
                    .unwrap()
                {
                    dec.send_packet(&ff::Packet::copy(&u.unit.annexb)).unwrap();
                    let mut out = ff::frame::Video::empty();
                    while dec.receive_frame(&mut out).is_ok() {
                        last = Some(std::mem::replace(&mut out, ff::frame::Video::empty()));
                    }
                }
            }
            let out = last.expect("a decoded frame");
            for (i, want) in [
                [63, 102, 240],
                [173, 42, 26],
                [32, 240, 118],
                [235, 128, 128],
                [16, 128, 128],
                [126, 128, 128],
            ]
            .into_iter()
            .enumerate()
            {
                let (x, y) = (i * BLOCK + BLOCK / 2, BLOCK / 2);
                let got = [
                    out.data(0)[y * out.stride(0) + x],
                    out.data(1)[(y / 2) * out.stride(1) + x / 2],
                    out.data(2)[(y / 2) * out.stride(2) + x / 2],
                ];
                // Lossy, but flat patches stay within a few levels.
                assert!(
                    got.iter()
                        .zip(want)
                        .all(|(g, w): (&u8, u8)| g.abs_diff(w) <= 4),
                    "{format:?} patch {i}: decoded {got:?}, expected {want:?}"
                );
            }
        }
    }

    #[test]
    fn vaapi_gpu_frames_encode_in_order() {
        // Nothing waits for the GPU conversion before the encoder takes its
        // output, and surfaces are pooled: each picture must still be its
        // own frame, not a pool neighbour or a half-converted one. Full
        // size, so the conversion takes long enough to be caught out.
        let (w, h) = (2388, 1668);
        let Some(mut e) = vaapi(w, h) else { return };
        if !e.gpu_convert() {
            eprintln!("skipped: no GPU colour conversion here");
            return;
        }
        let Some(codec) = ff::decoder::find(ff::codec::Id::H264) else {
            eprintln!("skipped: this FFmpeg has no H.264 decoder");
            return;
        };
        let mut dec = ff::codec::context::Context::new_with_codec(codec)
            .decoder()
            .video()
            .unwrap();
        // Grey levels far apart, one per frame, in BGRX.
        let level = |i: usize| (16 + (i * 37) % 224) as u8;
        let mut want = Vec::new();
        let mut got = Vec::new();
        for i in 0..16 {
            let px = vec![level(i); (w * h * 4) as usize];
            want.push(level(i));
            for u in e
                .encode(&px, w * 4, PixelFormat::Xrgb8888, i == 0, Instant::now())
                .unwrap()
            {
                dec.send_packet(&ff::Packet::copy(&u.unit.annexb)).unwrap();
                let mut out = ff::frame::Video::empty();
                while dec.receive_frame(&mut out).is_ok() {
                    // Top-left, centre and bottom-right luma.
                    let y = |x: usize, r: usize| out.data(0)[r * out.stride(0) + x];
                    let (w, h) = (w as usize, h as usize);
                    got.push([y(8, 8), y(w / 2, h / 2), y(w - 8, h - 8)]);
                }
            }
        }
        dec.send_eof().unwrap();
        let mut out = ff::frame::Video::empty();
        while dec.receive_frame(&mut out).is_ok() {
            got.push([out.data(0)[0]; 3]);
        }
        assert_eq!(got.len(), want.len());
        for (i, (g, l)) in got.iter().zip(&want).enumerate() {
            // Limited range: 16 + l * 219 / 255, and lossy.
            let y = 16 + (*l as u32 * 219 + 127) / 255;
            assert!(
                g.iter().all(|&g| (g as u32).abs_diff(y) <= 3),
                "frame {i}: luma {g:?}, expected {y}"
            );
        }
    }

    #[test]
    fn vaapi_gpu_convert_handles_any_size() {
        // The last is what a 1001x777 capture is cropped to (even sizes);
        // the capture buffer keeps its own, larger, stride.
        for (w, h, cw, ch) in [
            (2388, 1668, 2388, 1668),
            (1194, 834, 1194, 834),
            (1000, 776, 1001, 777),
        ] {
            let Some(mut e) = vaapi(w, h) else { return };
            if !e.gpu_convert() {
                eprintln!("skipped: no GPU colour conversion here");
                return;
            }
            let stride = cw * 4;
            let px = vec![0x40u8; (stride * ch) as usize];
            let mut times = Vec::new();
            for i in 0..12 {
                let t = Instant::now();
                let units = e
                    .encode(&px, stride, PixelFormat::Xrgb8888, i == 0, Instant::now())
                    .unwrap();
                times.push(t.elapsed().as_secs_f64() * 1000.0);
                assert_eq!(units.len(), 1, "{w}x{h} frame {i}");
                assert_eq!(units[0].unit.is_idr, i == 0, "{w}x{h} frame {i}");
            }
            times.sort_by(f64::total_cmp);
            eprintln!("{w}x{h}: encode call p50 {:.2} ms", times[times.len() / 2]);

            // The conversion alone: upload, VPP and a read back (informational).
            let Some((hw, mut g)) = gpu(w, h) else { return };
            let dst = hw_surface(hw.frames).unwrap();
            let mut conv = Vec::new();
            for _ in 0..30 {
                let t = Instant::now();
                let src = g
                    .convert(
                        &dst,
                        &px,
                        stride,
                        PixelFormat::Xrgb8888,
                        &mut Steps::new("test"),
                    )
                    .unwrap();
                // Mapping the output waits for the conversion.
                download(&dst).unwrap();
                drop(src);
                conv.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            conv.sort_by(f64::total_cmp);
            eprintln!(
                "{w}x{h}: GPU convert and read back p50 {:.2} ms",
                conv[conv.len() / 2]
            );
        }
    }
}

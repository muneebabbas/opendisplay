//! H.264 through the system FFmpeg: libx264 in software, a V4L2 mem2mem
//! hardware encoder, or VA-API (Intel/AMD). Capture hands us 4-byte RGB
//! pixels in the compositor's byte order ([`PixelFormat`]); swscale converts
//! them to the encoder's 4:2:0 format with BT.709 limited-range
//! coefficients, which the stream's VUI announces so the receiver converts
//! back the same way ([`Converter`], in parallel bands).
//!
//! VA-API takes GPU surfaces, not system memory: the converted NV12 frame
//! is uploaded into a surface from the encoder's own pool ([`HwFrames`])
//! before it is sent. That upload is the one copy a dmabuf capture path
//! would remove later; everything around it stays the same.

use std::collections::VecDeque;
use std::sync::Once;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use ff::format::Pixel;
use ff::util::color;
use ffmpeg_next as ff;
use opendisplay_proto::video::{AccessUnit, NalType, access_units, nal_units};
use tracing::warn;

use super::convert::{Converter, band_count};
use super::stage::{YuvFrame, new_video};
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
            hw.frames = av_hwframe_ctx_alloc(hw.device);
            if hw.frames.is_null() {
                bail!("av_hwframe_ctx_alloc failed");
            }
            let fc = (*hw.frames).data as *mut AVHWFramesContext;
            (*fc).format = AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
            (*fc).width = width as i32;
            (*fc).height = height as i32;
            // 0: the pool grows on demand, so a slow encoder holding
            // surfaces never starves the upload.
            (*fc).initial_pool_size = 0;
            let err = av_hwframe_ctx_init(hw.frames);
            if err < 0 {
                bail!("VA-API surface pool: {}", ff::Error::from(err));
            }
        }
        Ok(hw)
    }

    /// Copy a system-memory NV12 frame into a fresh surface, carrying pts
    /// and picture type over.
    fn upload(&self, sw: &ff::frame::Video) -> Result<ff::frame::Video> {
        use ff::ffi::*;
        let mut hw = ff::frame::Video::empty();
        // SAFETY: `hw` is an empty frame we own; the pool and `sw` are valid.
        unsafe {
            let err = av_hwframe_get_buffer(self.frames, hw.as_mut_ptr(), 0);
            if err < 0 {
                bail!("VA-API surface: {}", ff::Error::from(err));
            }
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

pub struct FfmpegEncoder {
    enc: ff::encoder::Video,
    convert: Converter,
    yuv: ff::frame::Video,
    pts: i64,
    /// Capture instants of frames handed to the encoder, by pts. A hardware
    /// encoder may return a packet one call late.
    in_flight: VecDeque<(i64, Instant, Instant)>,
    /// Parameter sets from the last IDR, prepended to any IDR that arrives
    /// without them (§5.1). x264 repeats them itself; V4L2 drivers vary.
    param_sets: Vec<u8>,
    /// VA-API only: where converted frames are uploaded before encoding.
    /// Declared after `enc` so the codec drops its references first.
    hw: Option<HwFrames>,
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
    /// `width`/`height` are already even.
    pub fn new(kind: Kind, width: u32, height: u32, s: EncoderSettings) -> Result<Self> {
        init();
        let name = match kind {
            Kind::X264 => "libx264",
            Kind::V4l2m2m => "h264_v4l2m2m",
            Kind::Vaapi => "h264_vaapi",
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
            }
        }
        let enc = v
            .open_with(opts)
            .with_context(|| format!("opening {name} at {width}x{height}"))?;

        let convert = Converter::new(width, height, format, band_count(height, s.threads))?;

        let yuv = new_video(format, width, height);

        Ok(Self {
            enc,
            convert,
            yuv,
            pts: 0,
            in_flight: VecDeque::new(),
            param_sets: Vec::new(),
            hw,
        })
    }

    /// The 4:2:0 format the encoder was opened with.
    pub fn format(&self) -> Pixel {
        self.yuv.format()
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
        self.convert
            .convert(pixels, stride, format, &mut self.yuv)?;
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

    fn submit(
        &mut self,
        frame: &mut ff::frame::Video,
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
        match &self.hw {
            Some(hw) => {
                let surface = hw.upload(frame)?;
                self.enc.send_frame(&surface)
            }
            None => self.enc.send_frame(frame),
        }
        .context("send_frame")?;
        self.in_flight.push_back((self.pts, captured_at, t0));
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
        Ok(out)
    }

    fn take_in_flight(&mut self, pts: Option<i64>) -> (Instant, Instant) {
        while let Some(&(p, cap, started)) = self.in_flight.front() {
            self.in_flight.pop_front();
            if pts.is_none_or(|t| t <= p) {
                return (cap, started);
            }
        }
        let now = Instant::now();
        (now, now)
    }

    /// One packet is one picture; normalize it to an access unit and make
    /// sure an IDR carries SPS and PPS.
    fn access_unit(&mut self, data: &[u8]) -> Option<AccessUnit> {
        let mut unit = access_units(data).pop()?;
        if unit.is_idr {
            let mut sets = Vec::new();
            for nal in nal_units(&unit.annexb) {
                if matches!(NalType::of(nal), Some(NalType::Sps | NalType::Pps)) {
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

    /// Opens VA-API, or explains why the test is skipped (no GPU in CI).
    fn vaapi(w: u32, h: u32) -> Option<FfmpegEncoder> {
        let s = EncoderSettings {
            backend: Backend::Vaapi,
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

    #[test]
    fn vaapi_idrs_only_on_demand_and_decode_as_bt709() {
        let (w, h) = (320, 240);
        let Some(mut e) = vaapi(w, h) else { return };
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
}

//! H.264 (and, on VA-API, HEVC) encoding behind one [`Encoder`] shape, with
//! the backend picked at run time (#360). `auto` tries, in order:
//!
//! 1. `v4l2m2m` — any V4L2 stateful mem2mem encoder through FFmpeg
//!    (`h264_v4l2m2m`): Raspberry Pi, Rockchip, and Asahi's AVE once a
//!    driver ships. Never auto-selected unless a device accepts the session;
//!    skipped without asking FFmpeg when no `/dev/video*` node is one.
//! 2. `x264` — libx264 through FFmpeg, `tune=zerolatency`. The software
//!    default: several times faster than OpenH264 on the same cores.
//! 3. `openh264` — compiled in, so it works even where the system FFmpeg was
//!    built without libx264 (Fedora's `ffmpeg-free`).
//!
//! `vaapi` — Intel/AMD hardware encode through FFmpeg (`h264_vaapi`) — is
//! opt-in for now. The GPU converts the capture's RGB to NV12 as well (the
//! driver's video-processing pipeline, `vpp.rs`), so the CPU only copies the
//! pixels into a surface; where the driver cannot, or does not get the
//! colours right, swscale converts on the CPU as for the other encoders. At
//! the GPU's power-saving clocks it can still add latency compared with
//! x264, so it does not join `auto` yet.
//!
//! VA-API also encodes HEVC (`hevc_vaapi`, Main profile, the same low-latency
//! settings), the only backend that does. The session picks the codec when
//! the receiver offers HEVC ([`can_encode_hevc`] says whether the GPU opens
//! it); an [`Encoder`] never falls back to another codec by itself.

mod convert;
mod ffmpeg;
mod openh264;
mod stage;
mod steps;
mod v4l2;
mod vpp;

pub use stage::{ConvertStage, MIN_STAGE_WIDTH, YuvFrame};

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use clap::ValueEnum;
use ffmpeg_next as ff;
use opendisplay_proto::video::{AccessUnit, Codec};
use tracing::{debug, info};

use crate::backend::{Frame, FrameBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    /// First that opens: v4l2m2m, x264, openh264. `vaapi` is opt-in.
    Auto,
    V4l2m2m,
    Vaapi,
    X264,
    Openh264,
}

/// `false` when no `/dev/video*` node is a V4L2 H.264 encoder, so
/// [`Backend::V4l2m2m`] cannot open; `true` when one is, or when a node
/// could not be asked.
pub fn v4l2m2m_present() -> bool {
    v4l2::h264_encoder() != v4l2::Presence::Absent
}

impl Backend {
    fn label(self) -> &'static str {
        match self {
            Backend::Auto => "auto",
            Backend::V4l2m2m => "V4L2 M2M (hardware)",
            Backend::Vaapi => "VA-API (hardware)",
            Backend::X264 => "x264",
            Backend::Openh264 => "OpenH264",
        }
    }
}

fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
    }
}

/// Whether a VA-API HEVC encoder opens at this size. Opens a throwaway one
/// through the same constructor the stream uses, once per size per process;
/// logs once per size when it does not.
pub fn can_encode_hevc(width: u32, height: u32) -> bool {
    let (width, height) = (width & !1, height & !1);
    #[cfg(debug_assertions)]
    if std::env::var_os("OD_FAIL_HEVC_ENCODER").is_some_and(|v| v == "1") {
        info!("HEVC encoder unavailable at {width}x{height} (OD_FAIL_HEVC_ENCODER=1); using H.264");
        return false;
    }
    static CACHE: OnceLock<Mutex<HashMap<(u32, u32), bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(&ok) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(width, height))
    {
        return ok;
    }
    // Not held while opening: that takes a while and other sizes may probe.
    let probe = Encoder::new(
        width,
        height,
        EncoderSettings {
            backend: Backend::Vaapi,
            codec: Codec::Hevc,
            bitrate_bps: 2_000_000,
            max_fps: 60.0,
            threads: 1,
        },
    );
    let ok = match probe {
        Ok(_) => true,
        Err(e) => {
            info!("HEVC encoder unavailable at {width}x{height} ({e:#}); using H.264");
            false
        }
    };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((width, height), ok);
    ok
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderSettings {
    pub backend: Backend,
    /// HEVC needs [`Backend::Vaapi`]; any other backend refuses it.
    pub codec: Codec,
    pub bitrate_bps: u32,
    pub max_fps: f32,
    /// 0 = let the backend decide.
    pub threads: u16,
}

/// One encoded picture ready for the wire.
pub struct Encoded {
    pub unit: AccessUnit,
    pub captured_at: Instant,
    pub encode_ms: f64,
}

enum Imp {
    Ffmpeg(Box<ffmpeg::FfmpegEncoder>),
    OpenH264(Box<openh264::OpenH264Encoder>),
}

pub struct Encoder {
    imp: Imp,
    backend: Backend,
    codec: Codec,
    width: u32,
    height: u32,
}

impl Encoder {
    /// Open the requested backend, or the first working one for
    /// [`Backend::Auto`]. I420 needs even dimensions; an odd panel loses a
    /// row/column.
    pub fn new(width: u32, height: u32, s: EncoderSettings) -> Result<Self> {
        let (width, height) = (width & !1, height & !1);
        if s.codec == Codec::Hevc && s.backend != Backend::Vaapi {
            bail!(
                "HEVC is only encoded by the VA-API backend, not {}",
                s.backend.label()
            );
        }
        let order: &[Backend] = match s.backend {
            Backend::Auto => &[Backend::V4l2m2m, Backend::X264, Backend::Openh264],
            Backend::V4l2m2m => &[Backend::V4l2m2m],
            Backend::Vaapi => &[Backend::Vaapi],
            Backend::X264 => &[Backend::X264],
            Backend::Openh264 => &[Backend::Openh264],
        };
        let mut errors = Vec::new();
        for &b in order {
            let imp = match b {
                Backend::V4l2m2m if !v4l2m2m_present() => {
                    debug!("encoder {}: no V4L2 H.264 encoder device", b.label());
                    if s.backend == Backend::Auto {
                        continue;
                    }
                    Err(anyhow!("no V4L2 H.264 encoder device"))
                }
                Backend::V4l2m2m => {
                    ffmpeg::FfmpegEncoder::new(ffmpeg::Kind::V4l2m2m, width, height, s)
                        .map(|e| Imp::Ffmpeg(Box::new(e)))
                }
                Backend::Vaapi => ffmpeg::FfmpegEncoder::new(ffmpeg::Kind::Vaapi, width, height, s)
                    .map(|e| Imp::Ffmpeg(Box::new(e))),
                Backend::X264 => ffmpeg::FfmpegEncoder::new(ffmpeg::Kind::X264, width, height, s)
                    .map(|e| Imp::Ffmpeg(Box::new(e))),
                Backend::Openh264 => openh264::OpenH264Encoder::new(width, height, s)
                    .map(|e| Imp::OpenH264(Box::new(e))),
                Backend::Auto => unreachable!(),
            };
            match imp {
                Ok(imp) => {
                    if !errors.is_empty() {
                        info!("encoder: using {} ({})", b.label(), errors.join("; "));
                    }
                    return Ok(Self {
                        imp,
                        backend: b,
                        codec: s.codec,
                        width,
                        height,
                    });
                }
                Err(e) => {
                    debug!("encoder {} unavailable: {e:#}", b.label());
                    errors.push(format!("{} unavailable: {e:#}", b.label()));
                }
            }
        }
        bail!(
            "no {} encoder could be opened: {}",
            codec_name(s.codec),
            errors.join("; ")
        )
    }

    /// The backend that actually opened; pass it back in [`EncoderSettings`]
    /// so a resize does not probe again.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn label(&self) -> &'static str {
        match (&self.imp, self.backend, self.codec) {
            (Imp::Ffmpeg(e), Backend::Vaapi, Codec::H264) if e.gpu_convert() => {
                "VA-API (hardware), GPU colour conversion"
            }
            (Imp::Ffmpeg(_), Backend::Vaapi, Codec::H264) => {
                "VA-API (hardware), CPU colour conversion"
            }
            (Imp::Ffmpeg(e), Backend::Vaapi, Codec::Hevc) if e.gpu_convert() => {
                "VA-API (hardware) HEVC, GPU colour conversion"
            }
            (Imp::Ffmpeg(_), Backend::Vaapi, Codec::Hevc) => {
                "VA-API (hardware) HEVC, CPU colour conversion"
            }
            _ => self.backend.label(),
        }
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The 4:2:0 format this encoder takes pre-converted frames in
    /// ([`FrameBuf::Yuv`] from a [`ConvertStage`]), if any.
    pub fn yuv_format(&self) -> Option<ff::format::Pixel> {
        match &self.imp {
            // The GPU converts inside the encoder: raw frames pass the stage.
            Imp::Ffmpeg(e) if e.gpu_convert() => None,
            Imp::Ffmpeg(e) => Some(e.format()),
            // OpenH264 converts with its own coefficients and announces no
            // BT.709 colour; its frames stay unconverted.
            Imp::OpenH264(_) => None,
        }
    }

    /// Frames at least this wide are converted by the [`ConvertStage`]
    /// ahead of the encoder.
    pub fn stage_min_width(&self) -> u32 {
        match self.backend {
            Backend::Vaapi => 0,
            _ => MIN_STAGE_WIDTH,
        }
    }

    /// Encode one frame: 4-byte RGB pixels in the capture's byte order
    /// ([`crate::backend::PixelFormat`]), or already converted by a
    /// [`ConvertStage`]; a dmabuf frame is an error until capture can hand
    /// GPU buffers to the VA-API backend. Returns the pictures the encoder
    /// produced, in order: usually one, none when it skipped the frame or
    /// holds it, two when a lagging encoder catches up.
    pub fn encode(&mut self, frame: &Frame, force_idr: bool) -> Result<Vec<Encoded>> {
        match (&frame.buf, &mut self.imp) {
            (
                FrameBuf::Cpu {
                    pixels,
                    stride,
                    format,
                    ..
                },
                Imp::Ffmpeg(e),
            ) => e.encode(pixels, *stride, *format, force_idr, frame.captured_at),
            (
                FrameBuf::Cpu {
                    pixels,
                    stride,
                    format,
                    ..
                },
                Imp::OpenH264(e),
            ) => e.encode(pixels, *stride, *format, force_idr, frame.captured_at),
            (FrameBuf::Yuv(y), Imp::Ffmpeg(e)) => e.encode_yuv(y, force_idr, frame.captured_at),
            (FrameBuf::Yuv(_), _) => bail!("{} takes RGB frames only", self.label()),
            (FrameBuf::Dmabuf { .. }, _) => bail!("{} cannot encode dmabuf frames", self.label()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hevc_needs_the_vaapi_backend() {
        for backend in [
            Backend::Auto,
            Backend::V4l2m2m,
            Backend::X264,
            Backend::Openh264,
        ] {
            let s = EncoderSettings {
                backend,
                codec: Codec::Hevc,
                bitrate_bps: 2_000_000,
                max_fps: 60.0,
                threads: 1,
            };
            let e = Encoder::new(320, 240, s).err().expect("must refuse HEVC");
            assert!(
                format!("{e:#}").contains("HEVC is only encoded by the VA-API backend"),
                "{backend:?}: {e:#}"
            );
        }
    }
}

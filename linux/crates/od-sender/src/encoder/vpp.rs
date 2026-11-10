//! Colour conversion on the GPU for the VA-API encoder: the capture's 4-byte
//! RGB is uploaded into a VA surface and converted to NV12 by the driver's
//! video-processing pipeline (VPP, `VAEntrypointVideoProc`), straight into a
//! surface of the encoder's own pool. On a 2388x1668 frame that is about a
//! millisecond of CPU time, against ~15 ms for swscale, and it is the whole
//! reason the encoder stops being CPU-bound.
//!
//! The conversion must match what [`super::convert`] does on the CPU, since
//! the stream announces BT.709 limited range and the receiver converts back
//! accordingly. That is spelled out to the driver in every pipeline buffer:
//! sRGB full-range in, BT.709 reduced-range out. `None` makes the driver
//! guess by resolution, and `Explicit` without primaries is ignored by iHD.
//! A driver that still gets it wrong (not every one honours the colour
//! standards) is caught by [`GpuConvert::new`]: it converts two patches and
//! compares them against the expected values before any frame is trusted.
//!
//! libva is reached through a hand-written binding of the handful of entry
//! points used, resolved at run time with `dlopen`: the same libva that
//! FFmpeg's VA-API code uses, but a build without it (the static FFmpeg of
//! the release tarball) neither needs libva's headers nor fails to start on a
//! machine without the library. The `VADisplay` is FFmpeg's own, so the
//! encoder and the conversion share one device. The structs mirror
//! `<va/va_vpp.h>` of libva 1.x/2.x; their layout is asserted below.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

use anyhow::{Context as _, Result, anyhow, bail};
use ff::format::Pixel;
use ffmpeg_next as ff;
use tracing::debug;

use super::steps::Steps;
use crate::backend::PixelFormat;

type VAStatus = c_int;
type VAGenericID = c_uint;
type VAConfigID = VAGenericID;
type VAContextID = VAGenericID;
type VASurfaceID = VAGenericID;
type VABufferID = VAGenericID;
type VAImageID = VAGenericID;
type VADisplay = *mut c_void;

const VA_STATUS_SUCCESS: VAStatus = 0;
/// `VAProfileNone`: video processing has no codec profile.
const VA_PROFILE_NONE: c_int = -1;
const VA_ENTRYPOINT_VIDEO_PROC: c_int = 10;
const VA_PROGRESSIVE: c_int = 1;
const VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE: c_int = 41;
const VA_PROC_COLOR_STANDARD_BT709: c_int = 2;
const VA_PROC_COLOR_STANDARD_SRGB: c_int = 8;
const VA_SOURCE_RANGE_REDUCED: u8 = 1;
const VA_SOURCE_RANGE_FULL: u8 = 2;
const VA_FOURCC_BGRX: u32 = u32::from_le_bytes(*b"BGRX");
const VA_FOURCC_RGBX: u32 = u32::from_le_bytes(*b"RGBX");
const VA_FOURCC_XRGB: u32 = u32::from_le_bytes(*b"XRGB");
const VA_FOURCC_XBGR: u32 = u32::from_le_bytes(*b"XBGR");

/// `VARectangle`.
#[repr(C)]
#[derive(Clone, Copy)]
struct VaRectangle {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

/// `VAImageFormat`.
#[repr(C)]
#[derive(Clone, Copy)]
struct VaImageFormat {
    fourcc: u32,
    byte_order: u32,
    bits_per_pixel: u32,
    depth: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    va_reserved: [u32; 4],
}

/// `VAImage`: a CPU view of a surface, here one from `vaDeriveImage`.
#[repr(C)]
#[derive(Clone, Copy)]
struct VaImage {
    image_id: VAImageID,
    format: VaImageFormat,
    buf: VABufferID,
    width: u16,
    height: u16,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 3],
    offsets: [u32; 3],
    num_palette_entries: i32,
    entry_bytes: i32,
    component_order: [i8; 4],
    va_reserved: [u32; 4],
}

/// `VAProcColorProperties`.
#[repr(C)]
#[derive(Clone, Copy)]
struct VaProcColorProperties {
    chroma_sample_location: u8,
    color_range: u8,
    colour_primaries: u8,
    transfer_characteristics: u8,
    matrix_coefficients: u8,
    reserved: [u8; 3],
}

/// `VAProcPipelineParameterBuffer`, every field kept so the size is the
/// driver's. Only the ones set in [`GpuConvert::run`] are used; the rest
/// stay zero.
#[repr(C)]
struct VaProcPipelineParameterBuffer {
    surface: VASurfaceID,
    surface_region: *const VaRectangle,
    surface_color_standard: c_int,
    output_region: *const VaRectangle,
    output_background_color: u32,
    output_color_standard: c_int,
    pipeline_flags: u32,
    filter_flags: u32,
    filters: *mut VABufferID,
    num_filters: u32,
    forward_references: *mut VASurfaceID,
    num_forward_references: u32,
    backward_references: *mut VASurfaceID,
    num_backward_references: u32,
    rotation_state: u32,
    blend_state: *const c_void,
    mirror_state: u32,
    additional_outputs: *mut VASurfaceID,
    num_additional_outputs: u32,
    input_surface_flag: u32,
    output_surface_flag: u32,
    input_color_properties: VaProcColorProperties,
    output_color_properties: VaProcColorProperties,
    processing_mode: c_int,
    output_hdr_metadata: *mut c_void,
    va_reserved: [u32; PIPELINE_RESERVED],
}

/// `va_vpp.h` pads the pipeline buffer to the same total on every ABI:
/// `VA_PADDING_LARGE - 16` words where pointers are 64-bit, `- 13` elsewhere.
const PIPELINE_RESERVED: usize = if cfg!(target_pointer_width = "64") {
    16
} else {
    19
};

// Offsets and sizes as printed by a C compiler against libva's headers
// (x86_64 and aarch64 agree). A mismatch fails the build, not a frame. Only
// 64-bit targets are built and checked; the 32-bit padding above follows the
// header but has not been compared against a 32-bit compiler.
#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<VaRectangle>() == 8);
    assert!(size_of::<VaImageFormat>() == 48);
    assert!(offset_of!(VaImageFormat, bits_per_pixel) == 8);
    assert!(offset_of!(VaImageFormat, va_reserved) == 32);
    assert!(size_of::<VaImage>() == 120);
    assert!(offset_of!(VaImage, buf) == 52);
    assert!(offset_of!(VaImage, width) == 56);
    assert!(offset_of!(VaImage, data_size) == 60);
    assert!(offset_of!(VaImage, pitches) == 68);
    assert!(offset_of!(VaImage, offsets) == 80);
    assert!(offset_of!(VaImage, num_planes) == 64);
    assert!(offset_of!(VaImage, component_order) == 100);
    assert!(offset_of!(VaRectangle, width) == 4);
    assert!(size_of::<VaProcColorProperties>() == 8);
    assert!(offset_of!(VaProcColorProperties, color_range) == 1);
    assert!(offset_of!(VaProcColorProperties, matrix_coefficients) == 4);
    assert!(size_of::<VaProcPipelineParameterBuffer>() == 224);
    assert!(offset_of!(VaProcPipelineParameterBuffer, surface_region) == 8);
    assert!(offset_of!(VaProcPipelineParameterBuffer, surface_color_standard) == 16);
    assert!(offset_of!(VaProcPipelineParameterBuffer, output_region) == 24);
    assert!(offset_of!(VaProcPipelineParameterBuffer, output_color_standard) == 36);
    assert!(offset_of!(VaProcPipelineParameterBuffer, filters) == 48);
    assert!(offset_of!(VaProcPipelineParameterBuffer, blend_state) == 96);
    assert!(offset_of!(VaProcPipelineParameterBuffer, additional_outputs) == 112);
    assert!(offset_of!(VaProcPipelineParameterBuffer, input_color_properties) == 132);
    assert!(offset_of!(VaProcPipelineParameterBuffer, output_color_properties) == 140);
    assert!(offset_of!(VaProcPipelineParameterBuffer, processing_mode) == 148);
    assert!(offset_of!(VaProcPipelineParameterBuffer, output_hdr_metadata) == 152);
    assert!(offset_of!(VaProcPipelineParameterBuffer, va_reserved) == 160);
};

// glibc before 2.34 keeps dlopen in libdl; later versions ship an empty one.
// SAFETY: these are the signatures `<dlfcn.h>` declares for both functions.
#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_NOW: c_int = 2;

/// Declares the libva entry points: a table of function pointers resolved
/// by name from the opened library.
macro_rules! libva {
    ($( fn $name:ident($($arg:ty),*) -> $ret:ty; )*) => {
        #[allow(non_snake_case)]
        struct Va {
            $( $name: unsafe extern "C" fn($($arg),*) -> $ret, )*
        }

        impl Va {
            /// # Safety
            /// `lib` is a handle from `dlopen` of libva, which is never closed.
            unsafe fn resolve(lib: *mut c_void) -> Result<Self> {
                Ok(Self {
                    $( $name: {
                        let sym = concat!(stringify!($name), "\0");
                        // SAFETY: `sym` is NUL-terminated; the declared
                        // signature is the one libva documents for the name.
                        let p = unsafe { dlsym(lib, sym.as_ptr().cast()) };
                        if p.is_null() {
                            bail!("libva has no {}", stringify!($name));
                        }
                        // SAFETY: a non-null symbol address is a function
                        // pointer of the declared type.
                        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) -> $ret>(p) }
                    }, )*
                })
            }
        }
    };
}

libva! {
    fn vaCreateConfig(VADisplay, c_int, c_int, *mut c_void, c_int, *mut VAConfigID) -> VAStatus;
    fn vaDestroyConfig(VADisplay, VAConfigID) -> VAStatus;
    fn vaCreateContext(VADisplay, VAConfigID, c_int, c_int, c_int, *mut VASurfaceID, c_int, *mut VAContextID) -> VAStatus;
    fn vaDestroyContext(VADisplay, VAContextID) -> VAStatus;
    fn vaCreateBuffer(VADisplay, VAContextID, c_int, c_uint, c_uint, *mut c_void, *mut VABufferID) -> VAStatus;
    fn vaDestroyBuffer(VADisplay, VABufferID) -> VAStatus;
    fn vaBeginPicture(VADisplay, VAContextID, VASurfaceID) -> VAStatus;
    fn vaRenderPicture(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus;
    fn vaEndPicture(VADisplay, VAContextID) -> VAStatus;
    fn vaDeriveImage(VADisplay, VASurfaceID, *mut VaImage) -> VAStatus;
    fn vaDestroyImage(VADisplay, VAImageID) -> VAStatus;
    fn vaMapBuffer(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus;
    fn vaUnmapBuffer(VADisplay, VABufferID) -> VAStatus;
    fn vaErrorStr(VAStatus) -> *const c_char;
}

/// libva, loaded once; the error is kept so every encoder open reports it.
fn va() -> Result<&'static Va> {
    static VA: OnceLock<std::result::Result<Va, String>> = OnceLock::new();
    VA.get_or_init(|| {
        // SAFETY: a NUL-terminated name; the handle is never closed.
        let lib = unsafe { dlopen(c"libva.so.2".as_ptr(), RTLD_NOW) };
        if lib.is_null() {
            return Err("libva.so.2 is not installed".to_string());
        }
        // SAFETY: `lib` is libva's handle.
        unsafe { Va::resolve(lib) }.map_err(|e| format!("{e:#}"))
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

impl Va {
    fn check(&self, what: &str, status: VAStatus) -> Result<()> {
        if status == VA_STATUS_SUCCESS {
            return Ok(());
        }
        // SAFETY: vaErrorStr returns a static NUL-terminated string.
        let msg = unsafe { CStr::from_ptr((self.vaErrorStr)(status)) };
        bail!("{what}: {}", msg.to_string_lossy())
    }
}

/// The FFmpeg pixel format whose bytes are the capture's. The fourth byte is
/// padding, also for the `A` formats, as on the CPU path: a compositor may
/// leave anything there and it must not blend. FFmpeg maps these to the VA
/// fourccs BGRX and RGBX.
fn source_pixel(f: PixelFormat) -> Pixel {
    match f {
        PixelFormat::Xrgb8888 | PixelFormat::Argb8888 => Pixel::BGRZ,
        PixelFormat::Xbgr8888 | PixelFormat::Abgr8888 => Pixel::RGBZ,
    }
}

/// A pool of VA surfaces on `device`. 0 initial surfaces: it grows on
/// demand, so a slow encoder holding some never starves the next frame.
pub(super) fn new_pool(
    device: *mut ff::ffi::AVBufferRef,
    sw_format: ff::ffi::AVPixelFormat,
    width: u32,
    height: u32,
) -> Result<*mut ff::ffi::AVBufferRef> {
    use ff::ffi::*;
    // SAFETY: `device` is a valid device reference; the new context is
    // unreffed on every failure path.
    unsafe {
        let mut frames = av_hwframe_ctx_alloc(device);
        if frames.is_null() {
            bail!("av_hwframe_ctx_alloc failed");
        }
        let fc = (*frames).data as *mut AVHWFramesContext;
        (*fc).format = AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*fc).sw_format = sw_format;
        (*fc).width = width as i32;
        (*fc).height = height as i32;
        (*fc).initial_pool_size = 0;
        let err = av_hwframe_ctx_init(frames);
        if err < 0 {
            av_buffer_unref(&mut frames);
            bail!("VA-API surface pool: {}", ff::Error::from(err));
        }
        Ok(frames)
    }
}

/// One surface from `frames`, as an `AVFrame` that gives it back on drop.
pub(super) fn hw_surface(frames: *mut ff::ffi::AVBufferRef) -> Result<ff::frame::Video> {
    let mut hw = ff::frame::Video::empty();
    // SAFETY: `hw` is an empty frame we own and `frames` is a valid pool.
    let err = unsafe { ff::ffi::av_hwframe_get_buffer(frames, hw.as_mut_ptr(), 0) };
    if err < 0 {
        bail!("VA-API surface: {}", ff::Error::from(err));
    }
    Ok(hw)
}

/// The surface behind a VA-API frame (`data[3]`).
fn surface_id(frame: &ff::frame::Video) -> VASurfaceID {
    // SAFETY: reading a plain field of a valid frame.
    unsafe { (*frame.as_ptr()).data[3] as usize as VASurfaceID }
}

/// A VPP context producing NV12 surfaces of one size from RGB ones.
pub struct GpuConvert {
    va: &'static Va,
    display: VADisplay,
    config: VAConfigID,
    context: VAContextID,
    device: *mut ff::ffi::AVBufferRef,
    width: u32,
    height: u32,
    /// Upload surfaces by capture byte order, made on first use.
    sources: Vec<(Pixel, *mut ff::ffi::AVBufferRef)>,
    /// Write source surfaces through `vaDeriveImage` ([`Self::upload_direct`]);
    /// cleared if the driver cannot, and FFmpeg's transfer is used instead.
    direct_upload: bool,
}

// SAFETY: the VA ids and FFmpeg references are only used from the thread that
// owns the encoder; moving them between threads is fine.
unsafe impl Send for GpuConvert {}

impl GpuConvert {
    /// Set up conversion into surfaces of `dst_frames` (the encoder's NV12
    /// pool, on `device`), then check the driver gets the colours right.
    pub fn new(
        device: *mut ff::ffi::AVBufferRef,
        dst_frames: *mut ff::ffi::AVBufferRef,
    ) -> Result<Self> {
        use ff::ffi::*;
        let va = va()?;
        // SAFETY: both are valid buffers of the contexts they claim.
        let (display, width, height) = unsafe {
            let dev = (*device).data as *mut AVHWDeviceContext;
            let vaapi = (*dev).hwctx as *mut AVVAAPIDeviceContext;
            let fc = (*dst_frames).data as *mut AVHWFramesContext;
            ((*vaapi).display, (*fc).width as u32, (*fc).height as u32)
        };
        if display.is_null()
            || width == 0
            || height == 0
            || width > u16::MAX as u32
            || height > u16::MAX as u32
        {
            bail!("no VA display or a frame of {width}x{height}");
        }
        let mut config = 0;
        // SAFETY: valid display and out-pointer.
        let status = unsafe {
            (va.vaCreateConfig)(
                display,
                VA_PROFILE_NONE,
                VA_ENTRYPOINT_VIDEO_PROC,
                std::ptr::null_mut(),
                0,
                &mut config,
            )
        };
        va.check("no video processing on this device", status)?;
        let mut context = 0;
        // SAFETY: as above; no render-target hint is given.
        let status = unsafe {
            (va.vaCreateContext)(
                display,
                config,
                width as c_int,
                height as c_int,
                VA_PROGRESSIVE,
                std::ptr::null_mut(),
                0,
                &mut context,
            )
        };
        if let Err(e) = va.check("VPP context", status) {
            // SAFETY: `config` was created above.
            unsafe { (va.vaDestroyConfig)(display, config) };
            return Err(e);
        }
        // SAFETY: the reference keeps the display alive for our lifetime.
        let device = unsafe { av_buffer_ref(device) };
        if device.is_null() {
            // SAFETY: both were created above.
            unsafe {
                (va.vaDestroyContext)(display, context);
                (va.vaDestroyConfig)(display, config);
            }
            bail!("av_buffer_ref failed");
        }
        // From here Drop releases everything.
        let mut gpu = Self {
            va,
            display,
            config,
            context,
            device,
            width,
            height,
            sources: Vec::new(),
            direct_upload: true,
        };
        if let Err(e) = gpu.verify(dst_frames) {
            // A driver whose surface mapping is not what it says gets the
            // colours right through FFmpeg's upload, or not at all.
            debug!("VA-API: colour check with mapped uploads failed: {e:#}");
            gpu.direct_upload = false;
            gpu.verify(dst_frames)
                .context("the driver's colour conversion is not BT.709 limited range")?;
        }
        Ok(gpu)
    }

    /// Convert one frame of capture pixels (`stride` bytes per row, at least
    /// `width` x `height` visible) into `dst`, a surface of the NV12 pool.
    /// `pixels` may be released on return; the conversion itself is only
    /// queued. Work on `dst` (encoding it, mapping it) is ordered after it
    /// by the driver, so nothing waits here. The returned source surface may
    /// still be read by the GPU: keep it until `dst` has been consumed, or
    /// the pool could hand it out for the next upload too soon.
    pub fn convert(
        &mut self,
        dst: &ff::frame::Video,
        pixels: &[u8],
        stride: u32,
        format: PixelFormat,
        steps: &mut Steps,
    ) -> Result<ff::frame::Video> {
        let (w, h, stride) = (self.width as usize, self.height as usize, stride as usize);
        if stride < w * 4 || pixels.len() < stride * (h - 1) + w * 4 {
            bail!(
                "{} bytes at stride {stride} is not a {w}x{h} frame",
                pixels.len()
            );
        }
        let pix = source_pixel(format);
        let pool = self.source_pool(pix)?;
        let src = hw_surface(pool)?;
        if self.direct_upload {
            if let Err(e) = self.upload_direct(surface_id(&src), pix, pixels, stride) {
                debug!(
                    "VA-API: mapping the source surface failed, uploading through FFmpeg: {e:#}"
                );
                self.direct_upload = false;
            }
        }
        if !self.direct_upload {
            upload_ffmpeg(&src, pix, w, h, pixels, stride)?;
        }
        steps.lap("upload");
        self.run(surface_id(&src), surface_id(dst))?;
        steps.lap("vpp submit");
        Ok(src)
    }

    /// Copy `pixels` into the source surface `surface` through a CPU mapping
    /// of the surface itself: about 2.5 ms for 2388x1668 on iHD, where
    /// FFmpeg's `av_hwframe_transfer_data` took 5-7 ms. Fails, changing
    /// nothing, if the driver cannot derive an image of the surface or
    /// derives one of another layout.
    fn upload_direct(
        &self,
        surface: VASurfaceID,
        pix: Pixel,
        pixels: &[u8],
        stride: usize,
    ) -> Result<()> {
        let va = self.va;
        let (w, h) = (self.width as usize, self.height as usize);
        // SAFETY: all-zero is a valid value: integers only.
        let mut image: VaImage = unsafe { std::mem::zeroed() };
        // SAFETY: a valid surface on our display and an out-pointer.
        let status = unsafe { (va.vaDeriveImage)(self.display, surface, &mut image) };
        va.check("vaDeriveImage", status)?;
        // libva names these by byte order; iHD names the derived image by
        // the order within a little-endian word (D3D's X8R8G8B8), so a BGRX
        // surface comes back as "XRGB". Same bytes either way, and `new`
        // checks the colours through this very path.
        let want = if pix == Pixel::RGBZ {
            [VA_FOURCC_RGBX, VA_FOURCC_XBGR]
        } else {
            [VA_FOURCC_BGRX, VA_FOURCC_XRGB]
        };
        let pitch = image.pitches[0] as usize;
        let offset = image.offsets[0] as usize;
        let result = if !want.contains(&image.format.fourcc)
            || (image.width as usize) < w
            || (image.height as usize) < h
            || pitch < w * 4
            || (image.data_size as usize) < offset + pitch * (h - 1) + w * 4
        {
            Err(anyhow!(
                "derived image is {:?} {}x{} pitch {pitch}",
                image.format.fourcc.to_le_bytes().map(char::from),
                image.width,
                image.height
            ))
        } else {
            let mut mapped = std::ptr::null_mut();
            // SAFETY: the image's buffer, mapped once and unmapped below.
            let status = unsafe { (va.vaMapBuffer)(self.display, image.buf, &mut mapped) };
            va.check("vaMapBuffer", status).and_then(|()| {
                let base = mapped.cast::<u8>();
                for (y, row) in pixels.chunks(stride).take(h).enumerate() {
                    // SAFETY: the mapping holds `data_size` bytes, which was
                    // checked to cover `h` rows of `pitch` from `offset`;
                    // `row` has at least `w * 4` bytes (checked by
                    // `convert`); the two do not overlap.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            row.as_ptr(),
                            base.add(offset + y * pitch),
                            w * 4,
                        )
                    };
                }
                // SAFETY: mapped above.
                let status = unsafe { (va.vaUnmapBuffer)(self.display, image.buf) };
                va.check("vaUnmapBuffer", status)
            })
        };
        // SAFETY: derived above, destroyed once; the surface stays.
        unsafe { (va.vaDestroyImage)(self.display, image.image_id) };
        result
    }

    /// Whether uploads go through a mapping of the surface.
    #[cfg(test)]
    pub(super) fn direct_upload(&self) -> bool {
        self.direct_upload
    }

    /// Force FFmpeg's upload (`false`), or the mapping where it works.
    #[cfg(test)]
    pub(super) fn set_direct_upload(&mut self, on: bool) {
        self.direct_upload = on;
    }

    fn source_pool(&mut self, pix: Pixel) -> Result<*mut ff::ffi::AVBufferRef> {
        if let Some(&(_, p)) = self.sources.iter().find(|(f, _)| *f == pix) {
            return Ok(p);
        }
        let pool = new_pool(self.device, pix.into(), self.width, self.height)
            .with_context(|| format!("{pix:?} surfaces"))?;
        self.sources.push((pix, pool));
        Ok(pool)
    }

    /// Queue rendering `src` into `dst`.
    fn run(&self, src: VASurfaceID, dst: VASurfaceID) -> Result<()> {
        let va = self.va;
        let region = VaRectangle {
            x: 0,
            y: 0,
            width: self.width as u16,
            height: self.height as u16,
        };
        // SAFETY: all-zero is a valid value: integers, null pointers.
        let mut pp: VaProcPipelineParameterBuffer = unsafe { std::mem::zeroed() };
        pp.surface = src;
        pp.surface_region = &region;
        pp.output_region = &region;
        pp.surface_color_standard = VA_PROC_COLOR_STANDARD_SRGB;
        pp.input_color_properties.color_range = VA_SOURCE_RANGE_FULL;
        pp.output_color_standard = VA_PROC_COLOR_STANDARD_BT709;
        pp.output_color_properties.color_range = VA_SOURCE_RANGE_REDUCED;
        let mut buf = 0;
        // SAFETY: `pp` is a complete parameter buffer of the size given, and
        // libva copies it; `region` outlives the call that reads it.
        let status = unsafe {
            (va.vaCreateBuffer)(
                self.display,
                self.context,
                VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE,
                std::mem::size_of::<VaProcPipelineParameterBuffer>() as c_uint,
                1,
                (&mut pp as *mut VaProcPipelineParameterBuffer).cast(),
                &mut buf,
            )
        };
        va.check("vaCreateBuffer", status)?;
        // SAFETY: valid ids on our display and context; the picture is ended
        // once begun and the buffer destroyed whatever happened.
        let result = unsafe {
            let begun = (va.vaBeginPicture)(self.display, self.context, dst);
            va.check("vaBeginPicture", begun).and_then(|()| {
                let rendered = (va.vaRenderPicture)(self.display, self.context, &mut buf, 1);
                let ended = (va.vaEndPicture)(self.display, self.context);
                va.check("vaRenderPicture", rendered)
                    .and_then(|()| va.check("vaEndPicture", ended))
            })
        };
        // SAFETY: `buf` was created above.
        unsafe { (va.vaDestroyBuffer)(self.display, buf) };
        result
    }

    /// Convert a red and a blue patch in both byte orders and compare the
    /// result with swscale's BT.709 limited-range values.
    fn verify(&mut self, dst_frames: *mut ff::ffi::AVBufferRef) -> Result<()> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut px = vec![0u8; w * h * 4];
        // Bytes of (red, blue) in memory, per byte order.
        for (format, red, blue) in [
            (PixelFormat::Xrgb8888, [0, 0, 255, 0], [255, 0, 0, 0]),
            (PixelFormat::Xbgr8888, [255, 0, 0, 0], [0, 0, 255, 0]),
        ] {
            for (y, row) in px.chunks_exact_mut(w * 4).enumerate() {
                let c = if y < h / 2 { red } else { blue };
                row.chunks_exact_mut(4).for_each(|p| p.copy_from_slice(&c));
            }
            let dst = hw_surface(dst_frames)?;
            // Held until the read back, which waits for the conversion.
            let _source =
                self.convert(&dst, &px, (w * 4) as u32, format, &mut Steps::new("verify"))?;
            let nv12 = download(&dst)?;
            for (at, want, name) in [
                (h / 4, [63, 102, 240], "red"),
                (h * 3 / 4, [32, 240, 118], "blue"),
            ] {
                let got = sample(&nv12, w / 2, at);
                if got.iter().zip(want).any(|(g, w)| g.abs_diff(w) > 2) {
                    bail!("{name} came out as {got:?} {format:?}, expected {want:?}");
                }
            }
        }
        Ok(())
    }
}

impl Drop for GpuConvert {
    fn drop(&mut self) {
        // SAFETY: each id and reference was created in `new` or
        // `source_pool` and is released once.
        unsafe {
            (self.va.vaDestroyContext)(self.display, self.context);
            (self.va.vaDestroyConfig)(self.display, self.config);
            for (_, mut p) in self.sources.drain(..) {
                ff::ffi::av_buffer_unref(&mut p);
            }
            ff::ffi::av_buffer_unref(&mut self.device);
        }
    }
}

/// Copy `pixels` into the VA surface `src` with FFmpeg's generic transfer.
fn upload_ffmpeg(
    src: &ff::frame::Video,
    pix: Pixel,
    w: usize,
    h: usize,
    pixels: &[u8],
    stride: usize,
) -> Result<()> {
    let mut wrapper = ff::frame::Video::empty();
    // SAFETY: the wrapper borrows `pixels` without owning it (no buffer
    // reference, so dropping it frees nothing) and is only read by the
    // transfer below, which returns before `pixels` can go away.
    let err = unsafe {
        let f = wrapper.as_mut_ptr();
        (*f).format = ff::ffi::AVPixelFormat::from(pix) as c_int;
        (*f).width = w as c_int;
        (*f).height = h as c_int;
        (*f).data[0] = pixels.as_ptr().cast_mut();
        (*f).linesize[0] = stride as c_int;
        ff::ffi::av_hwframe_transfer_data(src.as_ptr().cast_mut(), wrapper.as_ptr(), 0)
    };
    if err < 0 {
        bail!("uploading to a VA surface: {}", ff::Error::from(err));
    }
    Ok(())
}

/// An NV12 surface copied into system memory.
pub(super) fn download(hw: &ff::frame::Video) -> Result<ff::frame::Video> {
    let mut sw = ff::frame::Video::new(Pixel::NV12, hw.width(), hw.height());
    // SAFETY: both frames are valid; `sw` has buffers of the surface's size.
    let err = unsafe { ff::ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), hw.as_ptr(), 0) };
    if err < 0 {
        bail!("reading back a VA surface: {}", ff::Error::from(err));
    }
    Ok(sw)
}

/// Y, U and V at luma position (`x`, `y`) of a system-memory NV12 frame.
pub(super) fn sample(nv12: &ff::frame::Video, x: usize, y: usize) -> [u8; 3] {
    let uv = &nv12.data(1)[(y / 2) * nv12.stride(1) + (x / 2) * 2..];
    [nv12.data(0)[y * nv12.stride(0) + x], uv[0], uv[1]]
}

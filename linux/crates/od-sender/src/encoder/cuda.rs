//! RGB to NV12 on an NVIDIA GPU for the NVENC encoder, with BT.709
//! limited-range coefficients. NVENC converts RGB input itself, but always
//! with BT.601 whatever the stream signals, and the receivers render BT.709.
//!
//! The capture's pixels are copied into GPU memory and converted by a small
//! kernel (`cuda/nv12.cu`, embedded as PTX that the driver compiles at load)
//! straight into a CUDA frame of the encoder's pool, so the CPU does no
//! colour conversion. libcuda is loaded at run time, like libva: a machine
//! without the NVIDIA driver still runs, without NVENC.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};
use ffmpeg_next as ff;

use crate::backend::PixelFormat;
use crate::dl::{RTLD_NOW, dlopen, dlsym};

type CUresult = c_int;
type CUcontext = *mut c_void;
type CUstream = *mut c_void;
type CUmodule = *mut c_void;
type CUfunction = *mut c_void;
type CUdeviceptr = u64;

const CU_MEMORYTYPE_HOST: c_uint = 1;
const CU_MEMORYTYPE_DEVICE: c_uint = 2;

/// `CUDA_MEMCPY2D` from `cuda.h`.
#[repr(C)]
struct Memcpy2d {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: c_uint,
    src_host: *const c_void,
    src_device: CUdeviceptr,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: c_uint,
    dst_host: *mut c_void,
    dst_device: CUdeviceptr,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

impl Memcpy2d {
    fn zeroed() -> Self {
        // SAFETY: integers and raw pointers, for which all-zero is valid.
        unsafe { std::mem::zeroed() }
    }
}

macro_rules! libcuda {
    ($( fn $name:ident = $sym:literal ( $($arg:ty),* ) -> $ret:ty; )*) => {
        #[allow(non_snake_case)]
        struct Cu {
            $( $name: unsafe extern "C" fn($($arg),*) -> $ret, )*
        }

        impl Cu {
            /// # Safety
            /// `lib` is a handle from `dlopen` of libcuda, which is never closed.
            unsafe fn resolve(lib: *mut c_void) -> Result<Self> {
                Ok(Self {
                    $( $name: {
                        let sym = concat!($sym, "\0");
                        // SAFETY: `sym` is NUL-terminated; the declared
                        // signature is the one cuda.h gives the symbol.
                        let p = unsafe { dlsym(lib, sym.as_ptr().cast()) };
                        if p.is_null() {
                            bail!("libcuda has no {}", $sym);
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

libcuda! {
    fn ctx_push = "cuCtxPushCurrent_v2"(CUcontext) -> CUresult;
    fn ctx_pop = "cuCtxPopCurrent_v2"(*mut CUcontext) -> CUresult;
    fn module_load_data = "cuModuleLoadData"(*mut CUmodule, *const c_void) -> CUresult;
    fn module_unload = "cuModuleUnload"(CUmodule) -> CUresult;
    fn module_get_function = "cuModuleGetFunction"(*mut CUfunction, CUmodule, *const c_char) -> CUresult;
    fn mem_alloc_pitch = "cuMemAllocPitch_v2"(*mut CUdeviceptr, *mut usize, usize, usize, c_uint) -> CUresult;
    fn mem_free = "cuMemFree_v2"(CUdeviceptr) -> CUresult;
    fn memcpy_2d_async = "cuMemcpy2DAsync_v2"(*const Memcpy2d, CUstream) -> CUresult;
    fn launch_kernel = "cuLaunchKernel"(CUfunction, c_uint, c_uint, c_uint, c_uint, c_uint, c_uint, c_uint, CUstream, *mut *mut c_void, *mut *mut c_void) -> CUresult;
    fn stream_synchronize = "cuStreamSynchronize"(CUstream) -> CUresult;

    fn get_error_string = "cuGetErrorString"(CUresult, *mut *const c_char) -> CUresult;
}

fn cu() -> Result<&'static Cu> {
    static CU: OnceLock<Result<Cu, String>> = OnceLock::new();
    CU.get_or_init(|| {
        // SAFETY: a NUL-terminated name; the handle is never closed.
        let lib = unsafe { dlopen(c"libcuda.so.1".as_ptr(), RTLD_NOW) };
        if lib.is_null() {
            return Err("libcuda.so.1 not found".into());
        }
        // SAFETY: `lib` is libcuda, kept open for the process.
        unsafe { Cu::resolve(lib) }.map_err(|e| format!("{e:#}"))
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

/// `Err` naming the call and CUDA's description of a failed `CUresult`.
fn check(cu: &Cu, what: &str, r: CUresult) -> Result<()> {
    if r == 0 {
        return Ok(());
    }
    let mut s: *const c_char = std::ptr::null();
    // SAFETY: an out-pointer to a static string, or left null.
    unsafe { (cu.get_error_string)(r, &mut s) };
    let msg = if s.is_null() {
        format!("error {r}")
    } else {
        // SAFETY: CUDA returned a NUL-terminated static string.
        unsafe { std::ffi::CStr::from_ptr(s) }
            .to_string_lossy()
            .into_owned()
    };
    bail!("{what}: {msg}")
}

/// The leading fields of FFmpeg's `AVCUDADeviceContext`.
#[repr(C)]
struct CudaDeviceContext {
    cuda_ctx: CUcontext,
    stream: CUstream,
}

/// The PTX of `cuda/nv12.cu`, NUL-terminated for `cuModuleLoadData`.
const PTX: &str = concat!(include_str!("cuda/nv12.ptx"), "\0");

/// Converts RGB frames in system memory into NV12 CUDA frames.
pub struct CudaConvert {
    cu: &'static Cu,
    /// Our own reference to the FFmpeg CUDA device that owns `ctx`, so the
    /// context outlives the module and buffer we free in `Drop` whatever
    /// order the encoder's fields drop in.
    device: *mut ff::ffi::AVBufferRef,
    ctx: CUcontext,
    stream: CUstream,
    module: CUmodule,
    kernel: CUfunction,
    /// GPU copy of the capture: pointer, pitch, and the size it was made for.
    src: Option<(CUdeviceptr, usize, u32, u32)>,
}

// SAFETY: the CUDA handles are used only from the thread that owns the
// encoder; every call makes the context current first.
unsafe impl Send for CudaConvert {}

/// Pushes a CUDA context for the scope.
struct Current<'a>(&'a Cu);

impl<'a> Current<'a> {
    fn push(cu: &'a Cu, ctx: CUcontext) -> Result<Self> {
        // SAFETY: `ctx` is the device's live context.
        check(cu, "cuCtxPushCurrent", unsafe { (cu.ctx_push)(ctx) })?;
        Ok(Self(cu))
    }
}

impl Drop for Current<'_> {
    fn drop(&mut self) {
        let mut old = std::ptr::null_mut();
        // SAFETY: pops the context pushed in `push`.
        unsafe { (self.0.ctx_pop)(&mut old) };
    }
}

impl CudaConvert {
    /// On the CUDA device behind `device` (an FFmpeg CUDA device reference).
    pub fn new(device: *mut ff::ffi::AVBufferRef) -> Result<Self> {
        let cu = cu()?;
        // SAFETY: `device` is a CUDA AVHWDeviceContext, whose hwctx is an
        // AVCUDADeviceContext starting with the context and the stream.
        let (ctx, stream) = unsafe {
            let dc = (*device).data as *mut ff::ffi::AVHWDeviceContext;
            let hw = (*dc).hwctx as *const CudaDeviceContext;
            ((*hw).cuda_ctx, (*hw).stream)
        };
        // SAFETY: `device` is a live buffer reference; a new reference to it.
        let device = unsafe { ff::ffi::av_buffer_ref(device) };
        if device.is_null() {
            bail!("av_buffer_ref of the CUDA device failed");
        }
        let unref = |mut d: *mut ff::ffi::AVBufferRef| {
            // SAFETY: the reference taken above, released once.
            unsafe { ff::ffi::av_buffer_unref(&mut d) }
        };
        let _cur = match Current::push(cu, ctx) {
            Ok(c) => c,
            Err(e) => {
                unref(device);
                return Err(e);
            }
        };
        let mut module = std::ptr::null_mut();
        // SAFETY: NUL-terminated PTX; out-pointer valid.
        if let Err(e) = check(cu, "cuModuleLoadData", unsafe {
            (cu.module_load_data)(&mut module, PTX.as_ptr().cast())
        }) {
            unref(device);
            return Err(e);
        }
        let mut kernel = std::ptr::null_mut();
        // SAFETY: `module` loaded above; NUL-terminated name.
        if let Err(e) = check(cu, "cuModuleGetFunction", unsafe {
            (cu.module_get_function)(&mut kernel, module, c"rgb_to_nv12".as_ptr())
        }) {
            // SAFETY: unloading the module we loaded.
            unsafe { (cu.module_unload)(module) };
            unref(device);
            return Err(e);
        }
        Ok(Self {
            cu,
            device,
            ctx,
            stream,
            module,
            kernel,
            src: None,
        })
    }

    /// Convert `pixels` (`stride` bytes per row, in `format`'s byte order)
    /// into `dst`, a CUDA NV12 frame of the encoder's pool, cropping to its
    /// size. Returns once the GPU has finished reading `pixels` and writing
    /// `dst`.
    pub fn convert(
        &mut self,
        dst: &ff::frame::Video,
        pixels: &[u8],
        stride: u32,
        format: PixelFormat,
    ) -> Result<()> {
        let cu = self.cu;
        let (w, h) = (dst.width(), dst.height());
        // The kernel converts 2x2 blocks: an odd size would read past the
        // capture and write a luma row into the chroma plane.
        if w % 2 != 0 || h % 2 != 0 {
            bail!("CUDA conversion needs an even size, not {w}x{h}");
        }
        let rows = pixels.len() / stride.max(1) as usize;
        if (stride as usize) < w as usize * 4 || rows < h as usize {
            bail!(
                "a {}-byte, {rows}-row capture cannot fill a {w}x{h} frame",
                stride
            );
        }
        let _cur = Current::push(cu, self.ctx)?;
        let (src, src_pitch) = self.source(w, h)?;
        let copy = Memcpy2d {
            src_memory_type: CU_MEMORYTYPE_HOST,
            src_host: pixels.as_ptr().cast(),
            src_pitch: stride as usize,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_device: src,
            dst_pitch: src_pitch,
            width_in_bytes: w as usize * 4,
            height: h as usize,
            ..Memcpy2d::zeroed()
        };
        // SAFETY: host rows within `pixels` (checked above); device buffer
        // allocated for `w`x`h` at `src_pitch`.
        check(cu, "cuMemcpy2DAsync", unsafe {
            (cu.memcpy_2d_async)(&copy, self.stream)
        })?;
        // SAFETY: reading plain fields of a valid CUDA frame: data[0] and
        // data[1] are device pointers to the luma and chroma planes.
        let (mut y, mut uv, mut y_pitch, mut uv_pitch) = unsafe {
            let f = &*dst.as_ptr();
            (
                f.data[0] as CUdeviceptr,
                f.data[1] as CUdeviceptr,
                f.linesize[0] as u32,
                f.linesize[1] as u32,
            )
        };
        let (mut src_arg, mut src_pitch_arg) = (src, src_pitch as u32);
        let (mut w_arg, mut h_arg) = (w, h);
        let mut bgr: u32 = match format {
            PixelFormat::Xrgb8888 | PixelFormat::Argb8888 => 1,
            PixelFormat::Xbgr8888 | PixelFormat::Abgr8888 => 0,
        };
        let mut args: [*mut c_void; 9] = [
            (&raw mut src_arg).cast(),
            (&raw mut src_pitch_arg).cast(),
            (&raw mut y).cast(),
            (&raw mut y_pitch).cast(),
            (&raw mut uv).cast(),
            (&raw mut uv_pitch).cast(),
            (&raw mut w_arg).cast(),
            (&raw mut h_arg).cast(),
            (&raw mut bgr).cast(),
        ];
        let (bx, by) = (32u32, 8u32);
        let (gx, gy) = ((w / 2).div_ceil(bx), (h / 2).div_ceil(by));
        // SAFETY: the arguments match the kernel's parameters in order and
        // type; every pointer is valid for the whole frame.
        check(cu, "cuLaunchKernel", unsafe {
            (cu.launch_kernel)(
                self.kernel,
                gx,
                gy,
                1,
                bx,
                by,
                1,
                0,
                self.stream,
                args.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        })?;
        // The capture buffer goes back to the compositor when the caller is
        // done with it: the GPU must have read it by then. (The copy is from
        // pageable memory: CUDA cannot pin the compositor's read-only shm
        // mappings, cuMemHostRegister says not supported.)
        // SAFETY: a stream of our context.
        check(cu, "cuStreamSynchronize", unsafe {
            (cu.stream_synchronize)(self.stream)
        })
    }

    /// The GPU copy of the capture at `w`x`h`, (re)allocated on a size change.
    fn source(&mut self, w: u32, h: u32) -> Result<(CUdeviceptr, usize)> {
        if let Some((p, pitch, sw, sh)) = self.src {
            if (sw, sh) == (w, h) {
                return Ok((p, pitch));
            }
            // SAFETY: freeing our own allocation; the context is current.
            unsafe { (self.cu.mem_free)(p) };
            self.src = None;
        }
        let (mut p, mut pitch) = (0, 0);
        // SAFETY: out-pointers valid; the context is current.
        check(self.cu, "cuMemAllocPitch", unsafe {
            (self.cu.mem_alloc_pitch)(&mut p, &mut pitch, w as usize * 4, h as usize, 4)
        })?;
        self.src = Some((p, pitch, w, h));
        Ok((p, pitch))
    }
}

impl Drop for CudaConvert {
    fn drop(&mut self) {
        if let Ok(_cur) = Current::push(self.cu, self.ctx) {
            // SAFETY: our own allocation and module, in their context.
            unsafe {
                if let Some((p, ..)) = self.src.take() {
                    (self.cu.mem_free)(p);
                }
                (self.cu.module_unload)(self.module);
            }
        }
        // SAFETY: our reference from `new`, after the context's last use.
        unsafe { ff::ffi::av_buffer_unref(&mut self.device) };
    }
}

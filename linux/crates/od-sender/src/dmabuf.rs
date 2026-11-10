//! GPU buffers (dmabufs) for zero-copy screen capture: the capture hands
//! them to the compositor to render into, and the VA-API encoder imports the
//! same memory without a CPU copy. They are allocated with Mesa's libgbm on
//! the render node the compositor announced, so the buffer is one that the
//! compositor's GPU can write and the encoder's GPU can read.
//!
//! libgbm is reached through a hand-written binding of the handful of entry
//! points used, resolved at run time with `dlopen`, as libva is in
//! `encoder/vpp.rs`. The static release build must neither link nor require
//! libgbm: a machine without it (or without a GPU) just gets shared-memory
//! capture, and the error from [`gbm`] says why.
//!
//! A [`GbmBuffer`] holds only the exported dmabuf file descriptors and the
//! layout. The `gbm_bo` is destroyed right after the export, since the
//! descriptors keep the memory alive, so a buffer owns no gbm pointer and is
//! `Send + Sync`.

// Used by capture and the VA-API encoder.
#![allow(dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result, anyhow, bail};
use tracing::debug;

use crate::backend::{DmabufPlane, PixelFormat};

const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GBM_BO_USE_LINEAR: u32 = 1 << 4;
/// `GBM_BO_TRANSFER_READ` is 1 << 0, unused.
const GBM_BO_TRANSFER_WRITE: u32 = 1 << 1;

pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// `struct gbm_device`.
#[repr(C)]
struct GbmDeviceRaw {
    _private: [u8; 0],
}

/// `struct gbm_bo`.
#[repr(C)]
struct GbmBo {
    _private: [u8; 0],
}

// glibc before 2.34 keeps dlopen in libdl; later versions ship an empty one.
#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_NOW: c_int = 2;

/// Declares the libgbm entry points: a table of function pointers resolved
/// by name from the opened library. The `optional` ones are `Option`s, for
/// functions older Mesa versions lack.
macro_rules! libgbm {
    (
        $( fn $name:ident($($arg:ty),*) -> $ret:ty; )*
        optional {
            $( fn $oname:ident($($oarg:ty),*) -> $oret:ty; )*
        }
    ) => {
        struct Gbm {
            $( $name: unsafe extern "C" fn($($arg),*) -> $ret, )*
            $( $oname: Option<unsafe extern "C" fn($($oarg),*) -> $oret>, )*
        }

        impl Gbm {
            /// # Safety
            /// `lib` is a handle from `dlopen` of libgbm, which is never closed.
            unsafe fn resolve(lib: *mut c_void) -> Result<Self> {
                Ok(Self {
                    $( $name: {
                        let sym = concat!(stringify!($name), "\0");
                        // SAFETY: `sym` is NUL-terminated; the declared
                        // signature is the one gbm.h documents for the name.
                        let p = unsafe { dlsym(lib, sym.as_ptr().cast()) };
                        if p.is_null() {
                            bail!("libgbm has no {}", stringify!($name));
                        }
                        // SAFETY: a non-null symbol address is a function
                        // pointer of the declared type.
                        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) -> $ret>(p) }
                    }, )*
                    $( $oname: {
                        let sym = concat!(stringify!($oname), "\0");
                        // SAFETY: as above.
                        let p = unsafe { dlsym(lib, sym.as_ptr().cast()) };
                        if p.is_null() {
                            None
                        } else {
                            // SAFETY: as above.
                            Some(unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($oarg),*) -> $oret>(p) })
                        }
                    }, )*
                })
            }
        }
    };
}

libgbm! {
    fn gbm_create_device(c_int) -> *mut GbmDeviceRaw;
    fn gbm_device_destroy(*mut GbmDeviceRaw) -> ();
    fn gbm_bo_create(*mut GbmDeviceRaw, u32, u32, u32, u32) -> *mut GbmBo;
    fn gbm_bo_destroy(*mut GbmBo) -> ();
    fn gbm_bo_get_plane_count(*mut GbmBo) -> c_int;
    fn gbm_bo_get_fd_for_plane(*mut GbmBo, c_int) -> c_int;
    fn gbm_bo_get_stride_for_plane(*mut GbmBo, c_int) -> u32;
    fn gbm_bo_get_offset(*mut GbmBo, c_int) -> u32;
    fn gbm_bo_get_modifier(*mut GbmBo) -> u64;
    fn gbm_bo_get_width(*mut GbmBo) -> u32;
    fn gbm_bo_get_height(*mut GbmBo) -> u32;
    fn gbm_bo_get_format(*mut GbmBo) -> u32;
    fn gbm_bo_map(*mut GbmBo, u32, u32, u32, u32, u32, *mut u32, *mut *mut c_void) -> *mut c_void;
    fn gbm_bo_unmap(*mut GbmBo, *mut c_void) -> ();
    optional {
        fn gbm_bo_create_with_modifiers2(*mut GbmDeviceRaw, u32, u32, u32, *const u64, c_uint, u32) -> *mut GbmBo;
        fn gbm_bo_create_with_modifiers(*mut GbmDeviceRaw, u32, u32, u32, *const u64, c_uint) -> *mut GbmBo;
    }
}

// SAFETY: function pointers into a library that stays loaded.
unsafe impl Send for Gbm {}
unsafe impl Sync for Gbm {}

/// libgbm, loaded once; the error is kept so every open reports it.
fn gbm() -> Result<&'static Gbm> {
    static GBM: OnceLock<std::result::Result<Gbm, String>> = OnceLock::new();
    GBM.get_or_init(|| {
        // SAFETY: a NUL-terminated name; the handle is never closed.
        let lib = unsafe { dlopen(c"libgbm.so.1".as_ptr(), RTLD_NOW) };
        if lib.is_null() {
            return Err("libgbm.so.1 is not installed".to_string());
        }
        // SAFETY: `lib` is libgbm's handle.
        unsafe { Gbm::resolve(lib) }.map_err(|e| format!("{e:#}"))
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

/// The DRM fourcc of a capture format (`DRM_FORMAT_XRGB8888` and friends).
pub fn drm_fourcc(format: PixelFormat) -> u32 {
    match format {
        PixelFormat::Xrgb8888 => u32::from_le_bytes(*b"XR24"),
        PixelFormat::Argb8888 => u32::from_le_bytes(*b"AR24"),
        PixelFormat::Xbgr8888 => u32::from_le_bytes(*b"XB24"),
        PixelFormat::Abgr8888 => u32::from_le_bytes(*b"AB24"),
    }
}

/// Major number of a `dev_t`, as glibc's `gnu_dev_major`.
fn dev_major(dev: u64) -> u32 {
    (((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0xfff)) as u32
}

/// Minor number of a `dev_t`, as glibc's `gnu_dev_minor`.
fn dev_minor(dev: u64) -> u32 {
    (((dev >> 12) & 0xffff_ff00) | (dev & 0xff)) as u32
}

/// A `dev_t` from its parts, as glibc's `gnu_dev_makedev`.
fn dev_make(major: u32, minor: u32) -> u64 {
    let (major, minor) = (u64::from(major), u64::from(minor));
    ((major & 0xffff_f000) << 32)
        | ((major & 0xfff) << 8)
        | ((minor & 0xffff_ff00) << 12)
        | (minor & 0xff)
}

/// The `dev_t` in a `wl_array` as the compositor sends it: native-endian
/// (little, on every target we run on) and 8 bytes, or 4 where `dev_t` is
/// 32 bits.
pub fn dev_from_bytes(bytes: &[u8]) -> Option<u64> {
    match bytes.len() {
        8 => Some(u64::from_le_bytes(bytes.try_into().ok()?)),
        4 => Some(u64::from(u32::from_le_bytes(bytes.try_into().ok()?))),
        _ => None,
    }
}

/// The render node (`/dev/dri/renderD*`) of the GPU a compositor announced as
/// `dev`. That can be the primary node (`card0`) or already a render node
/// (minor 128 and up); GBM and VA-API want the render node, which needs no
/// DRM master.
pub fn render_node_for(dev: u64) -> Result<PathBuf> {
    render_node_in(Path::new("/sys/dev/char"), dev)
}

/// [`render_node_for`] with the sysfs `dev/char` directory as a parameter.
fn render_node_in(sys_dev_char: &Path, dev: u64) -> Result<PathBuf> {
    let (major, minor) = (dev_major(dev), dev_minor(dev));
    if minor >= 128 {
        return Ok(PathBuf::from(format!("/dev/dri/renderD{minor}")));
    }
    let dir = sys_dev_char
        .join(format!("{major}:{minor}"))
        .join("device/drm");
    let entries = std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("renderD") {
            return Ok(Path::new("/dev/dri").join(name));
        }
    }
    bail!("device {major}:{minor} has no render node")
}

/// An opened render node with a `gbm_device` on it.
pub struct GbmDevice {
    gbm: &'static Gbm,
    raw: *mut GbmDeviceRaw,
    // Closed after `raw` is destroyed: `Drop` runs before the fields drop.
    _file: File,
}

// SAFETY: a gbm_device is a handle to a DRM fd; Mesa's allocation entry points
// are thread-safe, and the device is used from the thread that owns it.
unsafe impl Send for GbmDevice {}

impl Drop for GbmDevice {
    fn drop(&mut self) {
        // SAFETY: `raw` came from gbm_create_device and is destroyed once,
        // before `_file` closes the fd it uses.
        unsafe { (self.gbm.gbm_device_destroy)(self.raw) };
    }
}

/// A `gbm_bo`, destroyed on drop.
struct Bo<'a> {
    gbm: &'a Gbm,
    raw: *mut GbmBo,
}

impl Drop for Bo<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` is a live bo owned by this guard.
        unsafe { (self.gbm.gbm_bo_destroy)(self.raw) };
    }
}

/// A GPU buffer as exported dmabufs. Holds no gbm pointer: the file
/// descriptors keep the memory alive after the `gbm_bo` is gone.
pub struct GbmBuffer {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// The modifier the driver actually chose.
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
}

impl GbmDevice {
    pub fn open(path: &Path) -> Result<Self> {
        let gbm = gbm()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: the fd is open and stays open as long as the device, which
        // is destroyed first.
        let raw = unsafe { (gbm.gbm_create_device)(file.as_raw_fd()) };
        if raw.is_null() {
            bail!("gbm_create_device failed on {}", path.display());
        }
        Ok(Self {
            gbm,
            raw,
            _file: file,
        })
    }

    /// Allocates a buffer the compositor can render into. `modifiers` are the
    /// ones the compositor accepts; empty (or only `INVALID`) lets the driver
    /// choose, `[LINEAR]` asks for a linear buffer.
    pub fn alloc(
        &self,
        width: u32,
        height: u32,
        format: PixelFormat,
        modifiers: &[u64],
    ) -> Result<GbmBuffer> {
        let bo = self.create_bo(width, height, format, modifiers)?;
        self.export(&bo, width, height, format)
    }

    /// [`alloc`](Self::alloc), with every pixel set to `fill(x, y)` (its four
    /// bytes in memory order) through a CPU mapping; Mesa detiles on the way
    /// for tiled buffers. For tests and checks of the encoder's import.
    pub(crate) fn alloc_filled(
        &self,
        width: u32,
        height: u32,
        format: PixelFormat,
        modifiers: &[u64],
        fill: impl Fn(u32, u32) -> [u8; 4],
    ) -> Result<GbmBuffer> {
        let bo = self.create_bo(width, height, format, modifiers)?;
        let mut stride = 0u32;
        let mut map_data: *mut c_void = std::ptr::null_mut();
        // SAFETY: the bo is live and the rectangle is inside it; the out
        // pointers are valid for the call.
        let ptr = unsafe {
            (self.gbm.gbm_bo_map)(
                bo.raw,
                0,
                0,
                width,
                height,
                GBM_BO_TRANSFER_WRITE,
                &mut stride,
                &mut map_data,
            )
        };
        if ptr.is_null() {
            bail!("gbm_bo_map failed");
        }
        // SAFETY: the mapping is `height` rows of `stride` bytes, valid until
        // gbm_bo_unmap below; nothing else touches it meanwhile.
        let map = unsafe {
            std::slice::from_raw_parts_mut(ptr.cast::<u8>(), stride as usize * height as usize)
        };
        for y in 0..height {
            let row = &mut map[y as usize * stride as usize..][..width as usize * 4];
            for (x, px) in row.chunks_exact_mut(4).enumerate() {
                px.copy_from_slice(&fill(x as u32, y));
            }
        }
        // SAFETY: `map_data` is from the gbm_bo_map above; `map` is not used
        // after this.
        unsafe { (self.gbm.gbm_bo_unmap)(bo.raw, map_data) };
        self.export(&bo, width, height, format)
    }

    fn create_bo(
        &self,
        width: u32,
        height: u32,
        format: PixelFormat,
        modifiers: &[u64],
    ) -> Result<Bo<'static>> {
        let gbm = self.gbm;
        let fourcc = drm_fourcc(format);
        let any = modifiers.iter().all(|&m| m == DRM_FORMAT_MOD_INVALID);
        let raw = if any {
            // SAFETY: the device is live; plain integers otherwise.
            unsafe { (gbm.gbm_bo_create)(self.raw, width, height, fourcc, GBM_BO_USE_RENDERING) }
        } else if modifiers == [DRM_FORMAT_MOD_LINEAR] {
            // Plainer and more widely supported than a one-entry modifier list.
            // SAFETY: as above.
            unsafe {
                (gbm.gbm_bo_create)(
                    self.raw,
                    width,
                    height,
                    fourcc,
                    GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR,
                )
            }
        } else if let Some(create) = gbm.gbm_bo_create_with_modifiers2 {
            // SAFETY: `modifiers` is valid for `len` entries for the call.
            unsafe {
                create(
                    self.raw,
                    width,
                    height,
                    fourcc,
                    modifiers.as_ptr(),
                    modifiers.len() as c_uint,
                    GBM_BO_USE_RENDERING,
                )
            }
        } else if let Some(create) = gbm.gbm_bo_create_with_modifiers {
            // SAFETY: as above.
            unsafe {
                create(
                    self.raw,
                    width,
                    height,
                    fourcc,
                    modifiers.as_ptr(),
                    modifiers.len() as c_uint,
                )
            }
        } else {
            bail!("libgbm cannot allocate with modifiers");
        };
        if raw.is_null() {
            bail!(
                "gbm could not allocate {width}x{height} {format:?} with modifiers [{}]",
                modifiers
                    .iter()
                    .map(|&m| modifier_name(m))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(Bo { gbm, raw })
    }

    /// Exports every plane as a dmabuf and drops the bo.
    fn export(
        &self,
        bo: &Bo<'_>,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<GbmBuffer> {
        let gbm = self.gbm;
        // SAFETY (all calls below): `bo.raw` is a live bo.
        let count = unsafe { (gbm.gbm_bo_get_plane_count)(bo.raw) };
        if count < 1 {
            bail!("gbm buffer has {count} planes");
        }
        let mut planes = Vec::with_capacity(count as usize);
        for plane in 0..count {
            let fd = unsafe { (gbm.gbm_bo_get_fd_for_plane)(bo.raw, plane) };
            if fd < 0 {
                bail!("gbm_bo_get_fd_for_plane({plane}) failed");
            }
            // SAFETY: gbm returned a new descriptor that nothing else owns.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            planes.push(DmabufPlane {
                fd: Arc::new(fd),
                offset: unsafe { (gbm.gbm_bo_get_offset)(bo.raw, plane) },
                stride: unsafe { (gbm.gbm_bo_get_stride_for_plane)(bo.raw, plane) },
            });
        }
        let modifier = unsafe { (gbm.gbm_bo_get_modifier)(bo.raw) };
        debug!(
            width,
            height,
            planes = planes.len(),
            modifier = %modifier_name(modifier),
            "allocated gbm buffer"
        );
        Ok(GbmBuffer {
            width,
            height,
            format,
            modifier,
            planes,
        })
    }
}

/// Whether a capture buffer with this DRM format modifier may be imported:
/// linear, and Intel's uncompressed tilings. Compressed (CCS) layouts are
/// refused although iHD accepts them: on Lunar Lake (iHD 26.3.5)
/// `I915_4_TILED_LNL_CCS` imports and converts without an error into a
/// black picture, and gbm picks it whenever it is offered. Other
/// vendors' tilings are untested, so only linear for them.
pub fn importable_modifier(modifier: u64) -> bool {
    const INTEL: u64 = 0x01 << 56;
    const ALLOWED: [u64; 5] = [
        DRM_FORMAT_MOD_LINEAR,
        INTEL | 1, // I915_X_TILED
        INTEL | 2, // I915_Y_TILED
        INTEL | 3, // I915_Yf_TILED
        INTEL | 9, // I915_4_TILED
    ];
    ALLOWED.contains(&modifier)
}

/// A DRM format modifier for logs.
pub fn modifier_name(modifier: u64) -> String {
    const INTEL: u64 = 0x01;
    const AMD: u64 = 0x02;
    if modifier == DRM_FORMAT_MOD_LINEAR {
        return "LINEAR".into();
    }
    if modifier == DRM_FORMAT_MOD_INVALID {
        return "INVALID".into();
    }
    let (vendor, code) = (modifier >> 56, modifier & 0x00ff_ffff_ffff_ffff);
    if vendor == INTEL {
        let name = match code {
            1 => "I915_X_TILED",
            2 => "I915_Y_TILED",
            3 => "I915_Yf_TILED",
            4 => "I915_Y_TILED_CCS",
            5 => "I915_Yf_TILED_CCS",
            6 => "I915_Y_TILED_GEN12_RC_CCS",
            7 => "I915_Y_TILED_GEN12_MC_CCS",
            8 => "I915_Y_TILED_GEN12_RC_CCS_CC",
            9 => "I915_4_TILED",
            10 => "I915_4_TILED_DG2_RC_CCS",
            11 => "I915_4_TILED_DG2_MC_CCS",
            12 => "I915_4_TILED_DG2_RC_CCS_CC",
            13 => "I915_4_TILED_MTL_RC_CCS",
            14 => "I915_4_TILED_MTL_MC_CCS",
            15 => "I915_4_TILED_MTL_RC_CCS_CC",
            16 => "I915_4_TILED_LNL_CCS",
            17 => "I915_4_TILED_BMG_CCS",
            _ => "",
        };
        if !name.is_empty() {
            return name.into();
        }
    }
    if vendor == AMD {
        return format!("AMD({modifier:#x})");
    }
    format!("{modifier:#x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn importable_modifiers() {
        for m in [
            0,
            0x0100_0000_0000_0001,
            0x0100_0000_0000_0002,
            0x0100_0000_0000_0003,
            0x0100_0000_0000_0009,
        ] {
            assert!(importable_modifier(m), "{}", modifier_name(m));
        }
        // Compressed Intel layouts, another vendor's tiling, INVALID.
        for m in [
            0x0100_0000_0000_0004,
            0x0100_0000_0000_0006,
            0x0100_0000_0000_000a,
            0x0100_0000_0000_000d,
            0x0100_0000_0000_0010,
            0x0100_0000_0000_0011,
            0x0200_0000_0000_0123,
            DRM_FORMAT_MOD_INVALID,
        ] {
            assert!(!importable_modifier(m), "{}", modifier_name(m));
        }
    }

    #[test]
    fn dev_t_round_trip() {
        for (major, minor) in [(226, 0), (226, 128), (0x1234, 0x5678)] {
            let dev = dev_make(major, minor);
            assert_eq!((dev_major(dev), dev_minor(dev)), (major, minor));
        }
        // What glibc's makedev(226, 128) is on Linux.
        assert_eq!(dev_make(226, 128), (226 << 8) | 128);
    }

    #[test]
    fn dev_from_wl_array() {
        let dev = dev_make(226, 128);
        assert_eq!(dev_from_bytes(&dev.to_le_bytes()), Some(dev));
        assert_eq!(dev_from_bytes(&(dev as u32).to_le_bytes()), Some(dev));
        assert_eq!(dev_from_bytes(&[1, 2, 3]), None);
        assert_eq!(dev_from_bytes(&[]), None);
    }

    #[test]
    fn render_node_lookup() {
        let root = std::env::temp_dir().join(format!("od-dmabuf-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let drm = root.join("226:0/device/drm");
        std::fs::create_dir_all(drm.join("card0")).unwrap();
        std::fs::create_dir_all(drm.join("renderD128")).unwrap();
        std::fs::create_dir_all(root.join("226:1/device/drm/card1")).unwrap();

        // A primary node maps through sysfs, a render node directly.
        assert_eq!(
            render_node_in(&root, dev_make(226, 0)).unwrap(),
            Path::new("/dev/dri/renderD128")
        );
        assert_eq!(
            render_node_in(&root, dev_make(226, 129)).unwrap(),
            Path::new("/dev/dri/renderD129")
        );
        // No render node behind it, and no such device.
        assert!(render_node_in(&root, dev_make(226, 1)).is_err());
        assert!(render_node_in(&root, dev_make(226, 5)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fourccs() {
        assert_eq!(drm_fourcc(PixelFormat::Xrgb8888), 0x3432_5258);
        assert_eq!(drm_fourcc(PixelFormat::Argb8888), 0x3432_5241);
        assert_eq!(drm_fourcc(PixelFormat::Xbgr8888), 0x3432_4258);
        assert_eq!(drm_fourcc(PixelFormat::Abgr8888), 0x3432_4241);
    }

    #[test]
    fn modifier_names() {
        assert_eq!(modifier_name(0), "LINEAR");
        assert_eq!(modifier_name(DRM_FORMAT_MOD_INVALID), "INVALID");
        assert_eq!(modifier_name(0x0100_0000_0000_0001), "I915_X_TILED");
        assert_eq!(modifier_name(0x0100_0000_0000_0009), "I915_4_TILED");
        assert_eq!(modifier_name(0x0100_0000_0000_0010), "I915_4_TILED_LNL_CCS");
        assert_eq!(
            modifier_name(0x0200_0000_0000_0123),
            "AMD(0x200000000000123)"
        );
        assert_eq!(modifier_name(0x0300_0000_0000_0001), "0x300000000000001");
    }

    #[test]
    fn allocates_on_the_gpu() {
        let node = Path::new("/dev/dri/renderD128");
        let dev = match GbmDevice::open(node) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: {e:#}");
                return;
            }
        };
        let (w, h) = (2388, 1668);
        let fmt = PixelFormat::Xrgb8888;
        let fill = |x: u32, y: u32| [x as u8, y as u8, (x ^ y) as u8, 0xff];
        let check = |b: &GbmBuffer| {
            assert!(!b.planes.is_empty());
            assert!(b.planes[0].stride >= w * 4);
            eprintln!(
                "{} planes, modifier {}, stride {}",
                b.planes.len(),
                modifier_name(b.modifier),
                b.planes[0].stride
            );
        };
        for mods in [&[DRM_FORMAT_MOD_LINEAR][..], &[0x0100_0000_0000_0009, 0]] {
            match dev.alloc(w, h, fmt, mods) {
                Ok(b) => check(&b),
                Err(e) => eprintln!("alloc with {mods:x?} unsupported: {e:#}"),
            }
            match dev.alloc_filled(w, h, fmt, mods, fill) {
                Ok(b) => check(&b),
                Err(e) => eprintln!("alloc_filled with {mods:x?} unsupported: {e:#}"),
            }
        }
        let b = dev.alloc(w, h, fmt, &[]).unwrap();
        check(&b);
    }
}

//! Run-time loading of the system libraries the static release build must
//! not link: libva (`encoder/vpp.rs`), libgbm (`dmabuf.rs`) and the NVIDIA
//! driver's libcuda (`encoder/cuda.rs`). A machine without them still runs,
//! without the features that need them.

use std::ffi::{c_char, c_int, c_void};

// glibc before 2.34 keeps dlopen in libdl; later versions ship an empty one.
// SAFETY: these are the signatures `<dlfcn.h>` declares for both functions.
#[link(name = "dl")]
unsafe extern "C" {
    pub fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    pub fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

pub const RTLD_NOW: c_int = 2;

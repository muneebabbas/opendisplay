#!/bin/sh
# Regenerate nv12.ptx from nv12.cu. Needs only clang (no CUDA toolkit).
# sm_52 PTX runs on every NVENC GPU the driver supports (Maxwell 2 and up);
# the driver JIT-compiles it for the GPU at load.
set -e
cd "$(dirname "$0")"
clang -x cuda --cuda-device-only --cuda-gpu-arch=sm_52 -nocudainc -nocudalib \
  -O3 -S -o nv12.ptx nv12.cu

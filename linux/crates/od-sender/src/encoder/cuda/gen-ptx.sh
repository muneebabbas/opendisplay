#!/bin/sh
# Regenerate nv12.ptx from nv12.cu. Needs only clang with the NVPTX target
# (no CUDA toolkit); nv12.cu uses clang's __nvvm builtins, so nvcc does not
# build it. Last generated with clang 23.1.
# sm_52 PTX runs on every NVENC GPU the driver supports (Maxwell 2 and up);
# the driver JIT-compiles it for the GPU at load.
#
# `gen-ptx.sh --check` regenerates into a temporary file and fails if the
# checked-in PTX differs from it (comment lines aside).
set -e
cd "$(dirname "$0")"
out=nv12.ptx
if [ "$1" = "--check" ]; then
  out=$(mktemp)
  trap 'rm -f "$out"' EXIT
fi
clang -x cuda --cuda-device-only --cuda-gpu-arch=sm_52 -nocudainc -nocudalib \
  -O3 -S -o "$out" nv12.cu
if [ "$1" = "--check" ]; then
  grep -v '^//' nv12.ptx > "$out.a"
  grep -v '^//' "$out" > "$out.b"
  trap 'rm -f "$out" "$out.a" "$out.b"' EXIT
  diff -u "$out.a" "$out.b" && echo "nv12.ptx matches nv12.cu"
fi

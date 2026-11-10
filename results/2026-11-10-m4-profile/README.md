# M4 per-step timings and fallback logs (2026-11-10)

* `m4-ab-shm.log` / `m4-ab-gpu.log`: same binary, `RUST_LOG=debug`, iPad,
  video workload, `OD_VAAPI_SHM_CAPTURE=1` vs default. Per frame:
  shm `upload 2.6-3.1, vpp submit 0.46-0.58 ms`; gpu `import 0.01, vpp submit
  0.62-0.99 ms`. `send_frame` (waits for the encode) ~12.3 ms in both.
* `m4-ipad.log`: first iPad session (HEVC, 59 fps, e2e p50 28-32 ms).
* `m4-fail.log`: debug build with `OD_FAIL_DMABUF_IMPORT=1` on a static
  screen: the first GPU frame fails, capture recreates its session on shared
  memory and the stream continues (IDR, 20 frames sent).
* perf of the video run (encode thread, ms of CPU per frame at 59.5 fps):
  iHD encode submit + kernel 1.37, VPP submit 0.92, timer interrupts 0.55,
  od NAL processing (`normalize_start_codes`, `access_units_for`) 0.29,
  FFmpeg's copy of the bitstream out of the coded buffer 0.26, iHD-internal
  memcpy 0.07. Not kept (57 MB).

# VA-API H.264 encode for od-sender (Intel/AMD)

Status: **stage A committed on branch `linux/vaapi-encoder` (three commits on
top of upstream v1.30.2) as an opt-in encoder (`--encoder vaapi`), measured
on one Intel machine against a real iPad.** The main finding is that the
encode is not where the sender's CPU goes: colour conversion is. GPU colour
conversion (stage A2) and dmabuf capture (stage B) are design sketches.
Written 2026-11-09. Fork-only notes (branch `fork`), not for upstream PRs.

This is "Spike 0" from the beta plan (removed in #397) and the open hardware
encode item in #299. The plan put it on an Intel box, which this is.

## Test machine

| | |
|---|---|
| Laptop | ASUS Zenbook, Intel Core Ultra 5 226V (Lunar Lake), 4 P + 4 LP E cores |
| GPU | Arc Graphics 130V (Xe2), `xe` kernel driver, media GT 400-1200 MHz |
| VA-API | libva 2.24, intel-media-driver (iHD) 26.3.5; H.264 encode on `VAEntrypointEncSlice` |
| Software | Arch / Omarchy, Linux 7.2.15, Hyprland 0.57.0, FFmpeg 9.0.2 (system, with VA-API) |
| Receiver | iPad (2388x1668 @2, App Store receiver) over USB (usbmuxd forward to TCP 9000) |

## Stage A: what the branch does

The encoder stays the FFmpeg encoder (`encoder/ffmpeg.rs`). VA-API is a third
`Kind` next to libx264 and `h264_v4l2m2m`:

```
capture (wl_shm, BGRA)
  -> convert stage thread: swscale BGRA -> NV12, BT.709 limited (unchanged)
  -> encode thread: av_hwframe_get_buffer + av_hwframe_transfer_data  (new: upload)
  -> h264_vaapi
  -> access units, SPS/PPS fix-up, wire (unchanged)
```

* `HwFrames` owns the VA-API device (`av_hwdevice_ctx_create`, default device:
  libva picks the render node) and a pool of NV12 surfaces at the encode size.
  The codec context takes its own reference (`hw_frames_ctx`), and its pixel
  format is `AV_PIX_FMT_VAAPI`. Everything else in the FFmpeg encoder (pts,
  in-flight bookkeeping, IDR parameter sets) is shared with x264.
* `Backend::Vaapi` / `--encoder vaapi` / `encoder = "vaapi"`, listed by
  `od-sender doctor`. **Not in `auto`**: the beta plan wanted it "ahead of
  x264 in auto order", but with the conversion still on the CPU it adds
  latency for a modest CPU saving (measurements below). It should join `auto`
  once conversion moves to the GPU.
* Static FFmpeg builds without VA-API compile unchanged: the hwcontext API is
  in libavutil either way, and `find_by_name("h264_vaapi")` fails at run time
  with "this FFmpeg build has no h264_vaapi".

### Encoder settings, and why

| Setting | Value | Why |
|---|---|---|
| bitrate / maxrate | `--bitrate-mbps` | Same targets as x264 |
| `rc_buffer_size` | bitrate / 4 | Same quarter-second VBV as x264 |
| `global_quality` | 21 | Same as x264's `crf=21`. With bitrate, maxrate and quality all set, FFmpeg's automatic RC mode picks **QVBR** on iHD (quality-targeted under a cap, i.e. what x264 does) and otherwise falls back to a bitrate mode the driver has (VBR or CBR; not yet checked on Mesa). No `rc_mode` is forced. |
| `gop_size` | `i32::MAX` | §5.3: keyframes only on demand. A picture sent as type I becomes an IDR (`vaapi_encode` sets `force_idr`). Verified: IDRs only where forced. |
| `max_b_frames` | 0 | Low latency (already set for every FFmpeg encoder) |
| `async_depth` | 1 | Default 2 holds one frame back: its packet only comes out when the *next* frame is sent, which on a static desktop could be seconds later. With 1, each call returns its own picture (asserted by the test). |
| `sei` | 0 | No encoder-identification SEI in IDRs |
| VUI | BT.709, limited | Set on the context as for x264; decodes as BT.709 (asserted) |

### The convert stage for the VA-API encoder

The convert stage (`encoder/stage.rs`) only engaged at 2560 px and wider: for
software encoders a narrower frame converts quickly enough inside the encode
call. A hardware encoder's encode call mostly *waits* on the GPU, so
converting the next frame on another thread meanwhile is free in wall time.
`Encoder::stage_min_width()` returns 0 for VA-API and the stage uses it.
Without it, VA-API at 2388x1668 ran convert, upload and encode in series and
managed 39 fps in `pipeline-bench`; with it, 55-57 fps.

## Measurements

### Real iPad, over USB

`od-sender run --to=127.0.0.1:<usbmux forward> --encoder=<x264|vaapi>
--workspace=9`, with the user's config (35 Mbit/s for this receiver; stream
2388x1668 @60). Each run: session up, the workload started on workspace 9
(shown on the iPad), 6 s settle, then a 20 s window. CPU is **whole-process**
CPU from `/proc/<pid>/stat`. iPad numbers are the receiver's own
`PHONE-STATS` (medians of its reports in the window).

* idle: nothing on the workspace;
* scroll: `foot` running `seq` in a loop (full-height text scrolling every frame);
* video: `mpv` full screen playing `testsrc2` at 60 fps.

| Workload | Encoder | Sender CPU | iPad fps | e2e p50 / p95 | Bitrate |
|---|---|---|---|---|---|
| idle | x264 | 0.01 cores | - | - | 0 |
| idle | VA-API | 0.09 cores | - | - | 0 |
| scroll | x264 | 1.61 cores | 59 | 20.5 / 28.5 ms | 11.6 Mbit/s |
| scroll | VA-API | 1.23 cores (-24%) | 59 | 31 / 37 ms | 7.7 Mbit/s |
| video | x264 | 2.62 cores | 59 | 25.5 / 34 ms | 26.1 Mbit/s |
| video | VA-API | 2.16 cores (-18%) | 46 | 42 / 49 ms | 23.4 Mbit/s |

No stalls or hitches in any window. The media GT stayed at 400-450 MHz
during VA-API runs (`/sys/class/drm/card0/device/tile0/gt1/freq0/act_freq`).

### Where the CPU goes

`perf record -g` on the VA-API video session (symbols from a
`CARGO_PROFILE_RELEASE_STRIP=none` build). Raw iPad results:
`results/2026-11-09-ipad/results.jsonl`, harness: `tools/ipad_bench.py`:

| By library | Share | | By thread | Share |
|---|---|---|---|---|
| libswscale | 69% | | `convert` | 73% |
| kernel | 13% | | `encode` | 26% |

So **BGRA -> NV12 conversion is about 1.3-1.5 cores** at ~50-60 fps, for
x264 and VA-API alike; the encode is the smaller part. The banded converter
spawns scoped threads per frame across all eight cores (including the LP E
cores), so 3-4 ms of wall time is ~20-25 ms of CPU per frame. A single-thread
`ffmpeg` conversion with the same flags costs about 4-6 ms per frame.

The encode thread's share includes the upload: about 4% of all samples are
the kernel zeroing fresh pages (`clear_pages`) under `xe`/`ttm`, i.e. the
upload path allocates GPU memory per frame.

### Correction to an earlier measurement

A first round of `pipeline-bench` numbers (x264 1.60 vs VA-API 0.73 cores)
summed `/proc/<pid>/task/*/stat`, which misses threads that have already
exited, which is exactly the per-frame conversion threads. Measured from
`/proc/<pid>/stat` the same bench gives x264 **2.78** vs VA-API **2.34**
cores, consistent with the iPad runs above.

### Correctness

* `cargo test -p od-sender vaapi` (skips without a VA-API device or an FFmpeg
  H.264 decoder, so CI without a GPU is unaffected):
  * `vaapi_idrs_only_on_demand_and_decode_as_bt709`: 12 changing frames, IDRs
    forced at 0 and 7. Exactly one picture per call, IDRs exactly at 0 and 7,
    and all 12 decode with FFmpeg's H.264 decoder as BT.709 limited range at
    the right size.
  * `vaapi_reopens_at_a_new_size`: landscape, portrait (rotation) and an odd
    logical size each open and start with an IDR.
* End to end on Hyprland 0.57, against `od-receiver --sink none` on loopback
  and against the iPad: virtual display created and removed, `streamConfig
  h264 2388x1668 @60`, IDR on connect, receiver reports `codec "H.264"`, no
  stalls.
* Not yet checked: picture quality side by side on the iPad, a long session,
  resize and rotation mid-session, AMD (Mesa: which RC mode FFmpeg falls back
  to), a machine with two GPUs.

## Stage A2: colour conversion on the GPU (plan and first findings)

Upload the captured BGRA and convert to NV12 with VA-API video processing
(VPP) instead of swscale. With the `ffmpeg` CLI on raw 2388x1668 BGRA frames,
after subtracting the cost of reading the input:

| Pipeline | CPU per frame |
|---|---|
| swscale (`area+accurate_rnd`) -> NV12 -> upload -> `h264_vaapi` | ~15 ms |
| upload BGRA -> `scale_vaapi=format=nv12` -> `h264_vaapi` | ~2.5 ms |

About six times less, roughly 0.15 instead of 0.9 cores at 60 fps, before
stage B.

**Blocker found: the colour matrix.** iHD's VPP ignores the requested output
matrix and range here. Flat patches converted with
`scale_vaapi=format=nv12:out_color_matrix=bt709:out_range=limited` come out
as **BT.601** limited:

| Patch | swscale (BT.709, matches the spec) | iHD VPP |
|---|---|---|
| red | 63, 102, 240 | 81, 90, 240 (BT.601) |
| green | 173, 42, 26 | 145, 54, 34 (BT.601) |
| blue | 32, 240, 118 | 41, 240, 110 (BT.601) |
| white / black / grey | 235 / 16 / 126, neutral | same |

`out_color_matrix=bt709` and `out_range=full` give byte-identical output. The
stream's VUI says BT.709, so saturated colours would shift on the receiver.
Options, in order of preference:

1. Set the VPP output colour standard directly through libva
   (`VAProcPipelineParameterBuffer.output_color_standard` /
   `output_color_properties`) and check whether iHD honours it when FFmpeg's
   filter does not.
2. Announce BT.601 in the VUI when the GPU converted. This needs every
   receiver (iOS, Mac, Android, Linux) to honour the VUI matrix. Today's
   comment in `encoder/ffmpeg.rs` says the receiver converts back per the VUI,
   but that has to be checked per receiver.
3. A small GPU shader for the conversion instead of VPP: exact, but a new
   dependency (GL/Vulkan compute).

Build impact: through FFmpeg this means libavfilter (`scale_vaapi`), which the
static build disables and `ffmpeg-next`'s `filter` feature would pull in.
Calling libva's VPP directly avoids that, but needs hand-written libva FFI.

## Stage A2: result (2026-11-10)

Branch `linux/vaapi-gpu-convert`. **The colour blocker was FFmpeg, not iHD.**

**Why `scale_vaapi` gave BT.601.** iHD lists `VAProcColorStandardExplicit`
among its output standards, and when it does, `libavfilter/vaapi_vpp.c`
(`vaapi_vpp_fill_colour_standard`) always picks Explicit and copies the
frame's primaries/transfer/matrix/range into `VAProcColorProperties`. In
Explicit mode iHD (`VpSetColorStandardExplictly`,
`media_softlet/linux/common/vp/ddi/ddi_vp_functions.cpp`) chooses the YUV
matrix **from `colour_primaries` alone** (1 -> BT.709, 6 -> BT.601, 9 ->
BT.2020, anything else -> BT.601 limited, range ignored).
`out_color_matrix=bt709` leaves primaries unspecified (2), hence BT.601.
`scale_vaapi=...:out_color_primaries=bt709` gives exact BT.709 too (checked
with ffmpeg n9.0.2).

**What od-sender does.** libva directly (spike: `research/vpp-spike/vpp.c`,
240 parameter combinations in `out.txt`): source `VA_RT_FORMAT_RGB32`
BGRX/RGBX, `surface_color_standard = SRGB`, input range FULL,
`output_color_standard = BT709`, output range REDUCED. Output is
within 1-2 levels of swscale (red 63,102,240; white 235,128,128). Never use
`None` (driver guesses by resolution: BT.601 up to 1280x720). The binding is
hand-written (14 entry points in M2, 16 with M4's import, struct layouts asserted at compile time) and
`dlopen`ed, so the static-FFmpeg tarball neither links nor needs libva. A
startup self-check converts two patches and falls back to swscale if a
driver gets the colours wrong (AMD untested).

**Getting from 46 to 60 fps** (per-frame steps, debug log `RUST_LOG=debug`
from `encoder/steps.rs`):

| Step, 2388x1668 | First version | Final |
|---|---|---|
| Upload | ~5-7 ms (`av_hwframe_transfer_data`) | ~2.5-3 ms (vaDeriveImage, map, row copy) |
| VPP submit | 0.4 ms | 0.4 ms |
| `vaSyncSurface` after VPP | 3.5-4.5 ms | removed (source surface held until its packet is out) |
| Encode (`send_frame`, waits for the GPU) | 8-14 ms | ~12.4 ms |

The serial CPU/GPU round trips also kept the media GT at 450 MHz; it now
runs at ~600 MHz. `quality` 1/4/7 made no measurable difference;
`low_power` is not implemented for H.264 here.

**Measured on the iPad** (Wi-Fi, 30 s windows, two passes, on battery;
`results/2026-11-10-wifi-m2-final/`):

| | CPU cores | fps sent / iPad | e2e p50 / p95 | encode p50 | battery |
|---|---|---|---|---|---|
| VA-API M2, scroll | 0.15 | 59.7 / 59 | 25 / 31 ms | 12.9 ms | ~15.0 W |
| x264, scroll | 2.43 | 58.2 / 58 | 26 / 35 ms | 13.8 ms | ~18.9 W |
| VA-API M2, video | 0.33 | 59.1 / 58.5 | 28 / 39 ms | 16.1 ms | ~13.4 W |
| x264, video | 3.08 | 59.5 / 59 | 31 / 38 ms | 14.3 ms | ~14.4 W |

Sender GPU use: video engine ~43%, VPP engine ~20%. Battery `power_now`
is noisy (the idle baseline read 6.6 W and 9.8 W in the two passes), so
the battery column is indicative only.

**Left on the table:** the upload of frame N+1 could overlap the encode of
frame N (convert-stage thread, ~2.5-3 ms); stage B removes the upload
entirely. The encode at ~600 MHz is now the floor (~12 ms), relevant for
120 fps. Known edge case: if `send_frame` fails after the VPP was queued,
the source surface returns to the pool early (error path only). Destroying
the VPP context makes later CPU-path uploads on the same device fail with
EIO on iHD, so after a GPU failure the context is kept until the encoder
closes (not investigated).

## Build and packaging

* **Source and Arch builds already use the system FFmpeg.** `ffmpeg-sys-next`
  finds FFmpeg through pkg-config unless `FFMPEG_DIR` is set. The in-repo
  `linux/packaging/arch/opendisplay-sender/PKGBUILD` depends on `ffmpeg` and
  builds plainly, so on Arch (whose FFmpeg has VA-API) stage A works with no
  build change. Same for `cargo build --release -p od-sender` from a checkout.
* **The release tarball does not.** It links the minimal static FFmpeg from
  `linux/packaging/ffmpeg/build-static.sh` (autodetect off, only libx264 and
  `h264_v4l2m2m`), so it has no `h264_vaapi`. The script already has
  `FFMPEG_VAAPI=1`, which adds `h264_vaapi` and links libva, libva-drm and
  libdrm dynamically. Turning it on for x86_64 would mean, in
  `linux-release.yml`:
  * `libva-dev libdrm-dev` in the build job's apt packages, and
    `FFMPEG_VAAPI=1` for the x86_64 static build (the cache key hashes the
    script, not the env var, so the key needs it too);
  * libva and libdrm becoming hard runtime dependencies of the tarball (the
    binary would not start without `libva.so.2`). They are on virtually every
    desktop, but that is a maintainer decision;
  * `check-symbol-floor.sh` and `third-party-licenses.py` re-checked with the
    new libraries (libva and libdrm are MIT).

  That belongs in a separate PR, once VA-API is worth enabling by default.
* **Flatpak** only packages the receiver today.

## Stage B: capture into GPU buffers (done in M4, see "M4: zero-copy capture")

The plan was: allocate dmabufs for `ext-image-copy-capture` from the
session's `dmabuf_device`/`dmabuf_format`, hand them out as
`FrameBuf::Dmabuf`, import them as VA surfaces, convert with stage A2's VPP,
and fall back to shm on any failure. That is what M4 does, except the import
goes through libva directly (`vaCreateSurfaces` with DRM PRIME 2) instead of
FFmpeg's `AV_PIX_FMT_DRM_PRIME` mapping: the hand-written libva binding was
already there, and the surface can be cached per buffer.

## Smaller things this surfaced

* **The CPU converter itself.** Independent of VA-API: ~20-25 ms of CPU per
  frame for a conversion that costs ~4-6 ms single-threaded. Worth measuring
  fewer bands, keeping them on P cores, a persistent thread pool instead of
  per-frame scoped threads, and whether `ACCURATE_RND` (which disables some
  swscale fast paths) is needed. This would help x264 users too.
* **Per-frame GPU allocation in the upload** (`clear_pages` under `xe`):
  likely the driver's image path in `av_hwframe_transfer_data`; mapping the
  surface directly might avoid it.
* **The media GT's clocks.** At 400 MHz an H.264 encode of 2388x1668 takes
  ~15-20 ms; a saturating `ffmpeg` encode clocks it higher and does ~5 ms.
  That is most of the latency gap to x264.

## M3: HEVC (2026-11-10)

Branch `linux/hevc`, fork PR #3. Design follows the Mac sender
(`Mac/StreamConfiguration.swift` `preferredCodec`/`make(codec:)`,
`Mac/MacSender.swift` `canEncodeHEVC`, `hevcEncoderFailed`):

* **Probe before planning.** od-sender opens its encoder lazily in the encode
  thread, after `streamConfig` is sent, so the session probes `hevc_vaapi` at
  the planned HEVC stream size first (cached per size). Each encoder opens its
  own VA device, so the throwaway probe cannot hit the iHD "destroyed VPP
  context -> EIO" problem on the live device. `hevc_vaapi` refuses tiny sizes
  on iHD (384x64: EINVAL), so the probe must use the real size.
* **Bitstream.** HEVC type `(b0>>1)&0x3f`; a picture starts at
  `first_slice_segment_in_pic_flag` (MSB of byte 2); suffix SEI (40) stays
  with its picture; only IDR 19/20 are keyframes (CRA is not: the forced-IDR
  check relies on `is_idr`). `hevc_vaapi` with an infinite GOP emits
  VPS/SPS/PPS on each IDR; the sender re-adds them if not.
* **Fallback.** Open failure in the pipeline, or 30 failed encodes in a row,
  is reported once on a channel; the session re-plans H.264 from the last
  hello, replaces the debouncer state, rebuilds and re-announces (~1.1 s).
  Lasts the whole `run`, reconnects included.

**Live** (iPad, Wi-Fi, 30 s, `results/2026-11-10-wifi-m3` two passes,
`-reversed` alternating order on scroll):

| | CPU cores | vcs busy | fps sent / iPad | e2e p50 / p95 | Mbit/s |
|---|---|---|---|---|---|
| H.264, scroll | 0.15 | 43% | 59.6 / 59 | 25-26 / 31-32 ms | 7.7 |
| HEVC, scroll | 0.15-0.16 | 43% | 59.6 / 59 | 25-26 / 32 ms | 7.5-7.7 |
| H.264, video | 0.32-0.33 | 43% | 59.3 / 59 | 29-30 / 39 ms | 28.9 |
| HEVC, video | 0.34-0.35 | 43% | 59.2 / 59 | 28.5 / 39 ms | 26.4 |

The battery reading on scroll was ~4 W higher for whichever codec ran second
(swapping the order swapped it): run-order drift, not codec.

**Offline rate-distortion** (`tools/rd_bench.py`,
`results/2026-11-10-rd-m3`): the same encoders and settings via the ffmpeg
CLI, 360 frames of 2388x1668@60 from a lossless BT.709 reference, CBR sweeps
for the curves and the live QVBR 21 setting with 35/8/4M caps.

| source | BD-rate PSNR-Y | BD-rate VMAF | BD-PSNR | live setting (QVBR 21, 35M cap) |
|---|---|---|---|---|
| code scroll, 1200 px/s | -31% | ~-45% (linear) | +3.7 dB | 1.11 vs 1.41 Mbit/s, +3.1 dB |
| full-screen text flood | -12% | ~-16% (linear) | +1.1 dB | both at the cap, +0.75 dB |
| testsrc2 video | -17% | -35% | +0.9 dB | both at the cap, VMAF 96.4 vs 96.0 |

Quirks: CBR lands at 0.99x the target; the flood cannot go below ~8 Mbit/s
on either codec (caps under that are not honoured); the 35M cap overshoots
to ~35.9. VMAF saturates (>= 99.5) on code scrolling above ~2 Mbit/s, so its
BD numbers come from linear interpolation over few points.

## M4: zero-copy capture (2026-11-10)

Branch `linux/dmabuf-capture` (on `linux/hevc`). Spike first
(`research/dmabuf-spike/`, C: ext-image-copy-capture -> gbm -> VA import ->
VPP), then the Rust path.

**What Hyprland 0.57 offers** (eDP-1, spike; the headless od output ends
up with the same XR24 I915_4_TILED buffers in the live runs; with
and without `paint_cursors`): per format, `shm_format` then `dmabuf_format`
for AR24 and XR24, each with modifiers [LINEAR, I915_X_TILED, I915_4_TILED,
I915_4_TILED_LNL_CCS, INVALID]; `dmabuf_device` 226:128 (the render node
itself); then `buffer_size`, `done`. Async `zwp_linux_buffer_params_v1.create`
is accepted for every modifier (`create_immed` would make a refusal a fatal
protocol error). No implicit fence is pending at `ready` (60/60): Hyprland
has finished its copy, VPP can be queued at once.

**The trap: compressed modifiers.** gbm picks `I915_4_TILED_LNL_CCS` (Xe2
flat CCS) when it is offered. iHD 26.3.5 imports it with
`VA_STATUS_SUCCESS`, VPP succeeds, and the output is **black** (16,128,128),
with no error anywhere; the buffer itself is fine (gbm's mapped read shows
the pixels). So the modifiers go through an allowlist
(`dmabuf::importable_modifier`): on Intel LINEAR, X, Y, Yf and 4 tiling, on
other vendors LINEAR only (untested; compression can fail as silently).
Both capture (before gbm) and the importer (before `vaCreateSurfaces`)
check it. With the filter gbm picks I915_4_TILED.

**How it works.**
* `dmabuf.rs`: libgbm `dlopen`ed like libva (the static tarball neither links
  nor needs it); allocation on the render node behind `dmabuf_device`
  (sysfs mapping for primary nodes); the bo is destroyed after export, the fds
  keep the memory.
* `wayland/capture.rs`: buffers are `Shm` or `Gpu`; GPU ones while
  `CaptureOptions::gpu` (a shared flag the session creates for
  `--encoder vaapi`) says the encoder wants them, else shm. Constraint batches
  are tracked across `done`. `Dmabuf::hold` is a recycler token: the buffer
  is not attached to a new frame until every clone is dropped; the encoder
  keeps one in `InFlight` until the packet is out (VPP has read it by then).
  `Dmabuf::id` is unique per buffer, so the importer caches the VA surface.
* `encoder/vpp.rs`: `convert_dmabuf` imports (BGRX for XR24/AR24, RGBX for
  XB24/AB24; iHD reads only the VA fourcc, the layer's DRM format is set to
  the X variant anyway), caches up to 8 surfaces by id, runs the same VPP
  colour parameters. First import ~0.1-0.9 ms, then free.
* Fallback, per run like the HEVC one: the encoder declines GPU frames when it
  has no GPU conversion; a GPU frame that fails to encode latches
  `GPU_CAPTURE_FAILED`; capture then frees its buffers and **recreates the
  session** on shm, so even a static screen gets a fresh first frame
  (verified with the debug-only `OD_FAIL_DMABUF_IMPORT=1`). Capture also drops
  to shm by itself after 3 failed frames on GPU buffers or a failed
  allocation/`wl_buffer`. `OD_VAAPI_SHM_CAPTURE=1` forces shm for A/B.
* wlr-screencopy (Hyprland < 0.57 under `auto`) and PipeWire (KDE, GNOME)
  stay on shm.

**Colours.** Flat-patch tests through gbm buffers, both byte orders, LINEAR
and 4-tiled: red 63,102,240, blue 32,240,118, exact; end to end through
H.264 and HEVC decode too (`vaapi_dmabuf_*` tests). A frame with a CCS
modifier is refused, and the refusal leaves the shm GPU path working.

**Measured** (iPad, Wi-Fi, HEVC, two passes, `results/2026-11-10-wifi-m4/`):

| | CPU cores | CPU per frame | encode p50 | e2e p50 / p95 |
|---|---|---|---|---|
| M4 scroll | 0.076-0.079 | 1.3 ms | 10.8-11.6 ms | 23.5-26 / 29.5-34 ms |
| M3 scroll | 0.153-0.157 | 2.6 ms | 13.0-13.1 ms | 26.5-27.5 / 33.5 ms |
| M4 video | 0.222-0.246 | 3.9 ms | 12.9-13.3 ms | 29 / 37 ms |
| M3 video | 0.342-0.351 | 5.8 ms | 16.1-16.2 ms | 27.5-29 / 39.5-40 ms |

59 fps on the iPad throughout. The compositor's render engine is less busy
too (scroll 27 vs 35 %). Battery follows run order again; no difference
readable.

**Where the CPU still goes** (perf, video, ms per frame): iHD encode submit
and the kernel ioctls under `avcodec_send_frame` 1.37; VPP submit 0.92 (on
imported tiled surfaces 0.62-0.99 ms against 0.46-0.58 ms on shm surfaces,
likely the dma-resv fence work for external buffers); timer interrupts 0.55;
od's NAL processing (`normalize_start_codes`, `access_units_for`) 0.29;
FFmpeg's copy of the bitstream out of the coded buffer 0.26 (scales with
bitrate: why video costs more than scroll). The capture->VPP part is now
~1 ms; the rest is the encoder driver's own CPU, not pixels.

**120 fps (M5 groundwork)** against `od-receiver --refresh 120`
(`results/2026-11-10-m5-120fps-local/`, H.264, alternated): with dmabufs
114-117 fps, encode p50 7.0-7.5 ms, p95 8.0-8.2 ms, media GT at **1200 MHz**
(its max); with shm only 49-59 fps and the media GT at 0-500 MHz (the GPU
idles while the CPU uploads, and the governor never clocks up). Zero-copy is
what makes 120 fps possible here; p95 is right at the 8.3 ms budget.

**Open.** HEVC at 120 fps (no receiver offers it at 120 yet); AMD and other
vendors (LINEAR only, untested); multi-GPU (gbm allocates on the
compositor's device, VA opens the default one; an import failure falls back
to shm); a `wl_buffer` whose async `create` answers after the 1 s timeout
leaks (rare); evicting a cached surface does not wait for a VPP still
reading it (only after a resize).

## Future work this enables

* **HEVC** (`hevc_vaapi`): done in M3 (Main), see "M3: HEVC" above. 4:4:4
  needs an iPad app change and a protocol field (PLAN M3).
* **AV1** (#312): Lunar Lake decodes AV1; encode depends on the GPU.
* **NVENC** (`h264_nvenc`/`hevc_nvenc`) for NVIDIA, where VA-API has no
  encode: the same FFmpeg backend shape, and it accepts system-memory NV12.
* **V4L2 M2M** could use the same "hardware waits, so stage always"
  `stage_min_width()` once someone measures it.
* **Device choice** on multi-GPU machines (a config key passed to
  `av_hwdevice_ctx_create`).

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

### The convert stage for hardware encoders

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

## Stage A2 (next): colour conversion on the GPU

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

## Stage B (not started): capture into GPU buffers

What removes the remaining CPU work (the BGRA upload as well):

1. Capture: allocate dmabufs (gbm, or `zwp_linux_dmabuf_v1` feedback) for
   `ext-image-copy-capture` using the session's `dmabuf_device` and
   `dmabuf_format` events (today `wayland/capture.rs` ignores them), and
   produce `FrameBuf::Dmabuf` (the variant exists, unused);
   `backend::Caps::dmabuf` already exists to report support.
2. Import: wrap the dmabuf as an `AV_PIX_FMT_DRM_PRIME` frame and map it to a
   VA surface (`av_hwframe_map`), as wl-screenrec does.
3. Convert on the GPU (stage A2, same colour question).
4. Fallback: any failure drops back to shm and stage A.

Open questions: modifiers and tiling Hyprland hands out versus what the
encoder imports; the cursor, painted into the video on Hyprland today, has to
come with the dmabuf too; how this fits the PipeWire path (KDE and GNOME give
dmabufs too).

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

## Future work this enables

* **HEVC** (`hevc_vaapi`): PROTOCOL.md already negotiates it (`videoCaps`,
  `streamConfig.codec`), the Mac sender uses it, and the iOS receiver offers
  it. This GPU encodes HEVC Main/Main10 and 4:4:4 (#313).
* **AV1** (#312): Lunar Lake decodes AV1; encode depends on the GPU.
* **NVENC** (`h264_nvenc`/`hevc_nvenc`) for NVIDIA, where VA-API has no
  encode: the same FFmpeg backend shape, and it accepts system-memory NV12.
* **V4L2 M2M** could use the same "hardware waits, so stage always"
  `stage_min_width()` once someone measures it.
* **Device choice** on multi-GPU machines (a config key passed to
  `av_hwdevice_ctx_create`).

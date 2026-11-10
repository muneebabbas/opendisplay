# Plan: GPU encoding (VA-API, then HEVC) for od-sender on the Zenbook

Owner's priority: **the best performance on my own machine**, an ASUS Zenbook
with an Intel Core Ultra 5 226V (Lunar Lake, Arc 130V / Xe2, iHD driver),
extending onto an iPad (2388x1668, 120 Hz panel) mostly over USB.
Contributing upstream is a later, separate decision, piece by piece, once
something is genuinely useful (see "Upstreaming" below).

## Goal and targets

Move the whole video path onto the GPU: capture -> colour conversion ->
encode, with the CPU only handling the compressed stream.

| Target | x264 | VA-API, stage A (CPU convert) | VA-API, M2 (GPU convert) | Goal |
|---|---|---|---|---|
| Sender CPU, 60 fps full-motion video | 3.08 cores | 2.16 cores | **0.33 cores** | **< 0.3 cores** |
| Sender CPU, fast scrolling | 2.43 cores | 1.67 cores | **0.15 cores** | **< 0.2 cores** |
| End-to-end latency p50 (iPad), video | 30 ms | 44 ms | **28 ms** | **<= x264** |
| fps under full motion | 59.5 | 46 | **59** | **60**, later **120** |
| Colours | exact BT.709 | exact BT.709 | exact BT.709 (tests) | **exact BT.709** (flat-patch test) |
| Battery draw, scroll / video (idle ~6.6 W) | 18.9 / 14.4 W | 17.1 / 14.1 W | 15.0 / 13.4 W | measured, clearly lower |
| Bits at equal quality, HEVC vs H.264 (M3) | | | BD-rate -31% code scroll, -17% video | **lower** |

Numbers (2026-11-10, iPad over Wi-Fi, 30 s windows, on battery): x264 and M2 from
`results/2026-11-10-wifi-m2-final/` (two passes, consistent); stage A from
`results/2026-11-10-wifi-m1/`. Battery `power_now` is noisy (the idle baseline
moved 6.6 -> 9.8 W between passes): treat it as indicative only. Method in
`research/vaapi-encode.md`.

## Milestones

Each milestone is a topic branch with a draft PR in the fork.

### M0 Base setup (done, 2026-11-09)
* `linux/vaapi-encoder`: opt-in VA-API H.264 encoder, hardware encoders
  always use the convert stage, docs. Three commits on upstream v1.30.2.
* This branch, the iPad harness, the design doc.

### M1 Measurement baseline (done, 2026-11-10)
* Harness: Wi-Fi target (`HOST:PORT`), battery `power_now`, GPU engine busy from
  DRM fdinfo (`intel_gpu_top` does not support Xe; RAPL is root-only), idle
  baseline line. Baseline in `results/2026-11-10-wifi-m1/`.
* Measure **battery draw** (`/sys/class/power_supply/BAT0/power_now`,
  unplugged, iPad not charging from the laptop if possible) for idle /
  scroll / video, x264 vs VA-API. Battery is the real goal; CPU time is a proxy.
* Extend `tools/ipad_bench.py`: battery power, GPU busy and power via
  `intel_gpu_top -J`, keep whole-process CPU (never sum `/proc/<pid>/task`).
* Write the baseline into `results/` and the target table above.

### M2 GPU colour conversion (stage A2): the big CPU win (done, 2026-11-10)
* Done on `linux/vaapi-gpu-convert` (6 commits on `linux/vaapi-encoder`): libva VPP
  via a hand-written, dlopen'd FFI; exact BT.709; direct surface-mapping upload;
  no CPU/GPU sync per frame. 60 fps at 0.15-0.33 cores. Details in
  `research/vaapi-encode.md` "Stage A2: result". Not yet in `auto`.
Branch `linux/vaapi-gpu-convert`, stacked on `linux/vaapi-encoder`.
* Upload the captured BGRA to a VA surface and convert to NV12 with VA-API
  video processing (VPP) instead of swscale. CLI experiment: ~15 ms ->
  ~2.5 ms of CPU per frame.
* **Blocker to solve first:** iHD's VPP via FFmpeg's `scale_vaapi` outputs
  BT.601 whatever is requested (`research/vaapi-encode.md`, "Stage A2").
  Try libva directly with `VAProcPipelineParameterBuffer` output colour
  standard / properties set to BT.709 limited. Fallbacks: a GPU shader;
  or BT.601 in the VUI if receivers honour it.
* Decide: libavfilter (`scale_vaapi`) vs direct libva FFI.
* Make the flat-patch colour check a test (skips without a GPU).
* Look at the per-frame GPU allocation in the upload (`clear_pages` under
  `xe` in perf).
* Done when: the M1 video workload is well under 1 core at 60 fps with exact
  colours. Then VA-API can join `auto` (in this fork at least).

### M3 HEVC (done, 2026-11-10)
Branch `linux/hevc` (3 commits on `linux/vaapi-gpu-convert`), fork draft PR #3.
* With `--encoder=vaapi`, HEVC is picked automatically when `hello.videoCaps`
  offers it and `hevc_vaapi` opens at the stream size (probe cached per size),
  like the Mac sender (PROTOCOL 6.5). HEVC plans skip the legacy ceiling and
  the H.264 level limits. Late failure (open fails in the pipeline, or 30
  failed encodes) re-plans as H.264 for the rest of the run, reconnects
  included (Mac: `hevcEncoderFailed` on `MacSender`, never reset).
* Knobs: `OD_FORCE_H264=1` (release, for A/B); debug-only
  `OD_FAIL_HEVC_ENCODER=1` / `OD_FAIL_HEVC_SESSION=1` (Mac's
  `failHEVCEncoder` / `failHEVCSession`). `doctor` reports HEVC.
* Verified on the iPad: PHONE-STATS `"codec":"HEVC"`, 59 fps; forced H.264,
  probe failure, late fallback, reconnect, and a receiver without HEVC.
* **Result:** live, HEVC equals H.264 on CPU (0.15 / 0.33 cores), video-engine
  time, fps and latency (`results/2026-11-10-wifi-m3*`). The win is quality
  per bit (`results/2026-11-10-rd-m3`): BD-rate PSNR-Y -31% on code
  scrolling, -12% on a full-screen text flood, -17% on video; at the live
  setting (QVBR 21) code scrolling takes 1.11 vs 1.41 Mbit/s and is +3.1 dB.
* Later: **HEVC 4:4:4** (sharper coloured text). The iGPU encodes it, but the
  iPad app asks VideoToolbox for NV12 (`Shared/StreamReceiver.swift`,
  `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`) and renders NV12, so it
  would subsample again; `videoCaps` has no profile/chroma field, and
  `"hevc"` means Main in practice. Needs an app change (Xcode on a Mac) and a
  protocol field; `od-receiver` could test the sender side first. SCC: likely
  no VideoToolbox decode (unverified).
* Open: the live `foot`/`seq` scroll bench sends ~7.7 Mbit/s for both codecs
  at QVBR 21, while the synthetic flood hits the 35M cap: the live content is
  much easier than the synthetic one; not investigated.

### M4 Zero-copy capture (stage B)
Branch `linux/dmabuf-capture`, on M2.
* Capture into dmabufs (`ext-image-copy-capture` dmabuf constraints, gbm),
  import as VA surfaces, convert and encode without touching the CPU.
  Fallback to shm. Cursor, modifiers and Hyprland quirks are the risks.
* Done when: CPU per frame ~0.5-1 ms; this is what makes 120 fps realistic.

### M5 120 fps (stretch)
* The iPad app caps H.264 and HEVC at 60 fps (`Shared/StreamReceiver.swift`
  `maxFrameRate: 60`, `iOS/OpenSidecarPhoneApp.swift`). 120 on the iPad
  needs an app change (upstream, or an Xcode build on a Mac).
* The Linux receiver advertises its monitor's refresh: test 120+ fps
  against `od-receiver` on the desktop (144 Hz monitor) first.
* Risk: encode must take < 8.3 ms per frame; the media engine runs at
  400 MHz under today's load (~15-20 ms). Check that it clocks up at 120 fps.

## Parked (written down, not planned)

* **CPU converter** (helps x264 users): banded swscale costs ~20-25 ms CPU
  per frame (per-frame scoped threads on all 8 cores incl. LP E cores,
  `ACCURATE_RND`); upstream has an unmerged P-core-only change
  (`upstream/linux/pcore-threads`). Ideas: measure 1 vs N bands, thread
  pool, P cores only, damage-only conversion, `yuvutils-rs`. Not needed once
  the GPU path works.
* **NVENC** for the desktop (RTX 3080): VA-API cannot encode on NVIDIA.
* **Release tarball with VA-API**: `build-static.sh` has `FFMPEG_VAAPI=1`;
  CI and runtime libva dependency are a maintainer decision. Unverified
  locally (the static build run was interrupted).
* **Monitor layout warning**: `~/.config/hypr/hyprmon.lua` places
  `od-a0ec23` at 224x900, overlapping the laptop screen where
  `monitors.lua` puts it (0x352); Hyprland warns on every connect.
  User config, not od-sender.

## Upstreaming (later)

Only once a unit is genuinely useful. Likely order: VA-API + GPU conversion
(M0 + M2) -> static build change -> zero-copy -> HEVC. Each from a clean
branch on upstream `main`, with tests, user docs, and measurements made
with upstream's own tools (`pipeline-bench`, `encode-bench`, `doctor`).
Upstream squash-merges, so the PR title must be a good Conventional Commit.
Nothing from this `fork` branch goes in.

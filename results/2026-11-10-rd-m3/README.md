# RD bench: VA-API H.264 vs HEVC on desktop-like content (M3)

Offline comparison on this machine (Intel Lunar Lake, Intel iHD driver 26.3.5 (VA-API 1.24), `/dev/dri/renderD128`, ffmpeg version n9.0.2).
Question: how many bits does HEVC save over H.264 at equal quality for scrolling content?

## Reproduce

```
cd ~/projects/opendisplay-fork
python3 -I tools/rd_bench.py --work ~/odrd-work --out results/2026-11-10-rd-m3
```
Total runtime about 15 min of encode+measure for the first pass plus 3 min for the final re-run (360 frames, full VMAF; about 9-14 s per VMAF+PSNR+SSIM pass).
`--dry-run` prints every ffmpeg command; the exact commands used are in `commands.txt` (same paths, `~/odrd-work` expanded). Scratch (`~/odrd-work`, ~1.1 GB) is deleted after the run.

Encoder command (per run; `<RC>` is below):
```
ffmpeg -init_hw_device vaapi=va:/dev/dri/renderD128 -filter_hw_device va -i ref.mkv -vf 'format=nv12,hwupload' \
  -c:v {h264_vaapi|hevc_vaapi} -bf 0 -g 100000 -async_depth 1 -sei 0 [-profile:v main for hevc] <RC> -f {h264|hevc} out.bin
Sweep A: -rc_mode CBR -b:v R -maxrate R -bufsize R/4
Sweep B: -global_quality 21 -b:v C -maxrate C -bufsize C/4        (C = 35M, 8M, 4M)
```
Measured: decode raw stream with software ffmpeg, compare against the lossless reference in one graph (`psnr`, `ssim`, `libvmaf` default model, `n_threads=8`), both sides yuv420p BT.709 limited, no RGB conversion. PSNR is the Y average from the psnr filter (pooled over frames, i.e. from mean MSE), SSIM is "All", VMAF is the pooled mean. Bitrate is `file size * 8 / 6 s`. All 360 frames were compared for every run.

## Sources

All 2388x1668, 60 fps, 360 frames (6 s), converted once to yuv420p BT.709 limited (`scale=out_color_matrix=bt709:out_range=tv`) and stored as FFV1 mkv; both codecs encode the same reference.

- S1 `scroll-code`: first 420 lines (truncated to 140 columns) of od-sender `session.rs` + `desktop.rs`, drawn with drawtext (CaskaydiaMono Nerd Font Mono, 28 px, #d4d4d4 on #1e1e1e) into a 2388x14000 image, cropped by a window moving down 1200 px/s (20 px/frame, integer pixels).
- S2 `scroll-flood`: imitation of `seq` flooding a terminal: 700 lines of 17 consecutive 7-digit numbers (same font) filling the width, window moves 1360 px (about 41 lines) per frame and wraps every 21400 px (about 16 frames), so nearly all of the screen changes every frame. Content repeats every ~16 frames but nothing references it (no long-term refs), so it behaves as non-repeating noise-like text.
- S3 `video`: `testsrc2=size=2388x1668:rate=60`.

## Results

Sweep A (CBR) rates (kbit/s): S1 250..6000 (8 points; the default 2-24 Mbit/s ladder saturates at VMAF>99 for this source), S2 8000..48000 (6 points; the CBR target is not reached below ~8 Mbit/s, see quirks), S3 2000..24000 (7 points). Full data in `rd.csv`, charts in `rd-<source>-{vmaf,psnr}.svg`.

### BD-rate, HEVC vs H.264 (negative = HEVC saves bits)

| source | BD-rate PSNR-Y | BD-rate SSIM (dB) | BD-rate VMAF | BD-PSNR-Y |
|---|---|---|---|---|
| scroll-code | -31.4% (lin -31.9%) | n/a (linear-interp: -56.0%) | n/a (linear-interp: -44.9%) | +3.72 dB |
| scroll-flood | -11.8% (lin -12.0%) | -20.9% (lin -21.3%) | n/a (linear-interp: -15.8%) | +1.08 dB |
| video | -16.8% (lin -16.2%) | -48.0% (lin -47.7%) | -34.9% (lin -35.5%) | +0.89 dB |

- Cubic fit of log10(rate) vs metric, least squares via 4x4 normal equations, over the overlapping metric interval; `(10^d-1)*100`. SSIM is mapped to dB (`-10 log10(1-SSIM)`) first, because raw SSIM near 1.0 breaks cubic fits. For VMAF, points with VMAF >= 99.5 are dropped (saturated); S2 then has only 3 points per codec and S1 only 5 (with a plateau), so no cubic fit is valid.
- "n/a" means the cubic was rejected (fewer than 4 points, or not monotonic on the overlap, i.e. ill-conditioned); the piecewise-linear interpolation BD ("lin", a cross-check computed for every cell) is quoted instead. Where the cubic is valid the two agree within ~1 point.
- BD-PSNR is the average PSNR gain at equal rate over the overlapping log-rate range (positive = HEVC better).
- Reading it: about 12% (flood) to 17% (video) to ~31-56% (code scroll, strongly metric dependent) fewer bits for HEVC at equal quality. S1 is the least certain: the quality range there spans very low rates (250 kbit/s) where both codecs are far from the live operating point.

### Sweep B, the live setting (`-global_quality 21`, caps 35M / 8M / 4M)

| source | codec | cap | actual kbit/s | PSNR-Y | SSIM | VMAF |
|---|---|---|---|---|---|---|
| scroll-code | h264 | 35000k | 1413 | 51.442 | 0.99975 | 99.992 |
| scroll-code | h264 | 8000k | 1346 | 50.512 | 0.99955 | 99.984 |
| scroll-code | h264 | 4000k | 1333 | 46.345 | 0.99834 | 99.918 |
| scroll-code | hevc | 35000k | 1114 | 54.581 | 0.99992 | 99.993 |
| scroll-code | hevc | 8000k | 985 | 52.834 | 0.99987 | 99.983 |
| scroll-code | hevc | 4000k | 964 | 48.821 | 0.9997 | 99.904 |
| scroll-flood | h264 | 35000k | 35963 | 32.06 | 0.98869 | 99.97 |
| scroll-flood | h264 | 8000k | 8311 | 20.083 | 0.81315 | 55.113 |
| scroll-flood | h264 | 4000k | 8070 | 19.177 | 0.79426 | 46.172 |
| scroll-flood | hevc | 35000k | 35956 | 32.814 | 0.99379 | 99.982 |
| scroll-flood | hevc | 8000k | 8219 | 22.059 | 0.89833 | 73.282 |
| scroll-flood | hevc | 4000k | 7638 | 21.595 | 0.88637 | 70.61 |
| video | h264 | 35000k | 35886 | 46.683 | 0.99625 | 95.999 |
| video | h264 | 8000k | 8201 | 36.823 | 0.97718 | 82.732 |
| video | h264 | 4000k | 4052 | 34.748 | 0.96528 | 74.315 |
| video | hevc | 35000k | 35894 | 46.684 | 0.99759 | 96.415 |
| video | hevc | 8000k | 8203 | 37.349 | 0.98491 | 86.472 |
| video | hevc | 4000k | 4104 | 35.911 | 0.9792 | 80.848 |

Per-point numbers for sweep B are single operating points, not equal-quality: compare rate and metrics side by side per codec.

### Rate control actually used

ffmpeg log (`-v verbose`) for sweep A: `RC mode: CBR`; for sweep B (no `-rc_mode`): `RC mode: QVBR` on both h264_vaapi and hevc_vaapi (so CBR was accepted by the driver for both codecs and VBR was not needed). `Block Level bitrate control: OFF` in all runs. No encoder warnings were logged.

## Quirks / caveats

- CBR actual rate is consistently 0.9896x of the target (e.g. 3958 kbit/s for 4M) for both codecs and all sources.
- S2 (flood): CBR/QVBR cannot go below a floor, 2M/4M/6M targets produce about 8.0-8.3 Mbit/s on h264 and 7.6-8.2 Mbit/s on hevc (HEVC 2M, 4M and 6M give identical bit-exact-size output, 7638 kbit/s). So the 4M cap in sweep B is not honoured (h264 8070, hevc 7638 kbit/s) and the 8M cap lands at 8.2-8.3 Mbit/s. Those low-rate points were dropped from the S2 sweep A ladder. Quality there is very low (PSNR-Y about 20-22 dB, VMAF 46-74): the live flood content cannot be carried at constrained Wi-Fi rates at acceptable quality by either codec.
- S3 h264 CBR 2M produces 2840 kbit/s (same floor effect); the actual rate is used for all analysis.
- S1 (code scroll) is very compressible: QVBR q21 needs only ~1.0-1.4 Mbit/s, the caps never bind, and quality at q21 (PSNR-Y 46-55 dB, VMAF 99.9+) is far above the CBR sweep points of the same rate; the BD ranges therefore cover mostly lower-quality territory than the live setting. Sweep B for S1 shows the live-relevant gain more directly: HEVC 0.96-1.11 Mbit/s vs H.264 1.33-1.41 Mbit/s (about 21-28% fewer) with 2.3-3.1 dB higher PSNR-Y.
- On S3 at cap 35M both codecs run at the cap (about 35.9 Mbit/s, slightly above it) with nearly identical PSNR; the saving there is only visible at lower caps/rates.
- Frame alignment gotcha found while developing: the mkv reference stores millisecond timestamps, so naive framesync of a raw 60 fps stream against it misaligns some frames (bogus PSNR of about 21 dB). Both inputs are retimed with `setpts=N/(60*TB)` in the final graph. Sanity check on S1: PSNR(ref, ref) = inf; PSNR(ref, ref shifted by 1 frame) = 17.16 dB Y (18.92 average), so alignment matters and is correct.
- One 6 s clip per source, one run per point (encoder is deterministic here but this is not a statistical sample). Synthetic content; real code editing, anti-aliasing and subpixel differences from the live compositor are not modelled. CBR is not what the live sender uses; sweep A is only for building equal-quality curves.
- Raw Annex B streams carry no VUI colour info from the encoder; the colour tags only exist on the reference, and metrics are computed in yuv420p with no conversion.

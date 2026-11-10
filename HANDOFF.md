# Handoff: where to pick up

Last updated 2026-11-10 (end of M4). Update this file at the end of every session.

## State

* **Code:** four stacked topic branches on upstream v1.30.2 (`0f44853`, still
  upstream's latest on 2026-11-10):
  * `linux/vaapi-encoder` (M0): opt-in VA-API H.264 encoder, 3 commits.
    Draft PR https://github.com/muneebabbas/opendisplay/pull/1
  * `linux/vaapi-gpu-convert` (M2), on top: 6 commits. GPU colour conversion
    through libva VPP. Draft PR #2.
  * `linux/hevc` (M3), on top: 3 commits. HEVC over VA-API when the receiver
    offers it. Draft PR https://github.com/muneebabbas/opendisplay/pull/3
  * `linux/dmabuf-capture` (M4), on top: 2 commits. Zero-copy capture into
    dmabufs imported as VA surfaces. Draft PR
    https://github.com/muneebabbas/opendisplay/pull/4
  Nothing has been sent upstream. CI does not run on these stacked fork PRs;
  the upstream checks were run locally before each commit.
* **M1-M4 are done.** `--encoder=vaapi` on Hyprland 0.57 now captures into
  GPU buffers (XR24 I915_4_TILED): 2388x1668 HEVC at 60 fps for **0.08
  cores scrolling, 0.23 on video** (M3: 0.155 / 0.35), encode p50 2-3 ms
  shorter, e2e at or below M3, exact BT.709 (tests through dmabufs), cursor
  painted into the GPU frames (checked pixel by pixel), fallback to shm
  verified. Details: PLAN M4, `research/vaapi-encode.md` "M4: zero-copy
  capture", `results/2026-11-10-wifi-m4/`.
* **Per-frame CPU vs the M4 goal (0.5-1 ms):** the pixel path (capture ->
  import -> VPP submit) is ~1 ms; the whole process is 1.3 ms/frame
  scrolling and 3.9 ms/frame on video, the rest being iHD's encode submit,
  interrupts, the bitstream copy and od's NAL parsing (perf in the research
  notes). Not something capture can remove.
* **120 fps groundwork (M5):** against `od-receiver --refresh 120` dmabuf
  capture sends 115 fps H.264 with encode p50 7.1 / p95 8.1 ms and the media
  GT at 1200 MHz; shm capture stalls at ~50 fps (media GT never clocks up).
  HEVC at 120 untested; the iPad app caps at 60.
* VA-API (and so HEVC and GPU capture) is still opt-in (`--encoder=vaapi`).
* **Not yet checked by eye:** the stream on the iPad itself since M3 (stats,
  decode and colour tests are clean). Look at it once.
* **Pending decision (owner):** the branch/release model proposed on
  2026-11-10 (`main` mirror, a `release` integration branch taking fork PRs as
  merges, tags there, upstream merged in, topic branches rebased for
  upstream PRs). Not set up; ask before doing any of it.

## Next step

Options (agree with the owner first):

1. **M5 120 fps:** needs an iPad app change (`maxFrameRate: 60`), or test
   HEVC at 120 with a receiver that offers it; encode p95 sits right at the
   8.3 ms budget for H.264.
2. **VA-API in `auto`** (then HEVC and zero-copy are automatic for everyone
   with a VA-API GPU).
3. Remaining CPU: od's NAL processing (~0.3 ms/frame, `normalize_start_codes`
   copies), the VPP submit on imported surfaces (0.6-1.0 vs 0.5 ms on shm).
4. Dmabufs over wlr-screencopy (Hyprland < 0.57) and PipeWire (KDE/GNOME);
   AMD (LINEAR only, untested).

## Resume on any machine

```bash
git clone git@github.com:muneebabbas/opendisplay.git ~/projects/opendisplay
cd ~/projects/opendisplay
git remote add upstream git@github.com:peetzweg/opendisplay.git
git remote set-url --push upstream no_push
git fetch upstream
git worktree add ../opendisplay-fork fork
git switch linux/vaapi-encoder
cd linux && cargo build --release -p od-sender     # links the system FFmpeg
./target/release/od-sender doctor                  # lists "VA-API (hardware): ok"
```

Keep up with upstream: `~/projects/opendisplay-fork/tools/sync-upstream.sh`
(add `--push` to update GitHub). Add new topic branches to its `STACKS`.

## Testing recipes

* **Checks before committing** (what upstream CI runs), in `linux/`:
  `cargo fmt --all --check`, `cargo clippy --locked --workspace --all-targets
  -- -D warnings`, `cargo test --locked --workspace`,
  `cargo test --locked -p od-sender --no-default-features`.
  VA-API tests: `cargo test -p od-sender vaapi`.
* **iPad over USB** (no Wi-Fi variance). Disconnect it in the bar panel
  first, keep the OpenDisplay app in the foreground, then:
  ```bash
  python3 -I ~/.config/omarchy/plugins/io.github.muneebabbas.opendisplay/bin/usbmux.py \
    forward --listen 127.0.0.1:47000 --udid 00008027-0014490A1AE8002E &
  ./target/release/od-sender run --to=127.0.0.1:47000 --encoder=vaapi --workspace=9
  ```
  The iPad's own stats arrive in the sender log as `PHONE-STATS` (fps,
  e2e50/95, hitches, stalls).
* **iPad over Wi-Fi**: `od-sender list` shows its address (2026-11-10:
  `10.93.10.226:9000`); `od-sender run --to=10.93.10.226:9000 --encoder=vaapi
  --workspace=9`. No charging from the laptop, so battery numbers are cleaner;
  latency is a bit noisier than USB.
* **A/B harness:** `python3 -I ~/projects/opendisplay-fork/tools/ipad_bench.py
  <od-sender> <PORT|HOST:PORT> <outdir> x264,vaapi idle,scroll,video 30`.
  Copy the binary to `/tmp` first so a rebuild cannot change it mid-run, and
  do not compile while it runs (skews CPU and battery). Records an idle
  battery baseline, `bat_w_*`, and GPU engine busy from DRM fdinfo
  (`gpu_*` system-wide, `sender_gpu_*` the sender; vcs = encode, vecs = VPP).
* **Codec A/B:** harness variants `vaapi-h264` (`OD_FORCE_H264=1`) and
  `vaapi-hevc`; the result rows carry the iPad's `codec`. Debug builds take
  `OD_FAIL_HEVC_ENCODER=1` / `OD_FAIL_HEVC_SESSION=1` to test the fallbacks.
* **Reconnect test without touching the iPad:** `results/2026-11-10-m3-functional/proxy.py
  127.0.0.1:47001 <ipad>`, point the sender at the proxy, `pkill -USR1 -f
  '^python3 -I .*proxy.py'` cuts the link.
* **Rate-distortion:** `python3 -I tools/rd_bench.py --work ~/odrd-work --out
  results/<dir>` (~20 min; libvmaf; `--dry-run` prints the ffmpeg commands).
* **Where a frame's time goes:** `RUST_LOG=debug` logs per-step averages
  (import or upload, vpp submit, send_frame, receive) every 2 s.
  `OD_VAAPI_CPU_CONVERT=1` forces the old swscale path,
  `OD_VAAPI_SHM_CAPTURE=1` shared-memory capture (the M3 path), for A/B.
  Debug builds: `OD_FAIL_DMABUF_IMPORT=1` tests the fallback to shm.
* **GPU capture without the encoder:** `od-sender dev capture-test --output
  eDP-1 --gpu [--cursor] [--gpu-linear --save x.ppm]` (linear buffers can be
  saved; the normal 4-tiled ones cannot be read by the CPU).
* **M3 vs M4 in one run:** the harness takes `<variant>@<binary>`
  (e.g. `vaapi-hevc@/tmp/od-sender-m3`) and `vaapi-hevc-shm`.
* **Profiling:** build with symbols without editing files:
  `CARGO_TARGET_DIR=/tmp/od-sym CARGO_PROFILE_RELEASE_STRIP=none cargo build
  --release -p od-sender`, then `perf record -g -p <pid>`.
* **Without the iPad:** `od-receiver --listen 127.0.0.1:9100 --width 2388
  --height 1668 --scale 2 --refresh 60` (needs `gst-plugin-va` /
  `gst-libav` to decode; `--sink none` skips decoding).

## Gotchas learned the hard way

* **CPU measurement:** use `/proc/<pid>/stat` (whole process). Summing
  `/proc/<pid>/task/*/stat` misses the converter's per-frame threads; it
  made VA-API look 3x better than it is.
* **Hyprland 0.57 is Lua-configured:** `hyprctl dispatch exec "[rules] cmd"`
  errors in Lua. Use
  `hyprctl eval 'hl.exec_cmd([[cmd]], { workspace = "9 silent" })'`.
* **`pkill -f <name>` / `pgrep -f`** match your own shell if its command line
  contains the name. Anchor patterns (`^mpv .*-title=odbench`).
* **Shell aliases:** `cp` and `rm` are interactive on this machine; use
  `command cp -f` in scripts.
* **zsh** doesn't word-split `$VAR`; run multi-flag ffmpeg lines under `bash -c`.
* **Overlap warning on connect:** from `~/.config/hypr/hyprmon.lua` vs
  `monitors.lua` (see PLAN "Parked"), not od-sender.
* **`intel_gpu_top` does not support Xe** (Lunar Lake); RAPL energy is
  root-only. GPU busy comes from `/proc/<pid>/fdinfo` `drm-cycles-*`.
* **Battery `power_now` is noisy:** the idle baseline read 6.6 W and 9.8 W
  in two passes minutes apart. Repeat runs; trust CPU and fps first.
* **iHD Explicit colour mode** reads only `colour_primaries`; never send
  VPP `None` either (it guesses by resolution). See research doc.
* **Destroying a VPP context on iHD** made later CPU-path uploads on the
  same device fail with EIO; the code keeps it alive after a GPU failure.
* **Battery order effect:** in an A/B the second run of a workload can read
  ~4 W higher regardless of encoder; alternate the order before believing a
  power difference.
* **`hevc_vaapi` on iHD rejects tiny sizes** (384x64: EINVAL); probe at the
  real stream size.
* **iHD + compressed modifiers:** `I915_4_TILED_LNL_CCS` (gbm's default
  pick on Lunar Lake) imports and converts without an error into a black
  picture. Keep the modifier allowlist (`dmabuf::importable_modifier`).
* **Moving the pointer on Hyprland 0.57:** `hyprctl dispatch
  'hl.dsp.cursor.move({ x = 600, y = 400 })'` (`movecursor X Y` errors).
* **Agent worktree isolation** in this T3 project is tied to the
  `omarchy-opendisplay` repo: for parallel agents make worktrees of
  `~/projects/opendisplay` by hand (`git worktree add ../od-wt-x <commit>`).
* **Tools installed for this work:** `perf intel-gpu-tools gst-plugin-va
  gst-libav nasm`. `kernel.perf_event_paranoid` was set to 0 for profiling;
  it resets to 2 on reboot (or `sudo sysctl kernel.perf_event_paranoid=2`).

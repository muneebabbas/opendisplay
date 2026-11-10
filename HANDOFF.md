# Handoff: where to pick up

Last updated 2026-11-10. Update this file at the end of every session.

## State

* **Code:** three stacked topic branches on upstream v1.30.2 (`0f44853`, still
  upstream's latest on 2026-11-10):
  * `linux/vaapi-encoder` (M0): opt-in VA-API H.264 encoder, 3 commits.
    Draft PR https://github.com/muneebabbas/opendisplay/pull/1
  * `linux/vaapi-gpu-convert` (M2), on top: 6 commits. GPU colour conversion
    through libva VPP. Draft PR #2.
  * `linux/hevc` (M3), on top: 3 commits. HEVC over VA-API when the receiver
    offers it. Draft PR https://github.com/muneebabbas/opendisplay/pull/3
  Nothing has been sent upstream.
* **M1, M2 and M3 are done.** `--encoder=vaapi` streams 2388x1668 at 60 fps
  at 0.15 (scroll) / 0.33 (video) cores, latency at or below x264, exact
  BT.709, and now **HEVC** to the iPad (PHONE-STATS `"codec":"HEVC"`).
  HEVC costs the same as H.264 live; it wins on quality per bit: BD-rate
  PSNR-Y -31% on code scrolling, -17% on video (PLAN M3,
  `research/vaapi-encode.md` "M3: HEVC").
* VA-API (and so HEVC) is still opt-in (`--encoder=vaapi`); `auto` unchanged.
* **Not yet checked by eye:** the HEVC picture on the iPad (stats and decode
  are clean; the colour test passes for HEVC). Look at it once.
* **Pending decision (owner):** the branch/release model proposed on
  2026-11-10 (`main` mirror, a `release` integration branch taking fork PRs as
  merges, tags there, upstream merged in, topic branches rebased for
  upstream PRs). Not set up; ask before doing any of it.

## Next step

Options, in PLAN order (agree with the owner first):

1. **M4 zero-copy capture** (`linux/dmabuf-capture`, on `linux/hevc`):
   dmabuf capture imported as VA surfaces; removes the ~2.5 ms upload and is
   what makes 120 fps realistic.
2. Small M2/M3 follow-ups: VA-API in `auto` (then HEVC is automatic for
   everyone with a VA-API GPU); overlap upload with encode; why the live
   `foot`/`seq` scroll is so much cheaper than the synthetic flood; AMD.
3. HEVC 4:4:4 only after an iPad app change (PLAN M3).

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
  (upload, vpp, send_frame, receive) every 2 s. `OD_VAAPI_CPU_CONVERT=1`
  forces the old swscale path for A/B.
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
* **Tools installed for this work:** `perf intel-gpu-tools gst-plugin-va
  gst-libav nasm`. `kernel.perf_event_paranoid` was set to 0 for profiling;
  it resets to 2 on reboot (or `sudo sysctl kernel.perf_event_paranoid=2`).

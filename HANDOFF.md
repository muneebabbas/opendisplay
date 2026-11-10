# Handoff: where to pick up

Last updated 2026-11-10. Update this file at the end of every session.

## State

* **Code:** two stacked topic branches on upstream v1.30.2 (`0f44853`, still
  upstream's latest on 2026-11-10):
  * `linux/vaapi-encoder` (M0): opt-in VA-API H.264 encoder, 3 commits.
    Draft PR https://github.com/muneebabbas/opendisplay/pull/1
  * `linux/vaapi-gpu-convert` (M2), on top: 6 commits. GPU colour conversion
    through libva VPP (`encoder/vpp.rs`, hand-written dlopen'd FFI), direct
    surface-mapping upload, no per-frame sync, debug step timings
    (`encoder/steps.rs`), 7 VA-API tests. Draft PR in the fork (see PLAN).
  Nothing has been sent upstream.
* **M1 and M2 are done.** On the iPad over Wi-Fi, `--encoder=vaapi` now does
  60 fps full-motion video at **0.33 cores** (x264: 3.08) and scrolling at
  **0.15 cores** (x264: 2.43), with latency equal to or better than x264 and
  exact BT.709 colours. Numbers and the why: PLAN target table,
  `research/vaapi-encode.md` "Stage A2: result".
* **The BT.601 colour "blocker" was FFmpeg**: `scale_vaapi` picks iHD's
  Explicit mode and iHD reads the matrix from the primaries only. Through
  libva with `output_color_standard = BT709` it is exact.
* VA-API is still opt-in (`--encoder=vaapi`); `auto` is unchanged.

## Next step: M3, HEVC

Create `linux/hevc` from `linux/vaapi-gpu-convert`. Scope agreed on
2026-11-10 (see PLAN M3):

1. Negotiation (`desktop.rs`): parse `hello.videoCaps` HEVC entries (today
   filtered to H.264). Pick HEVC automatically, no user setting (PROTOCOL
   6.5 says codec choice is the sender's; the Mac sender does the same),
   when offered and `hevc_vaapi` opens at the stream size; otherwise H.264.
   A dev-only override to force H.264 for A/B, like `OD_VAAPI_CPU_CONVERT`.
2. Encoder (`encoder/ffmpeg.rs`): `hevc_vaapi` with the same low-latency
   settings (no B-frames, IDR on demand, `async_depth=1`, no SEI). iHD lists
   HEVC Main / Main10 / 444 / SCC EncSlice; FFmpeg has `hevc_vaapi`.
3. Bitstream (`opendisplay-proto/src/video.rs`): the NAL parsing is
   H.264-only (`& 0x1f`, IDR 5, SPS/PPS 7/8). HEVC: type `(b0 >> 1) & 0x3f`,
   IDR 19/20 (also CRA 21 as a keyframe?), VPS/SPS/PPS 32/33/34. Every HEVC
   IDR must carry VPS+SPS+PPS (PROTOCOL 5.1); access-unit splitting and
   `param_sets` prepending must become codec-aware.
4. `streamConfig.codec = "hevc"`; fall back to H.264 for the rest of the
   session if the HEVC encoder fails (PROTOCOL 6.5).
5. Done when: the iPad shows a correct picture with `codec=hevc`, sane
   PHONE-STATS, reconnect works, H.264 still works when HEVC is not
   offered, unit tests for NAL parsing and codec choice, CI checks pass.
   Then benchmark HEVC vs H.264 (bitrate at equal quality on scroll is the
   expected win).

Smaller follow-ups for M2 (not blocking): overlap the upload with the
previous encode (convert-stage thread, ~2.5-3 ms latency); consider VA-API
in `auto`; check AMD (radeonsi) if a machine is available; the EIO after
destroying a VPP context on iHD.

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
* **Tools installed for this work:** `perf intel-gpu-tools gst-plugin-va
  gst-libav nasm`. `kernel.perf_event_paranoid` was set to 0 for profiling;
  it resets to 2 on reboot (or `sudo sysctl kernel.perf_event_paranoid=2`).

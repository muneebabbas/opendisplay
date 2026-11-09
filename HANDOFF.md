# Handoff: where to pick up

Last updated 2026-11-09. Update this file at the end of every session.

## State

* **Code:** branch `linux/vaapi-encoder` = upstream v1.30.2 (`0f44853`) +
  3 commits:
  1. `feat(linux): opt-in VA-API H.264 encoder for od-sender`
  2. `perf(linux): hardware encoders convert every frame ahead of the encoder`
  3. `docs(linux): document the opt-in VA-API encoder`

  Draft PR in the fork for review: https://github.com/muneebabbas/opendisplay/pull/1
  (fork only; nothing has been sent upstream).
* **Notes:** this `fork` branch: [PLAN.md](PLAN.md) (milestones M1-M5,
  parked ideas), [research/vaapi-encode.md](research/vaapi-encode.md)
  (design, measurements, the BT.601 colour finding), raw results in
  `results/2026-11-09-ipad/`.
* **Key findings so far:**
  * VA-API alone saves only ~20% CPU: **colour conversion (swscale, BGRA ->
    NV12) is ~69% of the sender's CPU**, for x264 and VA-API alike.
  * GPU conversion (VPP) cuts conversion CPU ~6x, but iHD outputs
    **BT.601** whatever is asked through FFmpeg's `scale_vaapi`: colours
    shift (`results/2026-11-09-ipad/compare.png`).
  * The iPad app caps streams at 60 fps (H.264 and HEVC). 120 fps needs an
    app change; the Linux receiver can be used to test 120+ meanwhile.

## Next step: M1, then M2

1. **M1, measurement baseline** (small): add battery power
   (`/sys/class/power_supply/BAT0/power_now`, on battery) and
   `intel_gpu_top -J` to `tools/ipad_bench.py`; rerun idle/scroll/video for
   x264 and VA-API; record in `results/` and the PLAN target table.
2. **M2, GPU colour conversion**: create `linux/vaapi-gpu-convert` from
   `linux/vaapi-encoder`. First task: a standalone check whether iHD's VPP
   produces BT.709 when called through libva directly
   (`VAProcPipelineParameterBuffer`: `output_color_standard =
   VAProcColorStandardBT709`, `output_color_properties` with limited range).
   Verify with the flat-patch values in `research/vaapi-encode.md`
   (BT.709 red = 63,102,240; BT.601 red = 81,90,240). The result decides the
   design (libva FFI vs `scale_vaapi` vs shader).

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
* **A/B harness:** `python3 -I ~/projects/opendisplay-fork/tools/ipad_bench.py
  <od-sender> 47000 <outdir> x264,vaapi idle,scroll,video 20`.
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
* **Tools installed for this work:** `perf intel-gpu-tools gst-plugin-va
  gst-libav nasm`. `kernel.perf_event_paranoid` was set to 0 for profiling;
  it resets to 2 on reboot (or `sudo sysctl kernel.perf_event_paranoid=2`).

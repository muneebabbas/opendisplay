# M4 live A/B: zero-copy (dmabuf) capture vs M3 (iPad over Wi-Fi, 2026-11-10)

```
python3 -I tools/ipad_bench.py /tmp/od-sender-m4 10.93.10.226:9000 results/2026-11-10-wifi-m4 \
  vaapi-hevc,vaapi-hevc@/tmp/od-sender-m3 idle,scroll,video 30
python3 -I tools/ipad_bench.py /tmp/od-sender-m4 10.93.10.226:9000 results/2026-11-10-wifi-m4-reversed \
  vaapi-hevc@/tmp/od-sender-m3,vaapi-hevc idle,scroll,video 30
```

* `vaapi-hevc` = `/tmp/od-sender-m4`, `linux/dmabuf-capture` before the squash (0537fb6; same encode/capture code as bb3eaad, which adds only the dev `--gpu-linear` hook, the `Caps::dmabuf` report and docs), release,
  `--encoder=vaapi`: GPU buffers XR24 I915_4_TILED, HEVC.
* `vaapi-hevc@/tmp/od-sender-m3` = `linux/hevc` 573db23 (release), the M3 binary:
  shm capture, upload into a VA surface.
* Second pass in reversed order (battery order effect). On battery, iPad over
  Wi-Fi, 30 s windows after 6 s warm-up; `cpu_cores` from `/proc/<pid>/stat`.

| | CPU cores (pass 1 / 2) | CPU per frame | encode p50 / p95 | fps sent / iPad | e2e p50 / p95 | render engine busy (system) |
|---|---|---|---|---|---|---|
| M4 scroll | **0.076 / 0.079** | 1.3 ms | 11.6 / 11.8, 10.8 / 10.9 ms | 59.8 / 59 | 26 / 34, 23.5 / 29.5 ms | 28.0 / 26.7 % |
| M3 scroll | 0.153 / 0.157 | 2.6 ms | 13.0 / 13.4, 13.1 / 13.4 ms | 59.7 / 59 | 26.5 / 33.5, 27.5 / 116 ms | 34.5 / 34.9 % |
| M4 video | **0.222 / 0.246** | 3.9 ms | 12.9 / 14.1, 13.3 / 14.2 ms | 59.5 / 59 | 29 / 37, 29 / 37 ms | 44.9 / 43.2 % |
| M3 video | 0.351 / 0.342 | 5.8 ms | 16.1 / 17.2, 16.2 / 17.8 ms | 59.3 / 58-59 | 29 / 40, 27.5 / 39.5 ms | 47.5 / 47.0 % |

* CPU halves on scrolling and drops by a third on video; encode p50 (capture to
  packet) is 2-3 ms shorter (the upload is gone); latency at or below M3.
* The compositor's render engine is less busy too (Hyprland copies into a
  tiled GPU buffer instead of linear shared memory).
* Sender video/VPP engines unchanged (vcs ~42-44 %, vecs ~19-20 %), media
  clock 600-650 MHz in both.
* Battery: scroll 15.3 / 18.8 W (M4) vs 19.2 / 15.1 W (M3), video 13.2 / 11.2 W
  vs 11.5 / 14.3 W. It follows run order again (second run higher); no power
  difference can be read from this.
* M3 reversed scroll e2e p95 116 ms is one Wi-Fi spike (hitch 4.5).

Where the remaining CPU goes (video, perf, `../2026-11-10-m4-profile/`): see
`research/vaapi-encode.md` "M4: zero-copy capture".

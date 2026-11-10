# M5 groundwork: 120 fps against the local od-receiver (2026-11-10)

`run.sh shm|gpu`, alternated shm, gpu, shm, gpu: `od-receiver --listen
127.0.0.1:9100 --width 2388 --height 1668 --scale 2 --refresh 120 --sink none`,
`/tmp/od-sender-m4 run --encoder=vaapi` (H.264: od-receiver offers no HEVC),
mpv `testsrc2` at 120 fps on the virtual display, 15 s sampled after 14 s.
`shm` = `OD_VAAPI_SHM_CAPTURE=1` (the M3 path), `gpu` = dmabuf capture (M4).

| | fps sent | encode p50 / p95 | media GT clock (median) | CPU cores |
|---|---|---|---|---|
| shm (2 runs) | 49-59 (one 5 s window 78) | 10-16 / 14-18 ms | 0-500 MHz | 0.245 / 0.251 |
| gpu (2 runs) | **114-117** | **7.0-7.5 / 8.0-8.2 ms** | **1200 MHz** (its max) | 0.300 / 0.292 |

With shm capture the media engine idles between the CPU upload and the
encode and the governor never clocks it up; with dmabufs the GPU work is back
to back and it runs at 1200 MHz, which brings an H.264 encode of 2388x1668
under the 8.3 ms budget of 120 fps (p95 just at it). HEVC at 120 is untested
(needs a receiver that offers it at 120: the iPad app caps at 60).
`first-*-run.log` are the earlier single runs with the same result.

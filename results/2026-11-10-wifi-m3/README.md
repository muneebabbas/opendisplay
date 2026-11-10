# M3 live A/B: VA-API H.264 vs HEVC (iPad over Wi-Fi, 2026-11-10)

`python3 -I tools/ipad_bench.py /tmp/od-hevc 10.93.10.226:9000 results/2026-11-10-wifi-m3 vaapi-h264,vaapi-hevc idle,scroll,video 30`,
run twice (results.jsonl has both passes). `vaapi-h264` = `--encoder=vaapi` with
`OD_FORCE_H264=1`; `vaapi-hevc` = `--encoder=vaapi` (the iPad offers HEVC).
Binary: `linux/hevc` 200f176, release. `../2026-11-10-wifi-m3-reversed` repeats
scroll in the order hevc, h264, hevc, h264 to check the battery reading (it
follows run order, not codec). Summary in `research/vaapi-encode.md` "M3: HEVC".

# Cursor in GPU-buffer frames (Hyprland 0.57, eDP-1, 2026-11-10)

`od-sender dev capture-test --output eDP-1 --seconds 1 --cursor --gpu --gpu-linear --save …`
(linear dmabufs, mapped and saved) vs the same with shm, pointer moved with
`hyprctl dispatch 'hl.dsp.cursor.move({ x = 600, y = 400 })'` (pixel 1200,800
at scale 2). `cmp.py` compares the PPMs.

* GPU frame with cursor vs shm frame with cursor (taken back to back): 0
  differing pixels in a 300x300 region around the pointer; 99.85-99.97 %
  identical overall (the rest is live window content).
* GPU frame without cursor: the arrow is missing (467 pixels differ, bbox
  1195-1226 x 800-845).
* Crops (120x120, 4x): `gpu-cursor2-crop.png`, `shm-cursor2-crop.png`,
  `gpu-nocursor-crop.png`. Only linear buffers can be read back this way;
  the 4-tiled ones used in streaming come from the same compositor path.

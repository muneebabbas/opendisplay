# dmabuf zero-copy spike: ext-image-copy-capture -> gbm -> VA-API (iHD) -> VPP NV12

Machine: Hyprland 0.57.0, Lunar Lake (Xe2, `xe` driver), iHD 26.3.5, libva 1.24 (2.24), Mesa gbm 26.2.6, eDP-1 2880x1800.
Build: `./build.sh` (runs wayland-scanner into `gen/`, then cc + pkg-config). Run: `./spike > out.txt`. Full output in `out.txt`.

## Result

Zero-copy works if you **leave `I915_FORMAT_MOD_4_TILED_LNL_CCS` out** of the modifier list.
Capture into a gbm bo (LINEAR, X_TILED or 4_TILED), import it into VA with DRM_PRIME_2, run VPP to NV12:
the colours are exact BT.709 limited, and the imported VA surface can be cached and reused for every capture into that bo.

## 1. What Hyprland sends (session on eDP-1)

Event order (the same with options=0 and with `paint_cursors`):

```
shm_format 0 (ARGB8888)
dmabuf_format AR24 [0x0 LINEAR, 0x0100000000000001 X_TILED, 0x0100000000000009 4_TILED,
                    0x0100000000000010 4_TILED_LNL_CCS, 0x00ffffffffffffff INVALID]
shm_format 1 (XRGB8888)
dmabuf_format XR24 [same 5 modifiers]
dmabuf_device  8-byte dev_t = 226:128  -> this is /dev/dri/renderD128 itself (the render node, not card0)
buffer_size 2880x1800
done
```

- shm and dmabuf formats come interleaved, per format. `buffer_size` comes last, just before `done`, so don't size anything until `done`.
- The list includes `DRM_FORMAT_MOD_INVALID`. gbm rejects it, so filter it out.
- The global `zwp_linux_dmabuf_v1` (advertised v5, bound v3) offers the same 4 XRGB8888 modifiers plus INVALID.
- Code that handles dev_t generically should still map it via `/sys/dev/char/M:m/device/drm/renderD*`. The spike does this.

## 2. gbm allocation (on renderD128, `GBM_BO_USE_RENDERING`)

| modifier list | gbm picked | planes | stride | size |
|---|---|---|---|---|
| all compositor mods | 0x0100000000000010 **4_TILED_LNL_CCS** | 1 | 11520 | 23068672 |
| all minus CCS | 0x0100000000000009 4_TILED | 1 | 11520 | 23068672 |
| LINEAR only | 0x0 | 1 | 11520 | 20971520 |
| X_TILED only | X_TILED | 1 | 11776 | 23068672 |

All offsets are 0. LNL_CCS is single-plane (Xe2 flat CCS has no separate aux plane). gbm can allocate XBGR8888 too.

## 3. Capture into dmabuf wl_buffer

`zwp_linux_buffer_params_v1.add` + `create` (async) was used instead of `create_immed`, because with `create_immed` a rejected buffer is a fatal protocol error. Every modifier was accepted. Capture always ended in `ready` and never in `failed`.

- capture() -> ready takes 5 ms to 600 ms. Hyprland only sends `ready` after its next render of the output, so on a static screen this time is just waiting for damage (a ~600 ms repaint cadence here).
- The real copy latency is `presentation_time` -> `ready`: **about 1.2 to 8.6 ms** (typically 2 to 5 ms) (assuming the presentation clock is CLOCK_MONOTONIC; the values are consistent with that).
- Each frame gets 1 damage rect and transform 0.

## 4. VA import (DRM_PRIME_2, `VA_RT_FORMAT_RGB32`)

iHD returns success for **every** modifier and every fourcc tried: BGRX, XRGB, BGRA and ARGB for DRM XR24, and RGBX, XBGR and RGBA for XB24.
The derived image is named after the "other" byte order: BGRX -> `XRGB`, BGRA -> `ARGB`, RGBX -> `XBGR`.
Use `VA_FOURCC_BGRX` for DRM_FORMAT_XRGB8888 and `VA_FOURCC_RGBX` for DRM_FORMAT_XBGR8888.

**Gotcha: LNL_CCS.** Import and VPP both return `VA_STATUS_SUCCESS`, but the VPP output is all black (16,128,128). This happens for screen captures and for synthetic fills alike.
The buffer content itself is fine: `gbm_bo_map(READ)` (Mesa's decompressing blit) shows the real pixels. So iHD 26.3.5 does not handle `I915_FORMAT_MOD_4_TILED_LNL_CCS` imports, and it fails **silently**.
Remove LNL_CCS (and BMG_CCS) from the list before calling gbm. gbm then picks 4_TILED.

## 5. Colour check (VPP: surface SRGB / input FULL -> BT709 / output REDUCED)

Synthetic red top half / blue bottom half, filled via `gbm_bo_map(GBM_BO_TRANSFER_WRITE)`, which detiles correctly for tiled bos:

| DRM fmt | modifier | VA fourcc | red | blue | |
|---|---|---|---|---|---|
| XR24 | LINEAR | BGRX | 63,102,240 | 32,240,118 | PASS (exact) |
| XR24 | X_TILED | BGRX | 63,102,240 | 32,240,118 | PASS |
| XR24 | 4_TILED | BGRX | 63,102,240 | 32,240,118 | PASS |
| XR24 | 4_TILED_LNL_CCS | BGRX | 16,128,128 | 16,128,128 | FAIL (black) |
| XB24 | LINEAR | RGBX | 63,102,240 | 32,240,118 | PASS |
| XB24 | 4_TILED_LNL_CCS | RGBX | 16,128,128 | 16,128,128 | FAIL |

For screen captures, the VPP output of the non-CCS buffers matches the `gbm_bo_map(READ)` pixels. For example, grey 34,34,34 gives Y=45, which is correct BT.709 limited.

## 6. Timing (2880x1800) and reuse

- **First import** of a bo (`vaCreateSurfaces` DRM_PRIME_2): 0.1 to 0.3 ms CPU. Re-importing a bo that already has a live surface: 4 to 20 us. Reusing a cached `VASurfaceID` costs nothing.
- **VPP** BGRX->NV12: submit only 0.16 to 0.35 ms CPU; submit+sync 3.3 to 5 ms wall (0.25 to 0.7 ms CPU). Pipelined it is about 2.4 ms GPU per frame.
- **Reuse works.** The imported surface aliases the bo memory. Overwriting the bo by CPU and then re-capturing into it both changed the VPP output of the cached surface. After capture #2, the cached surface gave the same NV12 hash as a fresh import of the same bo. A capture 3 s later produced a different hash.
- Recommended setup: a ring of N gbm bos, each with one wl_buffer and one VA surface created once.

## 7. Sync

Straight after `ready` (probed inside the event handler), `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` (flags READ) succeeds and the sync file polls as already signalled. That was true in 60 of 60 captures, across every modifier. A later probe with flags WRITE was also signalled.
So no fence is still pending when `ready` arrives, and presentation->ready takes a few ms. Together that suggests Hyprland waits for its copy to finish before sending `ready`. No torn content was seen.
Caveat: xe does not add implicit fences for its own submissions, so a missing fence on its own would not prove completion. The evidence is the correct content in every capture plus the ~2 to 5 ms delay before `ready`.
It is safe to submit VPP right after `ready` without waiting on anything.

## Bindings (x86_64, printed by the program)

```
VADRMPRIMESurfaceDescriptor: size 312
  0 fourcc u32 | 4 width u32 | 8 height u32 | 12 num_objects u32
  16 objects[4] (16 bytes each): +0 fd i32, +4 size u32, +8 drm_format_modifier u64
  80 num_layers u32
  84 layers[4] (56 bytes each): +0 drm_format u32, +4 num_planes u32, +8 object_index[4] u32,
                                +24 offset[4] u32, +40 pitch[4] u32
VASurfaceAttrib: size 24 {type i32 @0, flags u32 @4, value VAGenericValue @8}
VAGenericValue: size 16 {type i32 @0, value union @8 (i32 / f32 / ptr / func)}
VASurfaceAttribMemoryType = 6, VASurfaceAttribExternalBufferDescriptor = 7, VA_SURFACE_ATTRIB_SETTABLE = 2
VAGenericValueTypeInteger = 1, VAGenericValueTypePointer = 3
VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 = 0x40000000, VA_RT_FORMAT_RGB32 = 0x00020000
VA_FOURCC_BGRX = 0x58524742, VA_FOURCC_RGBX = 0x58424752 (XRGB 0x42475258, BGRA 0x41524742)
DRM_FORMAT_XRGB8888 = 0x34325258, XBGR8888 = 0x34324258, ARGB8888 = 0x34325241
I915_FORMAT_MOD_4_TILED = 0x0100000000000009, X_TILED = 0x0100000000000001, LNL_CCS = 0x0100000000000010, INVALID = 0x00ffffffffffffff
GBM_BO_USE_RENDERING = 4, GBM_BO_TRANSFER_WRITE = 2
DMA_BUF_IOCTL_EXPORT_SYNC_FILE = 0xc0086202, struct {u32 flags; i32 fd} (8 bytes), DMA_BUF_SYNC_READ = 1
```

Import attributes: `[{MemoryType, SETTABLE, Integer, DRM_PRIME_2}, {ExternalBufferDescriptor, SETTABLE, Pointer, &desc}]`, num_attribs = 2.
Descriptor values used: `num_objects=1`, `objects[0] = {fd, lseek(fd,0,SEEK_END), modifier}`, `num_layers=1`, `layers[0] = {DRM fourcc, nplanes, object_index 0, gbm offset, gbm stride}`.
The fd can be closed straight after `vaCreateSurfaces`, because iHD keeps its own GEM handle.

The VPP context was created with no render targets (`vaCreateContext(..., NULL, 0, ...)`) and iHD accepted it.

gbm functions used: `gbm_create_device(fd)`, `gbm_bo_create_with_modifiers2(dev, w, h, fmt, mods, n, GBM_BO_USE_RENDERING)`, `gbm_bo_get_modifier`, `gbm_bo_get_plane_count`, `gbm_bo_get_stride_for_plane`, `gbm_bo_get_offset`, `gbm_bo_get_fd` / `gbm_bo_get_fd_for_plane`, `gbm_bo_destroy`, and `gbm_device_destroy`.
`gbm_bo_map` / `gbm_bo_unmap` are only needed for tests.

// BT.709 limited-range RGB -> NV12 for the NVENC encoder (cuda.rs).
// NVENC's own RGB input is converted with BT.601 whatever the stream
// signals, and the receivers render BT.709, so the conversion is done here.
//
// Built without the CUDA toolkit: clang compiles it to PTX, which the
// driver compiles for the GPU at load. Regenerate nv12.ptx with gen-ptx.sh.
//
// One thread per 2x2 block: four luma samples and one chroma pair, the
// chroma from the average of the four pixels (centred siting, as swscale).

#define __global__ __attribute__((global))

typedef unsigned char u8;
typedef unsigned int u32;

static __attribute__((device)) inline u32 tid_x() {
    return __nvvm_read_ptx_sreg_ctaid_x() * __nvvm_read_ptx_sreg_ntid_x() + __nvvm_read_ptx_sreg_tid_x();
}
static __attribute__((device)) inline u32 tid_y() {
    return __nvvm_read_ptx_sreg_ctaid_y() * __nvvm_read_ptx_sreg_ntid_y() + __nvvm_read_ptx_sreg_tid_y();
}

static __attribute__((device)) inline u8 clamp8(float v) {
    v = v < 0.f ? 0.f : (v > 255.f ? 255.f : v);
    return (u8)(v + 0.5f);
}

static __attribute__((device)) inline float luma(float r, float g, float b) {
    return 16.f + (0.2126f * r + 0.7152f * g + 0.0722f * b) * (219.f / 255.f);
}

// `src` holds 4-byte pixels, `src_pitch` bytes apart per row. `bgr` is 1
// when the bytes are B, G, R, X (DRM XRGB8888 / ARGB8888) and 0 for R, G,
// B, X (XBGR8888 / ABGR8888). `w` and `h` are even.
extern "C" __global__ void rgb_to_nv12(const u8 *src, u32 src_pitch, u8 *y, u32 y_pitch,
                                       u8 *uv, u32 uv_pitch, u32 w, u32 h, u32 bgr) {
    u32 bx = tid_x(), by = tid_y();
    if (bx * 2 >= w || by * 2 >= h) return;
    float rs = 0.f, gs = 0.f, bs = 0.f;
    for (u32 dy = 0; dy < 2; dy++) {
        u32 row = by * 2 + dy;
        const u32 *p = (const u32 *)(src + (unsigned long)row * src_pitch) + bx * 2;
        u8 *yo = y + (unsigned long)row * y_pitch + bx * 2;
        for (u32 dx = 0; dx < 2; dx++) {
            u32 px = p[dx];
            float c0 = (float)(px & 0xff), c1 = (float)((px >> 8) & 0xff), c2 = (float)((px >> 16) & 0xff);
            float r = bgr ? c2 : c0, g = c1, b = bgr ? c0 : c2;
            yo[dx] = clamp8(luma(r, g, b));
            rs += r; gs += g; bs += b;
        }
    }
    float r = rs * 0.25f, g = gs * 0.25f, b = bs * 0.25f;
    u8 *c = uv + (unsigned long)by * uv_pitch + bx * 2;
    c[0] = clamp8(128.f + (-0.1146f * r - 0.3854f * g + 0.5f * b) * (224.f / 255.f));
    c[1] = clamp8(128.f + (0.5f * r - 0.4542f * g - 0.0458f * b) * (224.f / 255.f));
}

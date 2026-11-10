#!/usr/bin/env python3
"""Offline rate-distortion bench: VA-API H.264 vs HEVC (od-sender settings).

Reproduce:
  python3 -I tools/rd_bench.py --work ~/odrd-work --out results/2026-11-10-rd-m3
Options: --frames N (default 360), --dry-run (print all ffmpeg commands),
         --sources S1,S2,S3, --stage all|gen|encode|measure|analyze
Pure stdlib. Intermediate files go in --work (not /tmp).
"""
import argparse, csv, json, math, os, re, shlex, subprocess, sys, time

W, H, FPS = 2388, 1668, 60
DEV = "/dev/dri/renderD128"
FONT = "/usr/share/fonts/TTF/CaskaydiaMonoNerdFontMono-Regular.ttf"
SRC_DIR = os.path.expanduser("~/projects/opendisplay/linux/crates/od-sender/src")
SOURCES = {"S1": "scroll-code", "S2": "scroll-flood", "S3": "video"}
CODECS = {"h264": ("h264_vaapi", "h264", []), "hevc": ("hevc_vaapi", "hevc", ["-profile:v", "main"])}
# Sweep A CBR targets in kbit/s, per source. The default 2..24 Mbit/s ladder is
# pointless for S1 (saturates, VMAF>99 at 2M) and for S2 (iHD CBR cannot go below
# ~8 Mbit/s there), so those ladders are shifted.
RATES_A = {"S1": [250, 500, 1000, 1500, 2000, 3000, 4000, 6000],
           "S2": [8000, 12000, 16000, 24000, 32000, 48000],
           "S3": [2000, 4000, 6000, 8000, 12000, 16000, 24000]}
CAPS_B = [35000, 8000, 4000]  # kbit/s
TAGS = ["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]
TO_YUV = "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p"

DRY = False
LOG = []


def sh(cmd, check=True, capture=True):
    LOG.append(shlex.join(cmd))
    if DRY:
        print(shlex.join(cmd))
        return ""
    t = time.time()
    p = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
    if check and p.returncode:
        sys.exit("FAILED (%d): %s\n%s" % (p.returncode, shlex.join(cmd), p.stdout[-3000:]))
    return p.stdout


def ff(*a, **k):
    return sh(["ffmpeg", "-hide_banner", "-y", "-nostdin"] + list(a), **k)


# ---------------------------------------------------------------- sources
def prep_text(path, kind, frames):
    if DRY and not os.path.isdir(SRC_DIR):
        return
    if kind == "S1":
        raw = open(SRC_DIR + "/session.rs").read() + "\n" + open(SRC_DIR + "/desktop.rs").read()
        lines = [l.expandtabs(4).rstrip()[:140] for l in raw.split("\n")]
        lines = lines[:420]
    else:  # S2: 7-digit consecutive numbers filling the line, like `seq` in a wide terminal
        lines, n = [], 1000000
        for _ in range(700):
            lines.append(" ".join(str(n + i) for i in range(17)))
            n += 17
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


def gen_source(work, s, frames):
    ref = f"{work}/{s}.mkv"
    if os.path.exists(ref) and not DRY:
        return ref
    out = ["-c:v", "ffv1", "-level", "3", "-g", "1", "-frames:v", str(frames), "-r", str(FPS)] + TAGS
    if s == "S3":
        ff("-f", "lavfi", "-i", f"testsrc2=size={W}x{H}:rate={FPS}",
           "-vf", TO_YUV, *out, ref)
        return ref
    txt, png = f"{work}/{s}.txt", f"{work}/{s}_tall.png"
    prep_text(txt, s, frames)
    th = 24000 if s == "S2" else 14000
    ff("-f", "lavfi", "-i", f"color=c=0x1e1e1e:s={W}x{th}:d=1",
       "-vf", f"drawtext=fontfile={FONT}:textfile={txt}:expansion=none:fontsize=28:fontcolor=0xd4d4d4:x=0:y=0",
       "-frames:v", "1", png)
    if s == "S1":
        # 1200 px/s at 60 fps = 20 px/frame
        yexp = "min(1200*n/60,%d)" % (th - H)
    else:
        # ~40 lines/frame; wraps over the 24000 px (~700 line) image
        yexp = "mod(n*1360,21400)"
    ff("-loop", "1", "-framerate", str(FPS), "-i", png,
       "-vf", f"crop={W}:{H}:0:'{yexp}',{TO_YUV}", *out, ref)
    return ref


# ---------------------------------------------------------------- encode
def enc_cmd(ref, codec, rc, outfile):
    enc, fmt, extra = CODECS[codec]
    return (["ffmpeg", "-hide_banner", "-y", "-nostdin", "-v", "verbose",
             "-init_hw_device", f"vaapi=va:{DEV}", "-filter_hw_device", "va", "-i", ref,
             "-vf", "format=nv12,hwupload", "-c:v", enc, "-bf", "0", "-g", "100000",
             "-async_depth", "1", "-sei", "0"] + extra + rc + ["-f", fmt, outfile])


def rc_args(sweep, v):
    r = f"{v//1000}M" if v % 1000 == 0 else f"{v}k"
    bs = f"{v//4}k"
    if sweep == "A":
        return ["-rc_mode", "CBR", "-b:v", r, "-maxrate", r, "-bufsize", bs]
    return ["-global_quality", "21", "-b:v", r, "-maxrate", r, "-bufsize", bs]


def do_encode(work, s, codec, sweep, v, frames):
    tag = f"{s}_{codec}_{sweep}{v}"
    out, meta = f"{work}/{tag}.bin", f"{work}/{tag}.enc.json"
    if os.path.exists(meta) and not DRY:
        return json.load(open(meta))
    rc = rc_args(sweep, v)
    cmd = enc_cmd(f"{work}/{s}.mkv", codec, rc, out)
    log = sh(cmd, check=False)
    if DRY:
        return None
    ok = os.path.exists(out) and os.path.getsize(out) > 0
    d = {"tag": tag, "ok": ok, "cmd": shlex.join(cmd), "rc_lines": [], "warn": []}
    for l in log.splitlines():
        if re.search(r"RC mode|rate control|Using .*(CBR|VBR|QVBR|CQP|ICQ)|[Ww]arning|not support|ignor|Unsupported|fail", l, re.I):
            (d["rc_lines"] if re.search(r"RC mode|rate control|Using", l, re.I) else d["warn"]).append(l.strip())
    d["log_tail"] = [] if ok else log.splitlines()[-8:]
    if ok:
        d["bytes"] = os.path.getsize(out)
        d["kbps"] = d["bytes"] * 8 / (frames / FPS) / 1000
    json.dump(d, open(meta, "w"))
    return d


# ---------------------------------------------------------------- measure
def ref_in(work, s):
    return ["-i", f"{work}/{s}.mkv"]


def measure(work, s, tag, fmt, frames):
    mj = f"{work}/{tag}.m.json"
    if os.path.exists(mj) and not DRY:
        return json.load(open(mj))
    vlog = f"{work}/{tag}.vmaf.json"
    fg = ("[0:v]setpts=N/(60*TB),format=yuv420p,split=3[d1][d2][d3];"
          "[1:v]setpts=N/(60*TB),format=yuv420p,split=3[r1][r2][r3];"
          "[d1][r1]psnr[p];[d2][r2]ssim[s];"
          f"[d3][r3]libvmaf=n_threads={os.cpu_count()}:log_fmt=json:log_path={vlog}[v]")
    t = time.time()
    out = ff("-f", fmt, "-framerate", str(FPS), "-i", f"{work}/{tag}.bin", "-i", f"{work}/{s}.mkv",
             "-filter_complex", fg, "-map", "[p]", "-f", "null", "-", "-map", "[s]", "-f", "null", "-",
             "-map", "[v]", "-f", "null", "-")
    if DRY:
        return None
    m = re.search(r"PSNR y:([\d.inf]+).*?average:([\d.inf]+)", out)
    sm = re.search(r"SSIM Y:[\d.]+ .*All:([\d.]+)", out)
    j = json.load(open(vlog))
    nf = len(j["frames"])
    r = {"psnr_y": float(m.group(1)), "ssim": float(sm.group(1)),
         "vmaf": j["pooled_metrics"]["vmaf"]["mean"], "frames": nf, "secs": time.time() - t}
    if nf != frames:
        print(f"WARNING {tag}: {nf} frames compared, expected {frames}", file=sys.stderr)
    os.remove(vlog)
    json.dump(r, open(mj, "w"))
    return r


def alignment_check(work, frames):
    ref = f"{work}/S1.mkv"
    base = ["-i", ref]
    def psnr(second, shift=False):
        b = "trim=start_frame=1,setpts=N/(60*TB)" if shift else "setpts=N/(60*TB)"
        o = ff(*base, *second, "-filter_complex",
               f"[0:v]setpts=N/(60*TB)[a];[1:v]{b}[b];[a][b]psnr", "-f", "null", "-")
        if DRY:
            return None
        m = re.search(r"PSNR y:(\S+).*?average:(\S+)", o)
        n = re.findall(r"frame=\s*(\d+)", o)
        return {"psnr_y": m.group(1), "average": m.group(2)}
    same = psnr(["-i", ref])
    shifted = psnr(["-i", ref], shift=True)
    return {"ref_vs_ref": same, "ref_vs_shift1": shifted}


# ---------------------------------------------------------------- BD maths
def solve(A, b):
    n = len(b)
    M = [row[:] + [b[i]] for i, row in enumerate(A)]
    for c in range(n):
        p = max(range(c, n), key=lambda r: abs(M[r][c]))
        M[c], M[p] = M[p], M[c]
        for r in range(c + 1, n):
            f = M[r][c] / M[c][c]
            for k in range(c, n + 1):
                M[r][k] -= f * M[c][k]
    x = [0.0] * n
    for i in range(n - 1, -1, -1):
        x[i] = (M[i][n] - sum(M[i][k] * x[k] for k in range(i + 1, n))) / M[i][i]
    return x


def polyfit3(xs, ys):
    """Least squares cubic via 4x4 normal equations (x is centred/scaled internally)."""
    mu = sum(xs) / len(xs)
    sd = (sum((x - mu) ** 2 for x in xs) / len(xs)) ** 0.5 or 1.0
    z = [(x - mu) / sd for x in xs]
    A = [[sum(zi ** (i + j) for zi in z) for j in range(4)] for i in range(4)]
    b = [sum(y * zi ** i for zi, y in zip(z, ys)) for i in range(4)]
    c = solve(A, b)
    return lambda x: sum(c[i] * ((x - mu) / sd) ** i for i in range(4)), c, mu, sd


def integ(c, mu, sd, lo, hi):
    F = lambda x: sum(c[i] * ((x - mu) / sd) ** (i + 1) / (i + 1) for i in range(4)) * sd
    return F(hi) - F(lo)


def bd(rate1, q1, rate2, q2, what):
    """BD between curve 1 (ref, H.264) and 2 (test, HEVC). what='rate' or 'q'."""
    l1 = [math.log10(r) for r in rate1]
    l2 = [math.log10(r) for r in rate2]
    if what == "rate":
        x1, y1, x2, y2 = q1, l1, q2, l2
    else:
        x1, y1, x2, y2 = l1, q1, l2, q2
    lo, hi = max(min(x1), min(x2)), min(max(x1), max(x2))
    if hi <= lo:
        return None
    if min(len(x1), len(x2)) < 4:
        return {"value": None, "lo": lo, "hi": hi, "note": "fewer than 4 points; cubic fit not defined"}
    f1, c1, m1, s1 = polyfit3(x1, y1)
    f2, c2, m2, s2 = polyfit3(x2, y2)
    grid = [lo + (hi - lo) * i / 100 for i in range(101)]
    mono = all(f(b) > f(a) for f in (f1, f2) for a, b in zip(grid, grid[1:]))
    d = (integ(c2, m2, s2, lo, hi) - integ(c1, m1, s1, lo, hi)) / (hi - lo)
    v = (10 ** d - 1) * 100 if what == "rate" else d
    r = {"value": v, "lo": lo, "hi": hi, "cubic_monotonic_on_interval": mono}
    if what == "rate" and not mono:
        r["value"] = None
        r["note"] = "cubic fit not monotonic over the overlap (ill-conditioned); raw value %.4g discarded" % v
    return r


def bd_linear(rate1, q1, rate2, q2):
    """Cross-check: piecewise-linear log10(rate)(metric), averaged on a fine grid."""
    def interp(xs, ys, x):
        pts = sorted(zip(xs, ys))
        for (xa, ya), (xb, yb) in zip(pts, pts[1:]):
            if xa <= x <= xb:
                return ya + (yb - ya) * (x - xa) / ((xb - xa) or 1)
    lo, hi = max(min(q1), min(q2)), min(max(q1), max(q2))
    if hi <= lo:
        return None
    n = 400
    g = [lo + (hi - lo) * i / n for i in range(n + 1)]
    d = sum(interp(q2, [math.log10(r) for r in rate2], x) - interp(q1, [math.log10(r) for r in rate1], x) for x in g) / len(g)
    return (10 ** d - 1) * 100


# ---------------------------------------------------------------- SVG
def svg_chart(path, title, ylabel, series, xs_log=False):
    Wd, Hd, ml, mr, mt, mb = 760, 460, 70, 20, 40, 60
    pts = [p for s in series.values() for p in s]
    x0, x1 = 0, max(p[0] for p in pts) * 1.05
    ys = [p[1] for p in pts]
    y0, y1 = min(ys), max(ys)
    pad = (y1 - y0) * 0.05 or 1
    y0, y1 = y0 - pad, y1 + pad
    X = lambda x: ml + (x - x0) / (x1 - x0) * (Wd - ml - mr)
    Y = lambda y: Hd - mb - (y - y0) / (y1 - y0) * (Hd - mt - mb)
    o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{Wd}" height="{Hd}" font-family="sans-serif" font-size="12">',
         f'<rect width="{Wd}" height="{Hd}" fill="white"/>',
         f'<text x="{Wd/2}" y="22" text-anchor="middle" font-size="15">{title}</text>']
    for i in range(6):
        gy = y0 + (y1 - y0) * i / 5
        o.append(f'<line x1="{ml}" x2="{Wd-mr}" y1="{Y(gy):.1f}" y2="{Y(gy):.1f}" stroke="#ddd"/>')
        o.append(f'<text x="{ml-6}" y="{Y(gy)+4:.1f}" text-anchor="end">{gy:.3g}</text>')
    step = 5 if x1 > 20 else 2
    gx = 0
    while gx <= x1:
        o.append(f'<line x1="{X(gx):.1f}" x2="{X(gx):.1f}" y1="{mt}" y2="{Hd-mb}" stroke="#eee"/>')
        o.append(f'<text x="{X(gx):.1f}" y="{Hd-mb+16}" text-anchor="middle">{gx}</text>')
        gx += step
    o.append(f'<rect x="{ml}" y="{mt}" width="{Wd-ml-mr}" height="{Hd-mt-mb}" fill="none" stroke="black"/>')
    o.append(f'<text x="{(ml+Wd-mr)/2}" y="{Hd-18}" text-anchor="middle">actual bitrate (Mbit/s)</text>')
    o.append(f'<text transform="translate(16,{(mt+Hd-mb)/2}) rotate(-90)" text-anchor="middle">{ylabel}</text>')
    cols = {"h264": "#d62728", "hevc": "#1f77b4"}
    for k, (name, s) in enumerate(series.items()):
        c = cols.get(name, "#333")
        s = sorted(s)
        o.append(f'<polyline fill="none" stroke="{c}" stroke-width="2" points="' +
                 " ".join(f"{X(a):.1f},{Y(b):.1f}" for a, b in s) + '"/>')
        for a, b in s:
            o.append(f'<circle cx="{X(a):.1f}" cy="{Y(b):.1f}" r="3.5" fill="{c}"/>')
        ly = mt + 18 + 18 * k
        o.append(f'<line x1="{Wd-mr-120}" x2="{Wd-mr-95}" y1="{ly}" y2="{ly}" stroke="{c}" stroke-width="2"/>')
        o.append(f'<text x="{Wd-mr-90}" y="{ly+4}">{name}_vaapi</text>')
    o.append("</svg>")
    open(path, "w").write("\n".join(o))


# ---------------------------------------------------------------- main
def main():
    global DRY
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", default="~/odrd-work")
    ap.add_argument("--out", default="results/2026-11-10-rd-m3")
    ap.add_argument("--frames", type=int, default=360)
    ap.add_argument("--sources", default="S1,S2,S3")
    ap.add_argument("--rates", default=None, help="override sweep-A kbit/s list for all sources")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--stage", default="all")
    a = ap.parse_args()
    DRY = a.dry_run
    work, out = os.path.expanduser(a.work), os.path.expanduser(a.out)
    os.makedirs(work, exist_ok=True)
    os.makedirs(out, exist_ok=True)
    srcs = a.sources.split(",")
    stage = a.stage
    jobs = []
    for s in srcs:
        for codec in CODECS:
            for r in ([int(x) for x in a.rates.split(",")] if a.rates else RATES_A[s]):
                jobs.append((s, codec, "A", r))
            for c in CAPS_B:
                jobs.append((s, codec, "B", c))
    rows, encs = [], {}
    t0 = time.time()
    if stage in ("all", "gen"):
        for s in srcs:
            gen_source(work, s, a.frames)
    for (s, codec, sweep, v) in jobs:
        tag = f"{s}_{codec}_{sweep}{v}"
        if stage in ("all", "encode", "measure", "analyze"):
            if stage in ("measure", "analyze") and not os.path.exists(f"{work}/{tag}.enc.json") and not DRY:
                continue
            d = do_encode(work, s, codec, sweep, v, a.frames) if stage in ("all", "encode") else (
                None if DRY else json.load(open(f"{work}/{tag}.enc.json")))
            encs[tag] = d
            if DRY or not d or not d["ok"]:
                if d and not d["ok"]:
                    print(f"ENCODE FAILED {tag}: {d['log_tail']}", file=sys.stderr)
                if DRY and stage in ("all", "measure"):
                    measure(work, s, tag, CODECS[codec][1], a.frames)
                continue
            if stage in ("all", "measure", "analyze"):
                m = measure(work, s, tag, CODECS[codec][1], a.frames)
                rows.append({"source": SOURCES[s], "codec": codec, "sweep": sweep,
                             "requested": (f"CBR {v}k" if sweep == "A" else f"q21 cap {v}k"),
                             "actual_kbps": round(d["kbps"], 1), "psnr_y": round(m["psnr_y"], 3),
                             "ssim": round(m["ssim"], 5), "vmaf": round(m["vmaf"], 3)})
                print(f"[{time.time()-t0:6.0f}s] {tag}: {d['kbps']:.0f} kbps psnr {m['psnr_y']:.2f} "
                      f"ssim {m['ssim']:.4f} vmaf {m['vmaf']:.2f} ({m['secs']:.0f}s)", flush=True)
    if DRY:
        alignment_check(work, a.frames)
        return
    if stage in ("all", "gen"):
        pass
    if stage not in ("all", "measure", "analyze"):
        return
    align_f = f"{work}/align.json"
    if os.path.exists(align_f):
        align = json.load(open(align_f))
    else:
        align = alignment_check(work, a.frames)
        json.dump(align, open(align_f, "w"))
    with open(f"{out}/rd.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=["source", "codec", "sweep", "requested", "actual_kbps", "psnr_y", "ssim", "vmaf"])
        w.writeheader()
        w.writerows(rows)
    # BD analysis, sweep A
    bds = {}
    for s in srcs:
        name = SOURCES[s]
        cur = {c: sorted([r for r in rows if r["source"] == name and r["codec"] == c and r["sweep"] == "A"],
                         key=lambda r: r["actual_kbps"]) for c in CODECS}
        bds[name] = {}
        # SSIM is mapped to dB (-10log10(1-ssim)) and VMAF points >= VMAF_CAP are
        # dropped (saturated; they carry no rate information and wreck cubic fits).
        VMAF_CAP = 99.5
        for met in ("psnr_y", "ssim_db", "vmaf"):
            try:
                c1, c2 = cur["h264"], cur["hevc"]
                if met == "vmaf":
                    c1 = [r for r in c1 if r["vmaf"] < VMAF_CAP]
                    c2 = [r for r in c2 if r["vmaf"] < VMAF_CAP]
                f = (lambda r: -10 * math.log10(1 - r["ssim"])) if met == "ssim_db" else (lambda r: r[met if met != "ssim_db" else "ssim"])
                r1 = [r["actual_kbps"] for r in c1]
                r2 = [r["actual_kbps"] for r in c2]
                q1 = [f(r) for r in c1]
                q2 = [f(r) for r in c2]
                e = bd(r1, q1, r2, q2, "rate")
                e["n_points"] = [len(c1), len(c2)]
                e["linear_interp_check"] = bd_linear(r1, q1, r2, q2)
                bds[name]["bd_rate_" + met] = e
                if met == "psnr_y":
                    bds[name]["bd_psnr_db"] = bd(r1, q1, r2, q2, "q")
            except Exception as e:
                bds[name]["bd_rate_" + met] = {"error": str(e)}
        for c in CODECS:
            svg_chart(f"{out}/rd-{name}-vmaf.svg", f"{name}: VMAF vs bitrate (CBR sweep)", "VMAF",
                      {c: [(r["actual_kbps"] / 1000, r["vmaf"]) for r in cur[c]] for c in CODECS})
            svg_chart(f"{out}/rd-{name}-psnr.svg", f"{name}: PSNR-Y vs bitrate (CBR sweep)", "PSNR-Y (dB)",
                      {c: [(r["actual_kbps"] / 1000, r["psnr_y"]) for r in cur[c]] for c in CODECS})
    rcm = {t: d.get("rc_lines") for t, d in encs.items() if d}
    ver = subprocess.run(["ffmpeg", "-version"], stdout=subprocess.PIPE, text=True).stdout.splitlines()[0]
    va = subprocess.run(["vainfo"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True).stdout.splitlines()[:4]
    summ = {"ffmpeg": ver, "vainfo": va, "frames": a.frames, "bd": bds, "rc_modes": rcm,
            "alignment_check": align,
            "warnings": {t: d["warn"] for t, d in encs.items() if d and d.get("warn")},
            "failed": [t for t, d in encs.items() if d and not d["ok"]]}
    json.dump(summ, open(f"{out}/summary.json", "w"), indent=1)
    print("done in %.0fs" % (time.time() - t0))


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""A/B od-sender encoders against the real iPad (usbmux loopback forward or Wi-Fi).

usage: python3 -I ipad_bench.py SENDER TARGET OUTDIR ENCODER[,ENCODER] WORKLOAD[,WORKLOAD] [SECONDS]

TARGET is a bare PORT (meaning 127.0.0.1:PORT) or HOST:PORT (e.g. 10.93.10.226:9000).
Per run it records whole-process sender CPU, sent/iPad fps and latency, battery power
(BAT0 power_now), and GPU engine busy % from DRM fdinfo, system-wide (`gpu_*`) and for
the sender alone (`sender_gpu_*`); rcs = render/3D, vcs = video (encode), vecs = video
enhance (VPP). An `idle_baseline` line (no sender, no
workload) is measured first so sender cost can be computed as a delta. Run unplugged.
"""
import json, os, re, signal, statistics, subprocess, sys, time

sender, target, outdir = sys.argv[1], sys.argv[2], sys.argv[3]
if target.isdigit():
    target = f"127.0.0.1:{target}"
encoders = sys.argv[4].split(",")
workloads = sys.argv[5].split(",")
window = float(sys.argv[6]) if len(sys.argv) > 6 else 20.0
WS = "9"
TICK = os.sysconf("SC_CLK_TCK")
MEDIA_FREQ = "/sys/class/drm/card0/device/tile0/gt1/freq0/act_freq"
BAT = "/sys/class/power_supply/BAT0"
ANSI = re.compile(r"\x1b\[[0-9;]*m")

WORK = {
    "idle": None,
    "video": "mpv --no-config --title=odbench --fs --loop=inf --no-audio --really-quiet "
             "av://lavfi:testsrc2=size=1194x834:rate=60",
    "scroll": 'foot --app-id=odbench sh -c "while :; do seq 1 2000000; done"',
}


def cpu_ticks(pid):
    with open(f"/proc/{pid}/stat") as f:
        parts = f.read().rsplit(")", 1)[1].split()
    return int(parts[11]) + int(parts[12])  # utime + stime


def hypr(*args):
    subprocess.run(["hyprctl", *args], check=False, capture_output=True)


def kill_work():
    # Match only the workload windows' own flags, never a shell that mentions the name.
    subprocess.run(["pkill", "-f", "^(mpv|foot) .*-(title|app-id)=odbench"], check=False)
    time.sleep(1.0)


def med(xs):
    return round(statistics.median(xs), 1) if xs else None


def read_num(path):
    try:
        with open(path) as f:
            return float(f.read().strip())
    except (OSError, ValueError):
        return None


def battery_watts():
    uw = read_num(f"{BAT}/power_now")
    if uw is None:
        i, v = read_num(f"{BAT}/current_now"), read_num(f"{BAT}/voltage_now")
        if i is None or v is None:
            return None
        uw = i * v / 1e6
    return uw / 1e6


def battery_status():
    try:
        with open(f"{BAT}/status") as f:
            return f.read().strip()
    except OSError:
        return None


ENGINES = ("rcs", "vcs", "vecs", "ccs", "bcs")


def drm_clients(pids=None):
    """DRM fdinfo engine cycles per client: {client_id: {engine: (cycles, total)}}.

    Xe (and i915) publish per-client engine time in /proc/<pid>/fdinfo; busy % over
    an interval is d(drm-cycles-X) / d(drm-total-cycles-X). intel_gpu_top does not
    support Xe and RAPL is root-only, so this is the GPU source. Clients are keyed by
    drm-client-id because several fds can share one.
    """
    out = {}
    for pid in pids or [d for d in os.listdir("/proc") if d.isdigit()]:
        base = f"/proc/{pid}/fdinfo"
        try:
            fds = os.listdir(base)
        except OSError:
            continue
        for fd in fds:
            try:
                with open(f"{base}/{fd}") as f:
                    text = f.read()
            except OSError:
                continue
            if "drm-cycles-" not in text:
                continue
            kv = dict(l.split(":", 1) for l in text.splitlines() if ":" in l)
            cid = kv.get("drm-client-id", "").strip()
            eng = {}
            for e in ENGINES:
                c, t = kv.get(f"drm-cycles-{e}"), kv.get(f"drm-total-cycles-{e}")
                if c and t:
                    eng[e] = (int(c), int(t))
            out[cid] = eng
    return out


def gpu_busy(a, b, prefix):
    """Engine busy % between two drm_clients() snapshots, summed over clients."""
    res = {}
    for e in ENGINES:
        num = den = 0
        for cid, eb in b.items():
            if cid in a and e in eb and e in a[cid]:
                num += eb[e][0] - a[cid][e][0]
                den = max(den, eb[e][1] - a[cid][e][1])
        if den > 0:
            res[f"{prefix}_{e}_busy"] = round(100.0 * num / den, 1)
    return res


def window_sample(secs, pid=None, extra=None):
    """Sample battery power and media freq for `secs`; GPU busy from fdinfo deltas
    for the whole system (`gpu_*`) and for `pid` (`sender_gpu_*`)."""
    sys0, own0 = drm_clients(), drm_clients([str(pid)]) if pid else {}
    watts, freqs = [], []
    end = time.time() + secs
    try:
        while time.time() < end:
            w = battery_watts()
            if w is not None:
                watts.append(w)
            try:
                freqs.append(int(open(MEDIA_FREQ).read()))
            except OSError:
                pass
            if extra:
                extra()
            time.sleep(0.25)
    finally:
        gpu = gpu_busy(sys0, drm_clients(), "gpu")
        if pid:
            gpu.update(gpu_busy(own0, drm_clients([str(pid)]), "sender_gpu"))
    return watts, freqs, gpu


def bat_fields(watts):
    return {"bat_w_med": round(statistics.median(watts), 2) if watts else None,
            "bat_w_mean": round(statistics.mean(watts), 2) if watts else None,
            "bat_status": battery_status()}


def idle_baseline():
    watts, _, gpu = window_sample(window)
    return {"encoder": "none", "workload": "baseline", **bat_fields(watts), **gpu,
            "samples": [len(watts)]}


def run(encoder, workload):
    log_path = os.path.join(outdir, f"tx-{encoder}-{workload}.log")
    log = open(log_path, "w")
    p = subprocess.Popen(
        [sender, "run", f"--to={target}", f"--encoder={encoder}", f"--workspace={WS}"],
        stdout=log, stderr=subprocess.STDOUT)
    # Wait until the session is up.
    t0 = time.time()
    while time.time() - t0 < 20:
        time.sleep(0.5)
        with open(log_path) as f:
            if "encoder:" in f.read():
                break
    else:
        p.send_signal(signal.SIGINT); p.wait(10)
        return {"encoder": encoder, "workload": workload, "error": "no session"}
    if WORK[workload]:
        # Hyprland >= 0.55 is Lua-configured: `dispatch exec [rules]` is rejected.
        hypr("eval", f'hl.exec_cmd([[{WORK[workload]}]], {{ workspace = "{WS} silent" }})')
    time.sleep(6)  # warm-up: window mapped, encoder and GPU clocks settled
    mark = os.path.getsize(log_path)
    c0, w0 = cpu_ticks(p.pid), time.time()
    watts, freqs, gpu = window_sample(window, p.pid)
    cores = (cpu_ticks(p.pid) - c0) / TICK / (time.time() - w0)
    with open(log_path) as f:
        f.seek(mark)
        tail = ANSI.sub("", f.read())
    kill_work()
    p.send_signal(signal.SIGINT)
    try:
        p.wait(15)
    except subprocess.TimeoutExpired:
        p.kill()
    log.close()

    sent = [m for m in re.finditer(
        r"sent ([\d.]+) fps ([\d.]+) Mbit/s \| encode p50 ([\d.]+) ms p95 ([\d.]+) ms \| "
        r"capture->send p50 ([\d.]+) ms p95 ([\d.]+) ms", tail)]
    phone = []
    for line in tail.splitlines():
        if "PHONE-STATS" in line:
            try:
                phone.append(json.loads(line.split("PHONE-STATS", 1)[1].strip()))
            except ValueError:
                pass
    g = lambda i: [float(m.group(i)) for m in sent]
    pk = lambda k: [float(s[k]) for s in phone if k in s]
    return {
        "encoder": encoder, "workload": workload,
        "cpu_cores": round(cores, 3),
        "media_mhz_med": med(freqs),
        **bat_fields(watts), **gpu,
        "sent_fps": med(g(1)), "mbps": med(g(2)),
        "enc_p50": med(g(3)), "enc_p95": med(g(4)),
        "cap2send_p50": med(g(5)), "cap2send_p95": med(g(6)),
        "ipad_fps": med(pk("fps")), "ipad_e2e50": med(pk("e2e50")),
        "ipad_e2e95": med(pk("e2e95")), "ipad_dec50": med(pk("dec50")),
        "ipad_hitch": med(pk("hitch")), "ipad_stalls_max": max(pk("stalls"), default=None),
        "samples": [len(sent), len(phone)],
    }


kill_work()
os.makedirs(outdir, exist_ok=True)
results = []
r = idle_baseline()
print(json.dumps(r), flush=True)
results.append(r)
for w in workloads:
    for e in encoders:
        r = run(e, w)
        print(json.dumps(r), flush=True)
        results.append(r)
        time.sleep(2)
with open(os.path.join(outdir, "results.jsonl"), "a") as f:
    for r in results:
        f.write(json.dumps(r) + "\n")

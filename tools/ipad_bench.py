#!/usr/bin/env python3
"""A/B od-sender encoders against the real iPad over a usbmux loopback forward.

usage: python3 -I ipad_bench.py SENDER PORT OUTDIR ENCODER[,ENCODER] WORKLOAD[,WORKLOAD] [SECONDS]
"""
import json, os, re, signal, statistics, subprocess, sys, time

sender, port, outdir = sys.argv[1], sys.argv[2], sys.argv[3]
encoders = sys.argv[4].split(",")
workloads = sys.argv[5].split(",")
window = float(sys.argv[6]) if len(sys.argv) > 6 else 20.0
WS = "9"
TICK = os.sysconf("SC_CLK_TCK")
MEDIA_FREQ = "/sys/class/drm/card0/device/tile0/gt1/freq0/act_freq"
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


def run(encoder, workload):
    log_path = os.path.join(outdir, f"tx-{encoder}-{workload}.log")
    log = open(log_path, "w")
    p = subprocess.Popen(
        [sender, "run", f"--to=127.0.0.1:{port}", f"--encoder={encoder}", f"--workspace={WS}"],
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
    freqs = []
    end = w0 + window
    while time.time() < end:
        try:
            freqs.append(int(open(MEDIA_FREQ).read()))
        except OSError:
            pass
        time.sleep(0.25)
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
        "sent_fps": med(g(1)), "mbps": med(g(2)),
        "enc_p50": med(g(3)), "enc_p95": med(g(4)),
        "cap2send_p50": med(g(5)), "cap2send_p95": med(g(6)),
        "ipad_fps": med(pk("fps")), "ipad_e2e50": med(pk("e2e50")),
        "ipad_e2e95": med(pk("e2e95")), "ipad_dec50": med(pk("dec50")),
        "ipad_hitch": med(pk("hitch")), "ipad_stalls_max": max(pk("stalls"), default=None),
        "samples": [len(sent), len(phone)],
    }


kill_work()
results = []
for w in workloads:
    for e in encoders:
        r = run(e, w)
        print(json.dumps(r), flush=True)
        results.append(r)
        time.sleep(2)
with open(os.path.join(outdir, "results.jsonl"), "a") as f:
    for r in results:
        f.write(json.dumps(r) + "\n")

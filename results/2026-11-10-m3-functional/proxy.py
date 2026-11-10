#!/usr/bin/env python3
"""TCP proxy LISTEN -> UPSTREAM. SIGUSR1 cuts every live connection (simulates a dropped link)."""
import signal, socket, sys, threading
listen, upstream = sys.argv[1], sys.argv[2]
lh, lp = listen.rsplit(":", 1); uh, up = upstream.rsplit(":", 1)
live, lock = [], threading.Lock()
def pipe(a, b):
    try:
        while (d := a.recv(65536)):
            b.sendall(d)
    except OSError:
        pass
    for s in (a, b):
        try: s.shutdown(socket.SHUT_RDWR)
        except OSError: pass
def cut(*_):
    with lock:
        n = len(live)
        for s in live:
            try: s.shutdown(socket.SHUT_RDWR)
            except OSError: pass
        live.clear()
    print(f"proxy: cut {n} sockets", flush=True)
signal.signal(signal.SIGUSR1, cut)
srv = socket.create_server((lh, int(lp)))
print(f"proxy: {listen} -> {upstream}", flush=True)
while True:
    try:
        c, _ = srv.accept()
    except InterruptedError:
        continue
    u = socket.create_connection((uh, int(up)))
    with lock: live.extend([c, u])
    threading.Thread(target=pipe, args=(c, u), daemon=True).start()
    threading.Thread(target=pipe, args=(u, c), daemon=True).start()
    print("proxy: connection", flush=True)

# M3 functional checks against the iPad (2026-11-10)

T1 HEVC baseline, T2 `OD_FORCE_H264=1`, T3 debug `OD_FAIL_HEVC_ENCODER=1`
(probe fails), T4 debug `OD_FAIL_HEVC_SESSION=1` (late fallback), T5 reconnect
through `proxy.py` (SIGUSR1 cuts the link), T6 late fallback then reconnect
(stays H.264), T7 `od-receiver` without HEVC (gets H.264). All passed; logs
ANSI-stripped. Runner: `runtest.sh`.

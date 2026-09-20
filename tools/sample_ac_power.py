#!/usr/bin/env python3
"""Sample the inverter's AC power (and the grid meter's) through the proxy.

Reads both values from *one* block each, so the value and its scale factor always
come from the same read - the scale factors on this inverter are dynamic
(documented in docs/REGISTERS.md), so splitting them across reads can mis-scale by
10x.

    model 101 (inverter)  W at 40083, W_SF at 40084
    model 203 (grid)      M_AC_Power at 40206, per phase 40207..40209, SF at 40210

Usage:
    python3 tools/sample_ac_power.py [--count 10] [--interval 2] [--port 1503]
"""

import argparse
import sys
import time
from datetime import datetime

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from probe_via_proxy import read  # noqa: E402

INVERTER_W = 40083      # model 101, index 12
GRID_W = 40206          # model 203, index 16 (export positive)


def signed(word):
    return word - 0x10000 if word >= 0x8000 else word


def scaled(raw_word, sf_word):
    return signed(raw_word) * (10 ** signed(sf_word))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=1503)
    ap.add_argument("--count", type=int, default=10)
    ap.add_argument("--interval", type=float, default=2.0)
    args = ap.parse_args()

    print("reading through the proxy at %s:%d, %d samples every %.1fs"
          % (args.host, args.port, args.count, args.interval))
    print()
    print("  #  time      inverter AC power (101:W)      grid AC power (203:M_AC_Power)")
    print("  -- --------  ------------------------------  ---------------------------------")

    readings = []
    for n in range(1, args.count + 1):
        stamp = datetime.now().strftime("%H:%M:%S")
        try:
            inv, _raw = read(args.host, args.port, 1, INVERTER_W, 2)
            inv_w = scaled(inv[0], inv[1])
            inv_text = "%9.1f W  (raw %6d, SF %3d)" % (inv_w, signed(inv[0]), signed(inv[1]))
        except Exception as exc:
            inv_w, inv_text = None, "   FAILED: %s" % exc

        try:
            grid, _raw = read(args.host, args.port, 1, GRID_W, 5)
            raw_total = signed(grid[0])
            sf = signed(grid[4])
            grid_w = scaled(grid[0], grid[4])
            phase_w = [signed(v) * (10 ** sf) for v in grid[1:4]]
            phase_sum = sum(phase_w)
            # phases should sum to the total; a live-updating block can be torn
            flag = "" if abs(phase_sum - grid_w) <= max(1.0, abs(grid_w) * 0.05) else "  <-- phases do not sum to the total (torn read)"
            grid_text = "%8.1f W import  (raw %6d, SF %3d, phases %s W)%s" % (
                -grid_w, raw_total, sf,
                " ".join("%.1f" % v for v in phase_w), flag)
        except Exception as exc:
            grid_w, grid_text = None, "   FAILED: %s" % exc

        print("  %2d %s  %s  %s" % (n, stamp, inv_text, grid_text))
        readings.append((n, stamp, inv_w, grid_w))

        if n < args.count:
            time.sleep(args.interval)

    values = [r[2] for r in readings if r[2] is not None]
    failures = sum(1 for r in readings if r[2] is None)
    print()
    if values:
        print("inverter AC power: %d of %d reads, min %.1f W, max %.1f W, spread %.1f W"
              % (len(values), args.count, min(values), max(values), max(values) - min(values)))
    if failures:
        print("%d read(s) failed" % failures)
    grid_values = [r[3] for r in readings if r[3] is not None]
    if grid_values:
        print("grid AC power (import positive): min %.1f W, max %.1f W"
              % (min(grid_values), max(grid_values)))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
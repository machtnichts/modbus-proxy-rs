#!/usr/bin/env python3
"""Analyse the proxy's client-request log: at what interval does the client poll?

The proxy logs every client request at DEBUG level as

    2026-09-13 14:15:30.123 DEBUG   muxproxy: pdu from 172.18.0.2:54321: 030000007d

so the timestamps give the cadence directly and the PDU hex gives the request mix
(which function code, which register, how many registers).

Requests are grouped into "cycles" by a gap threshold: everything closer than
`--burst-gap` seconds belongs to the same poll cycle. That separates "how often
does it ask" from "how many requests per ask".

Usage:
    python3 tools/analyse_client_requests.py [--log PATH] [--burst-gap 1.0] [--peer SUBSTR]
"""

import argparse
import collections
import os
import re
import statistics
import sys

DEFAULT_LOG = "/home/adermake/EV-CHARGER-WT-HA/logs/muxproxy-measure.log"

REPLY = re.compile(r"pdu from (?P<peer>[^\s:]+):\d+: (?P<pdu>[0-9a-fA-F]+)")
LATENCY = re.compile(
    r"reply to (?P<peer>[^\s:]+):\d+: unit=(?P<unit>\d+) tid=(?P<tid>\d+) "
    r"req=(?P<req>[0-9a-fA-F]+) -> resp=(?P<resp>[0-9a-fA-F]+) in (?P<ms>\d+)ms"
)
TIMEOUT = re.compile(r"upstream tid=(?P<tid>\d+) TIMEOUT after (?P<ms>\d+)ms")


def latencies(log_path, peer_filter):
    """Every reply the proxy sent, with its latency - and every upstream timeout."""
    replies = []
    timeouts = []
    with open(log_path, errors="replace") as fh:
        for line in fh:
            stamp = line[:23] if len(line) > 23 else ""
            m = LATENCY.search(line)
            if m:
                peer = m.group("peer")
                if peer.startswith("::ffff:"):
                    peer = peer[7:]
                if peer_filter and peer_filter not in peer:
                    continue
                replies.append((stamp, peer, int(m.group("ms")),
                                m.group("req"), m.group("resp")))
                continue
            t = TIMEOUT.search(line)
            if t:
                timeouts.append((stamp, int(t.group("ms")), t.group("tid")))
    return replies, timeouts


def report_latency(log_path, peer_filter):
    replies, timeouts = latencies(log_path, peer_filter)
    if not replies:
        print("no replies recorded yet")
        return
    by_peer = collections.defaultdict(list)
    for stamp, peer, ms, req, resp in replies:
        by_peer[peer].append((stamp, ms, req, resp))

    print()
    print("=== reply latency (proxy -> client) ===")
    for peer in sorted(by_peer):
        rows = by_peer[peer]
        times = sorted(r[1] for r in rows)
        p95 = times[int(len(times) * 0.95) - 1] if times else 0
        print("  %s: %d replies, median %d ms, p95 %d ms, max %d ms"
              % (peer, len(rows), statistics.median(times), p95, times[-1]))
        slow = sorted(rows, key=lambda r: -r[1])[:3]
        for stamp, ms, req, resp in slow:
            print("      slowest: %s  %5d ms  req=%s resp=%s" % (stamp, ms, req, resp))

    if timeouts:
        print()
        print("=== upstream timeouts (the device did not answer in time) ===")
        print("  %d timeouts, first %s, last %s"
              % (len(timeouts), timeouts[0][0], timeouts[-1][0]))
        for stamp, ms, tid in timeouts[-5:]:
            print("      %s  tid=%s after %d ms" % (stamp, tid, ms))
    else:
        print()
        print("=== upstream timeouts: none ===")


def decode(pdu_hex):
    """(function code, description) for a client PDU."""
    raw = bytes.fromhex(pdu_hex)
    if not raw:
        return (None, "empty")
    fc = raw[0]
    if fc in (3, 4) and len(raw) >= 5:
        addr = int.from_bytes(raw[1:3], "big")
        count = int.from_bytes(raw[3:5], "big")
        return (fc, "read %s %d..%d (%d regs)" % ("holding" if fc == 3 else "input", addr, addr + count - 1, count))
    if fc in (1, 2) and len(raw) >= 5:
        addr = int.from_bytes(raw[1:3], "big")
        count = int.from_bytes(raw[3:5], "big")
        return (fc, "read %s %d..%d (%d bits)" % ("coils" if fc == 1 else "discrete", addr, addr + count - 1, count))
    if fc == 6 and len(raw) >= 5:
        return (fc, "write single %d = %d" % (int.from_bytes(raw[1:3], "big"), int.from_bytes(raw[3:5], "big")))
    if fc == 16 and len(raw) >= 5:
        return (fc, "write multiple %d (%d regs)" % (int.from_bytes(raw[1:3], "big"), int.from_bytes(raw[3:5], "big")))
    canary = raw[1] if len(raw) > 1 and raw[1] == 0x0E else ""
    if fc == 0x41:
        return (fc, "device identification (0x41%s)" % (" meicode" if canary else ""))
    return (fc, "function 0x%02x" % fc)


def parse(log_path, peer_filter):
    events = []
    with open(log_path, errors="replace") as fh:
        for line in fh:
            m = REPLY.search(line)
            if not m:
                continue
            peer = m.group("peer")
            if peer.startswith("::ffff:"):
                peer = peer[7:]
            if peer_filter and peer_filter not in peer:
                continue
            # the logger prefixes every line with "YYYY-MM-DD HH:MM:SS.mmm"
            events.append((line[:23], peer, m.group("pdu")))
    return events


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", default=DEFAULT_LOG)
    ap.add_argument("--burst-gap", type=float, default=1.0,
                    help="seconds; requests closer than this are one poll cycle")
    ap.add_argument("--peer", default=None, help="only this client (substring)")
    ap.add_argument("--show", type=int, default=0, help="print the first N raw request lines")
    args = ap.parse_args()

    if not os.path.exists(args.log):
        print("no log at %s - is the measurement proxy running?" % args.log)
        return 2

    events = parse(args.log, args.peer)
    if not events:
        print("no client requests found in %s" % args.log)
        return 2

    by_peer = collections.defaultdict(list)
    for ts, peer, pdu in events:
        by_peer[peer].append((ts, pdu))

    print("log: %s" % args.log)
    print("total client requests: %d, from %d client(s): %s"
          % (len(events), len(by_peer), ", ".join(sorted(by_peer))))
    if args.show:
        for ts, peer, pdu in events[:args.show]:
            print("  %s  %s  %s" % (ts, peer, pdu))

    for peer in sorted(by_peer):
        requests = by_peer[peer]
        stamps = [r[0] for r in requests]

        # group into poll cycles
        cycles, current = [], [requests[0]]
        for prev, cur in zip(requests, requests[1:]):
            d = seconds_between(prev[0], cur[0])
            if d > args.burst_gap:
                cycles.append(current)
                current = [cur]
            else:
                current.append(cur)
        cycles.append(current)

        print()
        print("=== client %s ===" % peer)
        print("  requests            : %d" % len(requests))
        print("  first / last        : %s / %s" % (stamps[0], stamps[-1]))

        if len(cycles) > 1:
            gaps = [seconds_between(cycles[i][0][0], cycles[i + 1][0][0])
                    for i in range(len(cycles) - 1)]
            print("  poll cycles         : %d" % len(cycles))
            print("  interval between    : median %.2f s, mean %.2f s, min %.2f s, max %.2f s"
                  % (statistics.median(gaps), statistics.mean(gaps), min(gaps), max(gaps)))
            hist = collections.Counter(round(g) for g in gaps)
            print("  interval histogram  : %s" % ", ".join(
                "%ds x%d" % (k, v) for k, v in sorted(hist.items())))
        per_cycle = collections.Counter(len(c) for c in cycles)
        print("  requests per cycle  : %s" % ", ".join(
            "%d req x%d cycles" % (k, v) for k, v in sorted(per_cycle.items())))

        # what it asks for
        kinds = collections.Counter(decode(p)[1] for _, p in requests)
        print("  request mix         :")
        for kind, n in kinds.most_common(20):
            print("      %5d x  %s" % (n, kind))

        # the shape of a typical cycle
        if cycles:
            busiest = max(cycles, key=len)
            print("  one full cycle      :")
            t0 = busiest[0][0]
            for ts, pdu in busiest:
                print("      +%6.3fs  %s" % (seconds_between(t0, ts), decode(pdu)[1]))

    report_latency(args.log, args.peer)
    return 0


def seconds_between(a, b):
    from datetime import datetime
    fmt = "%Y-%m-%d %H:%M:%S.%f"
    return (datetime.strptime(b, fmt) - datetime.strptime(a, fmt)).total_seconds()


if __name__ == "__main__":
    sys.exit(main())

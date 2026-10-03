#!/usr/bin/env python3
"""Differential test: the current build against a known-good baseline binary.

Both sides are this same proxy, one version apart: the freshly built
`target/release/muxproxy` and a saved baseline (`baseline/muxproxy`, normally the binary
that was installed and in service before this change). Each gets its own stub device (same
deterministic behaviour: reg[i] = i * 10, addresses 50-52 forbidden) and then the identical
scripted sequence of requests. Every response PDU is compared byte for byte.

This is stronger than "the same test suite passes": it catches divergences a suite would
not think to ask about - an exception code chosen differently, a byte count packed
differently, a write echoed in another shape. It is how the Rust proxy was checked against
the Python implementation it was ported from, and it is how the next change is checked
against the build that is running now.

No poll ranges are configured, so both sides answer purely on demand and the comparison
stays deterministic.

Usage:
    python3 tools/differential_test.py                     # build vs baseline/muxproxy
    python3 tools/differential_test.py --against <binary>  # build vs any other binary
    python3 tools/differential_test.py --allow-missing-baseline   # for `make check`
"""

import argparse
import hashlib
import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
RS = os.path.dirname(HERE)
CURRENT = os.path.join(RS, "target", "release", "muxproxy")
DEFAULT_BASELINE = os.path.join(RS, "baseline", "muxproxy")
STUB = os.path.join(RS, "target", "release", "stubmodbus")

# (label, unit id, PDU)
SEQUENCE = [
    ("read holding 0..4", 1, struct.pack(">BHH", 3, 0, 4)),
    ("read holding 0..4 again (cached)", 1, struct.pack(">BHH", 3, 0, 4)),
    ("read forbidden range -> device exception", 1, struct.pack(">BHH", 3, 50, 3)),
    ("read count 200 -> illegal", 1, struct.pack(">BHH", 3, 0, 200)),
    ("read count 0 -> illegal", 1, struct.pack(">BHH", 3, 0, 0)),
    ("write single reg 5 = 1234", 1, struct.pack(">BHH", 6, 5, 1234)),
    ("read back 4..2 (cache must be invalidated)", 1, struct.pack(">BHH", 3, 4, 2)),
    ("write multiple 10..2", 1,
     struct.pack(">BHHB", 16, 10, 2, 4) + struct.pack(">HH", 777, 888)),
    ("read back 10..2", 1, struct.pack(">BHH", 3, 10, 2)),
    ("read coils (FC1) unsupported by device", 1, struct.pack(">BHH", 1, 0, 8)),
    ("read input regs (FC4) unsupported by device", 1, struct.pack(">BHH", 4, 0, 2)),
    ("device identification (FC 0x41) unsupported", 1, bytes([0x41, 0x0E])),
    ("read holding, unit id 7", 7, struct.pack(">BHH", 3, 0, 2)),
    ("read holding far block", 1, struct.pack(">BHH", 3, 300, 5)),
]


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def wait_port(port, timeout=10.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), 0.25):
                return True
        except OSError:
            time.sleep(0.05)
    return False


def mb_request(port, unit, pdu, tid=0x1234):
    """One exchange; returns the response PDU (or an error marker)."""
    try:
        s = socket.create_connection(("127.0.0.1", port), 3)
    except OSError as e:
        return "CONNECT-FAILED: %s" % e
    s.settimeout(5)
    try:
        frame = struct.pack(">HHHB", tid, 0, len(pdu) + 1, unit) + pdu
        s.sendall(frame)
        hdr = b""
        while len(hdr) < 7:
            chunk = s.recv(7 - len(hdr))
            if not chunk:
                return "CLOSED-EARLY"
            hdr += chunk
        rtid, pid, length, runit = struct.unpack(">HHHB", hdr)
        body = b""
        while len(body) < length - 1:
            chunk = s.recv(length - 1 - len(body))
            if not chunk:
                return "CLOSED-EARLY"
            body += chunk
        if rtid != tid:
            return "BAD-TID %04x" % rtid
        if pid != 0:
            return "BAD-PID %04x" % pid
        if runit != unit:
            return "UNIT-MISMATCH %d" % runit
        return body.hex()
    finally:
        s.close()


def start_stub(port):
    return subprocess.Popen([STUB, "--port", str(port), "--quiet"],
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)


def start_proxy(binary, config):
    tmp = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
    json.dump(config, tmp, indent=2)
    tmp.close()
    return subprocess.Popen([binary, "-c", tmp.name],
                            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()[:16]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--against", default=DEFAULT_BASELINE,
                    help="the known-good binary to compare against (default %s)"
                         % os.path.relpath(DEFAULT_BASELINE, RS))
    ap.add_argument("--current", default=CURRENT, help="the freshly built binary")
    ap.add_argument("--allow-missing-baseline", action="store_true",
                    help="exit 0 with a note when the baseline is absent (used by `make check`)")
    args = ap.parse_args()

    if not os.path.exists(args.current):
        print("missing current build: %s (run `make build` first)" % args.current)
        return 2
    if not os.path.exists(STUB):
        print("missing stub device: %s (run `make build` first)" % STUB)
        return 2
    if not os.path.exists(args.against):
        print("no baseline binary at %s" % args.against)
        print("A baseline is the build that was installed and known to work - the point of")
        print("this comparison is one version against the next. Save one BEFORE installing a")
        print("new build:  make baseline   (copies bin/muxproxy to baseline/muxproxy)")
        return 0 if args.allow_missing_baseline else 2

    same_bytes = sha256(args.current) == sha256(args.against)

    baseline_stub, current_stub = free_port(), free_port()
    baseline_listen, current_listen = free_port(), free_port()
    baseline_http, current_http = free_port(), free_port()

    def config(listen, http, upstream):
        return {
            "listen": {"host": "127.0.0.1", "port": listen},
            "upstream": {"host": "127.0.0.1", "port": upstream, "unit": 1,
                         "connect_timeout": 5.0, "response_timeout": 5.0},
            "http": {"host": "127.0.0.1", "port": http},
            "poll": {"interval_active": 300.0, "interval_idle": 300.0,
                     "min_request_gap": 0.01, "ondemand_ttl": 5.0,
                     "startup_delay": 5.0, "max_registers_per_read": 125,
                     "ranges": []},
            "policy": {"allow_writes": True, "log_every_request": False},
            "logging": {"level": "ERROR", "file": None},
        }

    procs = []
    try:
        procs.append(start_stub(baseline_stub))
        procs.append(start_stub(current_stub))
        if not wait_port(baseline_stub) or not wait_port(current_stub):
            print("a stub never came up")
            return 2

        procs.append(start_proxy(args.against, config(baseline_listen, baseline_http,
                                                      baseline_stub)))
        procs.append(start_proxy(args.current, config(current_listen, current_http,
                                                      current_stub)))
        if not wait_port(baseline_listen) or not wait_port(current_listen):
            print("a proxy never came up")
            return 2

        print("baseline : %s  %s" % (args.against, sha256(args.against)))
        print("current  : %s  %s" % (args.current, sha256(args.current)))
        if same_bytes:
            print("NOTE: both are the same bytes - this run proves nothing was rebuilt, not")
            print("      that a change was verified. Rebuild before trusting it.")
        print("-" * 78)
        print("%-46s %-9s %s" % ("request", "result", "response PDU"))
        print("-" * 78)

        mismatches = []
        for label, unit, pdu in SEQUENCE:
            a = mb_request(baseline_listen, unit, pdu)
            b = mb_request(current_listen, unit, pdu)
            ok = a == b
            if not ok:
                mismatches.append(label)
            print("%-46s %-9s %s" % (label[:46], "MATCH" if ok else "DIFFER", b))
            if not ok:
                print("%-46s %-9s %s" % ("  ^ baseline said", "", a))

        print("-" * 78)
        total = len(SEQUENCE)
        print("%d/%d responses identical" % (total - len(mismatches), total))
        if mismatches:
            print("differing: %s" % ", ".join(mismatches))
            return 1
        return 0
    finally:
        for p in procs:
            p.terminate()
        time.sleep(0.3)
        for p in procs:
            p.kill()


if __name__ == "__main__":
    sys.exit(main())

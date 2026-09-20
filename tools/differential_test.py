#!/usr/bin/env python3
"""Differential test: Python proxy vs Rust proxy, byte for byte.

Both implementations get their own stub device (same deterministic behaviour:
reg[i] = i * 10, addresses 50-52 forbidden) and then receive an identical
scripted sequence of requests. Every response PDU is compared byte for byte.

This is stronger than "both pass the same tests": it catches divergences that a
suite would not think to ask about - an exception code chosen differently, a
byte count packed differently, a write echoed in another shape.

No poll ranges are configured, so both sides answer purely on demand and the
comparison stays deterministic.

Usage: python3 tools/differential_test.py
"""

import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
RS = os.path.dirname(HERE)
PY_PROXY = os.path.join(os.path.dirname(RS), "modbus-proxy", "muxproxy.py")
RS_PROXY = os.path.join(RS, "target", "release", "muxproxy")
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


def start_proxy(cmd, config, port):
    tmp = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
    json.dump(config, tmp, indent=2)
    tmp.close()
    proc = subprocess.Popen(cmd + ["-c", tmp.name],
                            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return proc


def main():
    for path, what in ((PY_PROXY, "python proxy"), (RS_PROXY, "rust proxy"),
                       (STUB, "stub device")):
        if not os.path.exists(path):
            print("missing %s: %s" % (what, path))
            return 2

    py_stub, rs_stub = free_port(), free_port()
    py_listen, rs_listen = free_port(), free_port()
    py_http, rs_http = free_port(), free_port()

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
        procs.append(start_stub(py_stub))
        procs.append(start_stub(rs_stub))
        if not wait_port(py_stub) or not wait_port(rs_stub):
            print("a stub never came up")
            return 2

        procs.append(start_proxy([sys.executable, PY_PROXY],
                                 config(py_listen, py_http, py_stub), py_listen))
        procs.append(start_proxy([RS_PROXY],
                                 config(rs_listen, rs_http, rs_stub), rs_listen))
        if not wait_port(py_listen) or not wait_port(rs_listen):
            print("a proxy never came up")
            return 2

        print("python proxy : %s" % PY_PROXY)
        print("rust proxy   : %s" % RS_PROXY)
        print("-" * 78)
        print("%-46s %-9s %s" % ("request", "result", "response PDU"))
        print("-" * 78)

        mismatches = []
        for label, unit, pdu in SEQUENCE:
            a = mb_request(py_listen, unit, pdu)
            b = mb_request(rs_listen, unit, pdu)
            ok = a == b
            if not ok:
                mismatches.append(label)
            print("%-46s %-9s %s" % (label[:46], "MATCH" if ok else "DIFFER", b))
            if not ok:
                print("%-46s %-9s %s" % ("  ^ python said", "", a))

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

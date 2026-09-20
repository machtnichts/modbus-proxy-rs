#!/usr/bin/env python3
"""Read registers through the proxy, with a raw Modbus/TCP request.

Used to prove the proxy's upstream path before a real client depends on it:
the SolarEdge exposes SunSpec model headers that are easy to assert on.

Usage: python3 tools/probe_via_proxy.py [--host 127.0.0.1] [--port 1503]
                                        [--read ADDR COUNT]...
Defaults to the two SunSpec headers the production config expects:
   40069 -> [101, 50]    inverter model 101
   40188 -> [203, 105]   meter model 203
"""

import argparse
import socket
import struct
import sys


def read(host, port, unit, address, count, timeout=8):
    frame = struct.pack(">HHHB", 0x1234, 0, 6, unit) + struct.pack(">BHH", 3, address, count)
    with socket.create_connection((host, port), timeout) as s:
        s.settimeout(timeout)
        s.sendall(frame)
        head = b""
        while len(head) < 7:
            chunk = s.recv(7 - len(head))
            if not chunk:
                raise RuntimeError("connection closed while reading the header")
            head += chunk
        tid, pid, length, runit = struct.unpack(">HHHB", head)
        body = b""
        while len(body) < length - 1:
            chunk = s.recv(length - 1 - len(body))
            if not chunk:
                raise RuntimeError("connection closed while reading the body")
            body += chunk
    if body[0] & 0x80:
        raise RuntimeError("device answered with exception 0x%02x" % body[1])
    # response PDU is [function, byte count, data...]
    byte_count = body[1]
    values = [struct.unpack(">H", body[2 + i:4 + i])[0] for i in range(0, byte_count, 2)]
    return values, body.hex()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=1503)
    ap.add_argument("--unit", type=int, default=1)
    ap.add_argument("--read", nargs=2, action="append", metavar=("ADDR", "COUNT"), type=int)
    args = ap.parse_args()

    targets = args.read or [[40069, 2], [40188, 2]]
    failures = 0
    for address, count in targets:
        try:
            values, raw = read(args.host, args.port, args.unit, address, count)
        except Exception as exc:
            print("read %d..%d  FAILED  %s" % (address, address + count - 1, exc))
            failures += 1
            continue
        hexes = " ".join("%04x" % v for v in values)
        hint = ""
        if address == 40069 and values[:2] == [101, 50]:
            hint = "  <- SunSpec inverter model 101, 50 points"
        elif address == 40188 and values[:2] == [203, 105]:
            hint = "  <- SunSpec meter model 203, 105 points"
        print("read %d..%d  OK  %s (%s)%s" % (address, address + count - 1, values, hexes, hint))
        print("             raw response PDU: %s" % raw)

    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
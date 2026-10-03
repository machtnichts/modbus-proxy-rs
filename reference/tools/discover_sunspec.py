#!/usr/bin/env python3
"""
Serialized SolarEdge SunSpec discovery.

The inverter is fragile: it answers one TCP client at a time and misbehaves when
requests overlap. This tool therefore:
  * opens exactly ONE connection,
  * sends ONE request at a time, waiting for the full response,
  * validates the MBAP transaction id of every reply,
  * pauses between requests,
  * retries a failing request before giving up.

Output: a definitive register map (models + offsets) on stdout, and a JSON file
suitable for configuring the multiplexing proxy.
"""
from __future__ import annotations

import json
import socket
import struct
import sys
import time

HOST = "192.168.178.84"
PORT = 1502
UNIT = 1
PAUSE = 0.25          # between upstream requests
TIMEOUT = 10.0
RETRIES = 4

MODEL_NAMES = {
    1: "common",
    101: "inverter-3ph",
    102: "inverter-1ph",
    103: "inverter-split",
    124: "storage",
    160: "mppt",
    201: "meter-1ph",
    202: "meter-split",
    203: "meter-3ph",
    204: "meter-3ph-dir",
    802: "battery",
    803: "battery",
    804: "battery",
}

_tid = 0


def _next_tid() -> int:
    global _tid
    _tid = (_tid + 1) & 0xFFFF
    return _tid


def read_registers(sock: socket.socket, addr: int, count: int, fc: int = 3):
    """Blocking single request/response with TID + framing validation."""
    last = None
    for attempt in range(RETRIES):
        try:
            tid = _next_tid()
            pdu = struct.pack(">BHH", fc, addr, count)
            req = struct.pack(">HHHB", tid, 0, len(pdu) + 1, UNIT) + pdu
            sock.sendall(req)

            hdr = b""
            while len(hdr) < 7:
                chunk = sock.recv(7 - len(hdr))
                if not chunk:
                    raise IOError("connection closed by peer")
                hdr += chunk
            rtid, _pid, length, _uid = struct.unpack(">HHHB", hdr)
            if rtid != tid:
                raise IOError("transaction id mismatch (want %d got %d)" % (tid, rtid))

            body = b""
            need = max(length - 1, 0)
            while len(body) < need:
                chunk = sock.recv(need - len(body))
                if not chunk:
                    break
                body += chunk
            if not body:
                raise IOError("empty response body")
            if body[0] & 0x80:
                raise IOError("modbus exception 0x%02x" % body[1])
            if body[0] != fc:
                raise IOError("unexpected function code 0x%02x" % body[0])
            nbytes = body[1]
            if nbytes != count * 2:
                raise IOError("byte count %d != expected %d" % (nbytes, count * 2))
            return list(struct.unpack(">%dH" % count, body[2:2 + nbytes]))
        except Exception as exc:  # noqa: BLE001 - retry any transport failure
            last = exc
            time.sleep(0.8)
    raise IOError("read(addr=%d count=%d fc=%d) failed: %s" % (addr, count, fc, last))


def u16s(regs) -> str:
    return "".join(chr(r >> 8) + chr(r & 0xFF) for r in regs)


def main() -> int:
    print("connecting to %s:%d ..." % (HOST, PORT))
    sock = socket.create_connection((HOST, PORT), timeout=TIMEOUT)
    sock.settimeout(TIMEOUT)
    print("connected")

    # --- 1. locate the SunS marker, one register at a time -------------------
    print("\n[1] hunting 'SunS' one register at a time")
    suns_at = None
    for addr in (39998, 39999, 40000, 40001, 40002):
        try:
            r = read_registers(sock, addr, 1)
        except Exception as exc:  # noqa: BLE001
            print("   PDU %-6d ERR %s" % (addr, exc))
            continue
        pair = ""
        if addr + 1 <= 65535:
            try:
                r2 = read_registers(sock, addr + 1, 1)
                pair = u16s(r + r2)
            except Exception:  # noqa: BLE001
                pair = "?"
        print("   PDU %-6d = 0x%04X %-4s | as pair: %r" % (addr, r[0], repr(u16s(r)), pair))
        if pair.startswith("SunS"):
            suns_at = addr
            break
        time.sleep(PAUSE)

    if suns_at is None:
        print("\nFATAL: could not locate the SunS marker")
        return 2
    print("   'SunS' occupies PDU %d..%d" % (suns_at, suns_at + 1))

    # --- 2. walk the model chain -------------------------------------------
    print("\n[2] walking the SunSpec model chain")
    models = {}
    pdu = suns_at + 2
    for _ in range(30):
        try:
            hdr = read_registers(sock, pdu, 2)
        except Exception as exc:  # noqa: BLE001
            print("   chain stopped at PDU %d: %s" % (pdu, exc))
            break
        did, length = hdr
        if did in (0, 0xFFFF) or length in (0, 0xFFFF) or length > 2000:
            print("   chain end marker at PDU %d (did=%d len=%d)" % (pdu, did, length))
            break
        name = MODEL_NAMES.get(did, "unknown")
        print("   PDU %-6d DID=%-5d %-14s len=%d  data PDU %d..%d"
              % (pdu, did, name, length, pdu + 2, pdu + 1 + length))
        models[did] = {"header": pdu, "len": length, "name": name, "start": pdu + 2}
        pdu += 2 + length
        time.sleep(PAUSE)

    if not models:
        print("\nFATAL: no SunSpec models found")
        return 3

    # --- 3. sample every model's first registers ----------------------------
    print("\n[3] sampling model bodies")
    sample = {}
    for did, info in sorted(models.items()):
        n = min(info["len"], 40)
        try:
            regs = read_registers(sock, info["start"], n)
        except Exception as exc:  # noqa: BLE001
            print("   DID %-5d %-14s read ERR %s" % (did, info["name"], exc))
            continue
        sample[did] = regs
        names = []
        if did == 1:
            names = ["Mn", "Md", "Opt", "Vr", "SN"]
            print("   DID 1 common: %r" % (u16s(regs[:16]),))
        print("   DID %-5d %-14s regs[0:40]: %s" % (did, info["name"], regs[:40]))
        time.sleep(PAUSE)

    # --- 4. vendor-specific battery block ----------------------------------
    print("\n[4] vendor battery block 0xE000.. (57600..)")
    vendor = {}
    for start in range(0xE000, 0xE200 + 1, 0x40):
        if start > 0xE200:
            break
        try:
            regs = read_registers(sock, start, 64)
        except Exception as exc:  # noqa: BLE001
            print("   0x%04X ERR %s" % (start, exc))
            continue
        vendor["0x%04X" % start] = regs
        nz = [(i, v) for i, v in enumerate(regs) if v]
        print("   0x%04X nonzero regs: %s" % (start, nz[:12]))
        time.sleep(PAUSE)

    sock.close()

    out = {
        "host": HOST,
        "port": PORT,
        "unit": UNIT,
        "suns_at_pdu": suns_at,
        "models": models,
        "sample": sample,
        "vendor": vendor,
    }
    with open("sunspec_map.json", "w", encoding="utf-8") as fh:
        json.dump(out, fh, indent=2)
    print("\nmap written to sunspec_map.json")
    print("models:", sorted(models))
    return 0


if __name__ == "__main__":
    sys.exit(main())

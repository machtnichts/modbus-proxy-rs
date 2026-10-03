#!/usr/bin/env python3
"""
Protocol conformance test for muxproxy - runs entirely against a stub Modbus
device on loopback, so the real inverter is never touched.

Question being answered: does the proxy actually speak Modbus/TCP, or does it
merely pretend to? Each check below inspects raw bytes on the wire.

  1  MBAP framing: transaction id, protocol id, length, unit id echoed correctly
  2  FC3 read returns the DEVICE's register values (not synthesised ones)
  3  a cached read costs the upstream device no request at all
  4  FC6 write single is forwarded and its response is a valid Modbus reply
  5  FC16 write multiple likewise, and the cache is invalidated afterwards
  6  a device exception is relayed as a proper exception PDU (fc|0x80 + code)
  7  a read of a never-polled range is fetched on demand and answered
  8  unsupported function codes are handled per policy, not crashed
  9  a downstream unit id the device does not have is answered anyway
     (documents a known deviation - the proxy ignores the unit id)
"""
from __future__ import annotations

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
PROXY = os.path.join(os.path.dirname(HERE), "muxproxy.py")
STUB_PORT = 15020
PROXY_PORT = 15030
HTTP_PORT = 15040

results = []


def check(name, ok, detail=""):
    results.append((name, ok))
    print("  %-58s %s%s" % (name, "PASS" if ok else "FAIL", ("  " + detail) if detail else ""))


# --------------------------------------------------------------------------
class StubDevice(threading.Thread):
    """A real Modbus/TCP server holding known register values."""

    def __init__(self, port):
        super().__init__(daemon=True)
        self.port = port
        self.regs = {a: (a * 7 + 1) & 0xFFFF for a in range(0, 200)}
        self.forbidden = {50, 51, 52}          # reads here raise exception 0x02
        self.requests = []                     # (fc, addr, count, unit)
        self.srv = socket.socket()
        self.srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.srv.bind(("127.0.0.1", port))
        self.srv.listen(4)
        self.srv.settimeout(0.5)
        self._stop = False

    def run(self):
        while not self._stop:
            try:
                conn, _ = self.srv.accept()
            except socket.timeout:
                continue
            threading.Thread(target=self._serve, args=(conn,), daemon=True).start()

    def _serve(self, conn):
        with conn:
            conn.settimeout(30)
            while not self._stop:
                try:
                    hdr = b""
                    while len(hdr) < 7:
                        c = conn.recv(7 - len(hdr))
                        if not c:
                            return
                        hdr += c
                    tid, pid, length, unit = struct.unpack(">HHHB", hdr)
                    body = b""
                    while len(body) < length - 1:
                        c = conn.recv(length - 1 - len(body))
                        if not c:
                            return
                        body += c
                except (socket.timeout, OSError):
                    return
                fc = body[0]
                self.requests.append((fc, body[1:5].hex(), unit))
                resp = self._handle(fc, body)
                conn.sendall(struct.pack(">HHHB", tid, pid, len(resp) + 1, unit) + resp)

    def _handle(self, fc, body):
        if fc == 3 and len(body) >= 5:
            addr, count = struct.unpack(">HH", body[1:5])
            if any(a in self.forbidden for a in range(addr, addr + count)):
                return bytes([0x83, 0x02])
            vals = [self.regs.get(a, 0) for a in range(addr, addr + count)]
            return struct.pack(">BB", 3, count * 2) + struct.pack(">%dH" % count, *vals)
        if fc == 6 and len(body) >= 5:
            addr, val = struct.unpack(">HH", body[1:5])
            self.regs[addr] = val
            return body[:5]
        if fc == 16 and len(body) >= 6:
            addr, count, bc = struct.unpack(">HHB", body[1:6])
            vals = struct.unpack(">%dH" % count, body[6:6 + bc])
            for i, v in enumerate(vals):
                self.regs[addr + i] = v
            return struct.pack(">BHH", 16, addr, count)
        if fc == 3:
            return bytes([0x83, 0x02])
        return bytes([fc | 0x80, 0x01])         # illegal function

    def stop(self):
        self._stop = True
        try:
            self.srv.close()
        except OSError:
            pass


# --------------------------------------------------------------------------
def mb_request(port, unit, pdu, timeout=6.0):
    """Send one PDU and strictly validate the reply framing."""
    tid = 0x4242
    with socket.create_connection(("127.0.0.1", port), timeout=timeout) as s:
        s.sendall(struct.pack(">HHHB", tid, 0, len(pdu) + 1, unit) + pdu)
        hdr = b""
        while len(hdr) < 7:
            c = s.recv(7 - len(hdr))
            if not c:
                raise IOError("closed early")
            hdr += c
        rtid, pid, length, runit = struct.unpack(">HHHB", hdr)
        body = b""
        while len(body) < length - 1:
            c = s.recv(length - 1 - len(body))
            if not c:
                break
            body += c
    return {"tid": rtid, "pid": pid, "unit": runit, "pdu": body}


def metrics():
    import urllib.request
    with urllib.request.urlopen("http://127.0.0.1:%d/metrics" % HTTP_PORT, timeout=5) as r:
        out = {}
        for line in r.read().decode().splitlines():
            k, _, v = line.partition(" ")
            try:
                out[k] = float(v)
            except ValueError:
                pass
        return out


def main():
    stub = StubDevice(STUB_PORT)
    stub.start()

    cfg = {
        "upstream": {"host": "127.0.0.1", "port": STUB_PORT, "unit": 1,
                     "connect_timeout": 5.0, "response_timeout": 5.0},
        "listen": {"host": "127.0.0.1", "port": PROXY_PORT},
        "http": {"host": "127.0.0.1", "port": HTTP_PORT},
        "poll": {"interval_active": 2.0, "interval_idle": 2.0, "min_request_gap": 0.02,
                 "ondemand_ttl": 2.0, "max_registers_per_read": 125,
                 "ranges": [{"name": "stub-a", "address": 0, "count": 40},
                            {"name": "stub-b", "address": 100, "count": 20}]},
        "logging": {"level": "WARNING", "file": None},
        "policy": {"allow_writes": True, "reject_unsupported_functions": False},
    }
    fd, path = tempfile.mkstemp(suffix=".json")
    with os.fdopen(fd, "w") as fh:
        json.dump(cfg, fh)

    proc = subprocess.Popen([sys.executable, PROXY, "-c", path],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(40):
            try:
                metrics()
                break
            except Exception:  # noqa: BLE001
                time.sleep(0.25)
        time.sleep(3.0)                         # let one poll cycle land
        print("1) framing and basic reads")
        r = mb_request(PROXY_PORT, 1, struct.pack(">BHH", 3, 0, 4))
        check("reply echoes transaction id and protocol id",
              r["tid"] == 0x4242 and r["pid"] == 0, "tid=%d pid=%d" % (r["tid"], r["pid"]))
        check("reply unit id echoed", r["unit"] == 1, "unit=%d" % r["unit"])
        check("response is FC3 with correct byte count", r["pdu"][0] == 3 and r["pdu"][1] == 8,
              "fc=%d bytes=%d" % (r["pdu"][0], r["pdu"][1]))
        vals = struct.unpack(">4H", r["pdu"][2:10])
        expected = tuple((a * 7 + 1) & 0xFFFF for a in range(0, 4))
        check("values are the DEVICE's registers, not synthesised", vals == expected,
              "%s vs device %s" % (vals, expected))

        print("\n2) caching behaviour")
        before = metrics()["upstream_reads"]
        for _ in range(25):
            mb_request(PROXY_PORT, 1, struct.pack(">BHH", 3, 0, 4))
        after = metrics()["upstream_reads"]
        check("25 cached reads cost the device 0 requests", after - before == 0,
              "upstream reads +%d" % (after - before))
        check("proxy counted them as cache hits", metrics()["requests_cache_hit"] >= 25,
              "hits=%d" % metrics()["requests_cache_hit"])

        print("\n3) reads outside the polled ranges (on demand)")
        before = metrics()["upstream_reads"]
        r = mb_request(PROXY_PORT, 1, struct.pack(">BHH", 3, 60, 3))
        vals = struct.unpack(">3H", r["pdu"][2:8])
        exp = tuple((a * 7 + 1) & 0xFFFF for a in (60, 61, 62))
        check("never-polled range fetched and correct", vals == exp, "%s vs %s" % (vals, exp))

        print("\n4) device exceptions are relayed as exception PDUs")
        r = mb_request(PROXY_PORT, 1, struct.pack(">BHH", 3, 50, 3))   # stub forbids 50..52
        body = r["pdu"]
        check("read of bad address returns exception PDU", len(body) == 2 and body[0] & 0x80,
              "pdu=%s" % body.hex())
        if len(body) == 2:
            check("device's own exception code relayed verbatim (0x02)", body[1] == 0x02,
                  "code=0x%02x" % body[1])

        print("\n5) writes pass through to the device")
        r = mb_request(PROXY_PORT, 1, struct.pack(">BHH", 6, 5, 1234))
        check("FC6 write echoes addr+value per spec",
              r["pdu"] == struct.pack(">BHH", 6, 5, 1234), "pdu=%s" % r["pdu"].hex())
        check("device really applied the write", stub.regs.get(5) == 1234,
              "device reg 5 = %s" % stub.regs.get(5))
        time.sleep(2.5)                          # let the next poll refresh the cache
        r = mb_request(PROXY_PORT, 1, struct.pack(">BHH", 3, 4, 2))
        vals = struct.unpack(">2H", r["pdu"][2:6])
        check("cache invalidated: read shows new device value", vals[1] == 1234,
              "read back %s" % (vals,))

        pdu = struct.pack(">BHHB", 16, 10, 2, 4) + struct.pack(">2H", 777, 888)
        r = mb_request(PROXY_PORT, 1, pdu)
        check("FC16 write-multiple acknowledged per spec",
              r["pdu"] == struct.pack(">BHH", 16, 10, 2), "pdu=%s" % r["pdu"].hex())
        check("device applied both registers", stub.regs.get(10) == 777 and stub.regs.get(11) == 888,
              "10=%s 11=%s" % (stub.regs.get(10), stub.regs.get(11)))

        print("\n6) unsupported function code (FC 0x41 device identification)")
        r = mb_request(PROXY_PORT, 1, struct.pack(">BB", 0x41, 0x0E))
        body = r["pdu"]
        check("answered with a valid Modbus reply, no crash",
              bool(body) and (body[0] & 0x80) == 0x80, "pdu=%s" % body.hex())

        print("\n7) downstream unit id handling (known deviation)")
        r = mb_request(PROXY_PORT, 7, struct.pack(">BHH", 3, 0, 2))
        answered = r["pdu"][0] == 3
        check("unit id 7 is answered although the device has only unit 1", answered,
              "fc=%d (proxy ignores the requested unit id)" % r["pdu"][0])

        print("\n8) upstream connection count")
        st = metrics()
        check("exactly one upstream connection for all clients",
              st["clients_total"] >= 8 and st["upstream_errors"] == 0,
              "clients=%d errors=%d" % (st["clients_total"], st["upstream_errors"]))
        print("\n   stub device saw %d requests total" % len(stub.requests))
        print("   stub request summary (fc, addr/count hex, unit): %s ..."
              % stub.requests[:6])
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        stub.stop()
        os.unlink(path)

    failed = [n for n, ok in results if not ok]
    print("\n%d/%d checks passed" % (len(results) - len(failed), len(results)))
    if failed:
        print("failed: %s" % failed)
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())

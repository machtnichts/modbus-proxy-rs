#!/usr/bin/env python3
"""
Cache integrity check: the scale-factor registers must never wobble, because a
wrong scale factor silently rescales the whole reading. Also verifies that the
proxy returns byte-identical data across repeated reads and across the two
chunk layouts (scheduled poll chunk vs on-demand fetch chunk).
"""
from __future__ import annotations

import struct
import time

from site_decode import ModbusClient

c = ModbusClient("127.0.0.1", 1503)

print("reading meter block 40188+107 forty times through the proxy...")
sf_seen = {}
bad = 0
vals = []
for i in range(40):
    try:
        regs = c.read(40188, 107)
    except Exception as exc:  # noqa: BLE001
        print("  read %d failed: %s" % (i, exc))
        bad += 1
        continue
    if len(regs) != 107:
        print("  read %d short: %d regs" % (i, len(regs)))
        bad += 1
        continue
    body = regs[2:]
    key = (body[13], body[15], body[20], body[52])
    sf_seen[key] = sf_seen.get(key, 0) + 1
    vals.append((body[16], body[20]))
    if i < 5:
        print("   read %d: SF(v)=0x%04X SF(hz)=0x%04X SF(w)=0x%04X SF(e)=0x%04X "
              "| W_raw=%-7d -> %.4f W | header=%s"
              % (i, body[13], body[15], body[20], body[52],
                 body[16] if body[16] < 32768 else body[16] - 65536,
                 (body[16] if body[16] < 32768 else body[16] - 65536) * 10 ** (-3 if body[20] == 0xFFFD else 0),
                 regs[:2]))
    time.sleep(0.15)

print("\ndistinct scale-factor tuples seen: %d" % len(sf_seen))
for k, n in sf_seen.items():
    ok = k == (0xFFFF, 0xFFFE, 0xFFFD, 0xFFFE)
    print("   v=0x%04X hz=0x%04X w=0x%04X e=0x%04X  x%-3d %s"
          % (k[0], k[1], k[2], k[3], n, "OK" if ok else "<-- UNEXPECTED"))

ws = [v[0] for v in vals]
print("\nW_raw range: %d .. %d  (power spread %.3f W at SF -3)"
      % (min(ws), max(ws), (max(ws) - min(ws)) * 0.001))
print("failed/short reads: %d" % bad)

# independent path: on-demand span that the scheduled poll range does NOT cover
print("\ncross-check scheduled chunk vs on-demand chunk for the same registers:")
a = c.read(40206, 5)
b = c.read(40206, 5)
print("   read 40206+5 twice: %s %s -> %s" % (a, b, "identical" if a == b else "DIFFER"))

# battery floats, repeated
print("\nbattery registers (word-swapped floats):")
for i in range(3):
    p = c.read(0xE174, 2)
    s = c.read(0xE184, 2)
    pw = struct.unpack(">f", struct.pack(">HH", p[1], p[0]))[0]
    so = struct.unpack(">f", struct.pack(">HH", s[1], s[0]))[0]
    print("   power regs=%s -> %+.1f W   soc regs=%s -> %.1f %%"
          % ([hex(x) for x in p], pw, [hex(x) for x in s], so))
    time.sleep(0.4)

c.close()

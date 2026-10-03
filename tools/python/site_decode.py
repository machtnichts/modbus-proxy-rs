#!/usr/bin/env python3
"""
Authoritative SolarEdge site decoder.

Register offsets come from SolarEdge's "SunSpec Logging" Technical Note v3.2
(June 2025), "Meter 2" mapping, shifted to this device's meter block:

    meter block header (DID/length) @ base-0 PDU 40188
    meter data                       @ base-0 PDU 40190   (105 registers)
    meter 1 field offsets = doc's Meter 2 offsets - 174

Field map used here (data-relative index):
    0  M_AC_Current          5  M_AC_Voltage_LN     16 M_AC_Power
    1  M_AC_Current_A        6  M_AC_Voltage_AN     17 M_AC_Power_A
    2  M_AC_Current_B        7  M_AC_Voltage_BN     18 M_AC_Power_B
    3  M_AC_Current_C        8  M_AC_Voltage_CN     19 M_AC_Power_C
    4  M_AC_Current_SF       9  M_AC_Voltage_LL     20 M_AC_Power_SF
    10/11/12 AB/BC/CA       13 M_AC_Voltage_SF    14 M_AC_Freq  15 M_AC_Freq_SF
    36 M_Exported  (acc32)  44 M_Imported (acc32)  52 M_Energy_W_SF
    53 M_Energy_VA_SF ...

Inverter (model 101, header @ PDU 40069, data 40071): DCW at data index 29
with scale factor at 30.

Battery (SolarEdge vendor registers, word-swapped float32):
    0xE174  instantaneous power  (+ = charging)
    0xE184  state of energy / SOC
    0xE176  lifetime export (discharged) counter
    0xE17A  lifetime import (charged) counter

Sign convention: this meter reports export as positive. The charging app (and common energy
accounting) treat import as positive, so the sign is flipped for downstream use.
"""
from __future__ import annotations

import json
import socket
import struct
import threading
import time
from typing import Dict, List, Optional

# --- fixed addressing -------------------------------------------------------
SUNSPEC_METER_HEADER = 40188
SUNSPEC_METER_DATA = 40190
SUNSPEC_METER_LEN = 105
SUNSPEC_INVERTER_HEADER = 40069
SUNSPEC_INVERTER_DATA = 40071
SUNSPEC_INVERTER_LEN = 50

V_BAT_POWER = 0xE174
V_BAT_SOC = 0xE184
V_BAT_EXPORT = 0xE176
V_BAT_IMPORT = 0xE17A

_tid_lock = threading.Lock()
_tid = [0]


class ModbusClient:
    """Minimal Modbus/TCP client (FC3) against the proxy or a device."""

    def __init__(self, host: str = "127.0.0.1", port: int = 1503, unit: int = 1,
                 timeout: float = 6.0, retries: int = 3):
        self.host, self.port, self.unit = host, port, unit
        self.timeout, self.retries = timeout, retries
        self._sock: Optional[socket.socket] = None
        self._lock = threading.Lock()
        self.stats = {"reads": 0, "errors": 0, "reconnects": 0}

    def _connect(self) -> None:
        if self._sock is not None:
            return
        self._sock = socket.create_connection((self.host, self.port), timeout=self.timeout)
        self._sock.settimeout(self.timeout)

    def close(self) -> None:
        if self._sock:
            try:
                self._sock.close()
            except Exception:  # noqa: BLE001
                pass
        self._sock = None

    def read(self, address: int, count: int, fc: int = 3) -> List[int]:
        with self._lock:
            last: Optional[Exception] = None
            for _ in range(self.retries):
                try:
                    self._connect()
                    with _tid_lock:
                        _tid[0] = (_tid[0] + 1) & 0xFFFF
                        tid = _tid[0] or 1
                    pdu = struct.pack(">BHH", fc, address, count)
                    assert self._sock is not None
                    self._sock.sendall(struct.pack(">HHHB", tid, 0, len(pdu) + 1, self.unit) + pdu)
                    hdr = b""
                    while len(hdr) < 7:
                        chunk = self._sock.recv(7 - len(hdr))
                        if not chunk:
                            raise IOError("connection closed")
                        hdr += chunk
                    rtid, _pid, length, _uid = struct.unpack(">HHHB", hdr)
                    if rtid != tid:
                        raise IOError("transaction id mismatch")
                    body = b""
                    while len(body) < length - 1:
                        chunk = self._sock.recv(length - 1 - len(body))
                        if not chunk:
                            break
                        body += chunk
                    if not body:
                        raise IOError("empty response")
                    if body[0] & 0x80:
                        raise IOError("modbus exception 0x%02x" % body[1])
                    if body[1] != count * 2:
                        raise IOError("byte count mismatch")
                    self.stats["reads"] += 1
                    return list(struct.unpack(">%dH" % count, body[2:2 + body[1]]))
                except Exception as exc:  # noqa: BLE001
                    last = exc
                    self.stats["errors"] += 1
                    self.close()
                    time.sleep(0.3)
            raise IOError("read(%d,%d) failed: %s" % (address, count, last))


# --- decoding helpers -------------------------------------------------------
def s16(v: int) -> int:
    return v - 65536 if v > 32767 else v


def _sf(v: int) -> int:
    v = s16(v)
    if v == -32768 or abs(v) > 10:
        return 0
    return v


def scaled(raw: int, factor: int) -> float:
    return raw * (10.0 ** factor)


def acc32(hi: int, lo: int) -> int:
    v = (hi << 16) | lo
    return 0 if v >= 0xFFFFFFFE else v


def f32_swapped(hi: int, lo: int) -> float:
    """SolarEdge stores floats with the 16-bit words swapped."""
    return struct.unpack(">f", struct.pack(">HH", lo, hi))[0]


def u64_swapped(regs: List[int]) -> Optional[int]:
    """64-bit counters: word order reversed relative to big-endian."""
    if any(r == 0xFFFF for r in regs[:2]):
        return None
    words = list(reversed(regs))
    v = 0
    for w in words:
        v = (v << 16) | w
    return v


def _nan(v: float) -> bool:
    return v != v


class SiteReader:
    """Reads a consistent site snapshot through the proxy."""

    def __init__(self, host: str = "127.0.0.1", port: int = 1503, unit: int = 1):
        self.client = ModbusClient(host, port, unit)

    def close(self) -> None:
        self.client.close()

    def snapshot(self) -> Dict:
        out: Dict = {"ts": time.time()}
        meter = self.client.read(SUNSPEC_METER_DATA, SUNSPEC_METER_LEN)
        inv = self.client.read(SUNSPEC_INVERTER_DATA, SUNSPEC_INVERTER_LEN)
        bat = self.client.read(V_BAT_POWER, 2)
        soc = self.client.read(V_BAT_SOC, 2)

        a_sf, v_sf = _sf(meter[4]), _sf(meter[13])
        w_sf, hz_sf = _sf(meter[20]), _sf(meter[15])
        e_sf = _sf(meter[52])

        pv_dc = scaled(s16(inv[29]), _sf(inv[30]))
        bat_power = f32_swapped(*bat)
        bat_soc = f32_swapped(*soc)
        grid_raw = scaled(s16(meter[16]), w_sf)

        out.update({
            "pv_dc_w": None if _nan(pv_dc) else pv_dc,
            "inverter_ac_w": scaled(s16(inv[12]), _sf(inv[13])),
            "inverter_dc_v": scaled(s16(inv[27]), _sf(inv[28])),
            "inverter_dc_a": scaled(s16(inv[25]), _sf(inv[26])),
            "inverter_status": inv[36],
            "grid_w_raw": grid_raw,
            "grid_import_w": -grid_raw,          # positive = import from grid
            "grid_phase_w": [scaled(s16(meter[i]), w_sf) for i in (17, 18, 19)],
            "grid_a": [scaled(s16(meter[i]), a_sf) for i in (1, 2, 3)],
            "grid_a_total": scaled(s16(meter[0]), a_sf),
            "grid_v_ln": [scaled(s16(meter[i]), v_sf) for i in (6, 7, 8)],
            "grid_hz": scaled(s16(meter[14]), hz_sf),
            "grid_va_w": scaled(s16(meter[21]), _sf(meter[25])),
            "grid_var": scaled(s16(meter[26]), _sf(meter[30])),
            "battery_power_w": None if _nan(bat_power) else bat_power,
            "battery_soc": None if _nan(bat_soc) else bat_soc,
        })

        exp_kwh = acc32(meter[36], meter[37])
        imp_kwh = acc32(meter[44], meter[45])
        # counters are in 0.01 Wh units; kWh = value * 10^(sf-3)
        out["grid_export_kwh"] = round(exp_kwh * (10.0 ** (e_sf - 3)), 3) if exp_kwh else None
        out["grid_import_kwh"] = round(imp_kwh * (10.0 ** (e_sf - 3)), 3) if imp_kwh else None
        out["energy_sf"] = e_sf

        # derived household numbers, same convention as the app
        pv_total = None
        if out["pv_dc_w"] is not None and out["battery_power_w"] is not None:
            pv_total = out["pv_dc_w"] + out["battery_power_w"]
        out["pv_total_w"] = pv_total
        return out


def main() -> int:
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=1503)
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()
    r = SiteReader(a.host, a.port)
    try:
        snap = r.snapshot()
    finally:
        r.close()
    if a.json:
        print(json.dumps(snap, indent=1, default=str))
    else:
        for k, v in snap.items():
            print("  %-20s %s" % (k, v))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

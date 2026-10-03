#!/usr/bin/env python3
"""Probe the go-e charger HTTP API (read-only). API v1 and v2 endpoints."""
import json
import urllib.request
import urllib.error

GOE = "http://192.168.178.22"

paths = [
    "/status",
    "/status?filter=car,amp,alw,nrg,psm,eto,wh,frc,model,ver,fwv,sse,trx,acu,amt,ast,alw,pha,tma,cbl,fsp,typ,adi,rtc",
    "/api/status",
    "/api/status?filter=car,amp,alw,nrg,psm,eto,wh,frc,model,ver,fwv,sse,trx",
    "/api/set",
]

for p in paths:
    try:
        with urllib.request.urlopen(GOE + p, timeout=6) as r:
            body = r.read().decode("utf-8", "replace")
        print("=== %-14s HTTP200" % p)
        print(body[:1200])
        try:
            d = json.loads(body)
            print("--- keys:", sorted(d.keys()))
        except Exception:  # noqa: BLE001
            pass
    except urllib.error.HTTPError as e:
        print("=== %-14s HTTP %s: %s" % (p, e.code, e.read()[:200]))
    except Exception as exc:  # noqa: BLE001
        print("=== %-14s ERR %s" % (p, exc))
    print()

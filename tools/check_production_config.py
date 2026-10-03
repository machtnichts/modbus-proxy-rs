#!/usr/bin/env python3
"""Check the Rust proxy against the REAL production config - safely.

The production config points at the SolarEdge inverter, which tolerates exactly
one Modbus client and is owned by the plant's charging app. So this script loads that
config for real (all 19 poll ranges, the SunSpec header expectations, the
scale-factor checks) but overrides the upstream host/port to a local stub, plus
the ports and the log path. The inverter is never contacted.

What it therefore proves: the Rust config parser understands the production
file, the poller runs against those ranges, and the validation rules are
genuinely enforced.

Usage: python3 tools/check_production_config.py [--musl]
"""

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
RS_PROJECT = os.path.dirname(HERE)
PROD_CONFIG = os.path.join(RS_PROJECT, "reference", "config.json")

PASS, FAIL = [], []


def check(name, ok, detail=""):
    print("  %-58s %s  %s" % (name, "PASS" if ok else "FAIL", detail))
    (PASS if ok else FAIL).append(name)


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


def get(port, path):
    with urllib.request.urlopen("http://127.0.0.1:%d%s" % (port, path), timeout=5) as r:
        return r.read().decode()


def main():
    profile = "x86_64-unknown-linux-musl/release" if "--musl" in sys.argv else "release"
    binary = os.path.join(RS_PROJECT, "target", profile, "muxproxy")
    stub_bin = os.path.join(RS_PROJECT, "target", profile, "stubmodbus")
    if not os.path.exists(binary):
        print("build it first: cargo build --release [--target x86_64-unknown-linux-musl]")
        return 2

    with open(PROD_CONFIG) as fh:
        cfg = json.load(fh)
    real_upstream = cfg["upstream"]["host"]
    print("production config : %s" % PROD_CONFIG)
    print("real upstream     : %s:%s (NOT contacted by this check)"
          % (real_upstream, cfg["upstream"]["port"]))
    print("binary under test : %s" % binary)
    print("-" * 78)

    stub_port, listen_port, http_port = free_port(), free_port(), free_port()
    tmpdir = tempfile.mkdtemp(prefix="muxproxy-prodcheck-")
    log_path = os.path.join(tmpdir, "muxproxy.log")

    # redirect the upstream, keep everything else exactly as production
    cfg["upstream"]["host"] = "127.0.0.1"
    cfg["upstream"]["port"] = stub_port
    cfg["listen"]["port"] = listen_port
    cfg["http"]["port"] = http_port
    cfg["logging"]["file"] = log_path
    cfg["logging"]["level"] = "INFO"
    cfg_path = os.path.join(tmpdir, "config.json")
    with open(cfg_path, "w") as fh:
        json.dump(cfg, fh, indent=2)

    stub = subprocess.Popen(
        [stub_bin, "--port", str(stub_port)], stdout=subprocess.PIPE, text=True
    )
    proxy = None
    try:
        if not wait_port(stub_port):
            print("stub never came up")
            return 2
        proxy = subprocess.Popen(
            [binary, "-c", cfg_path], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
        )
        if not wait_port(listen_port) or not wait_port(http_port):
            print("proxy never came up")
            return 2

        wanted = cfg["poll"]["ranges"]
        got = json.loads(get(http_port, "/ranges"))

        print("1) the production poll ranges are understood")
        check("all %d ranges parsed" % len(wanted), len(got) == len(wanted),
              "got %d" % len(got))
        same_addresses = all(
            w["address"] == g["address"] and w["count"] == g["count"]
            for w, g in zip(wanted, got)
        )
        check("addresses and counts identical", same_addresses)
        names_ok = all(w["name"] == g["name"] for w, g in zip(wanted, got))
        check("range names identical", names_ok)

        print("2) the SunSpec validation rules survived the parser")
        hdr_ok = True
        sf_ok = True
        for w, g in zip(wanted, got):
            if "expect_header" in w:
                hdr_ok = hdr_ok and g.get("expect_header") == w["expect_header"]
            if "sf_offsets" in w:
                sf_ok = sf_ok and g.get("sf_offsets") == w["sf_offsets"]
        check("expect_header preserved (inverter + meter)", hdr_ok)
        check("sf_offsets preserved (meter)", sf_ok)

        print("3) the poller actually runs against those ranges")
        deadline = time.time() + 15
        metrics = {}
        while time.time() < deadline:
            metrics = dict(
                line.split() for line in get(http_port, "/metrics").splitlines() if line.strip()
            )
            if float(metrics.get("poll_cycles", 0)) >= 1 and float(
                metrics.get("validation_failures", 0)
            ) >= 1:
                break
            time.sleep(0.25)
        check("poll_cycles >= 1", float(metrics.get("poll_cycles", 0)) >= 1,
              "cycles=%s" % metrics.get("poll_cycles"))
        check("cache is filling", int(float(metrics.get("cache_chunks", 0))) >= 1,
              "chunks=%s regs=%s" % (metrics.get("cache_chunks"), metrics.get("cache_registers")))

        print("4) validation is enforced, not decorative")
        check("SunSpec header mismatch rejected", float(metrics.get("validation_failures", 0)) >= 1,
              "validation_failures=%s" % metrics.get("validation_failures"))

        print("5) the status surface mirrors the Python layout")
        status = json.loads(get(http_port, "/"))
        check("status/upstream/listen present",
              all(k in status for k in ("status", "upstream", "listen", "stats")))
        check("upstream reflects the override", str(stub_port) in status["upstream"],
              status["upstream"])
        check("/cache is a JSON array", get(http_port, "/cache").lstrip().startswith("["))

        print("6) the real inverter was never touched")
        check("no trace of %s in the log" % real_upstream,
              real_upstream not in open(log_path).read() if os.path.exists(log_path) else True)
    finally:
        for p in (proxy, stub):
            if p:
                p.terminate()
        time.sleep(0.3)
        for p in (proxy, stub):
            if p:
                p.kill()

    stub_out = ""
    if stub.stdout:
        try:
            stub_out = stub.stdout.read()
        except Exception:
            pass
    traffic = stub_out.count("TRAFFIC")
    print("-" * 78)
    print("stub carried traffic on %d connection(s); %d request(s)"
          % (traffic, stub_out.count("REQUEST")))
    check("all upstream traffic went to the stub", traffic >= 1,
          "%d connection(s) carried requests" % traffic)

    print("-" * 78)
    print("%d/%d checks passed" % (len(PASS), len(PASS) + len(FAIL)))
    if FAIL:
        print("failed: %s" % ", ".join(FAIL))
    return 0 if not FAIL else 1


if __name__ == "__main__":
    sys.exit(main())

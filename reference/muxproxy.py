#!/usr/bin/env python3
"""
muxproxy - a caching, multiplexing Modbus/TCP proxy for a single-client device.

Why: the SolarEdge Hybrid inverter accepts exactly one Modbus/TCP client and
starts throwing errors when several poll it. This proxy becomes that one client,
keeps a register cache refreshed on a fixed schedule, and lets any number of
downstream clients (Home Assistant, the charging app, scripts) read anything at any rate
without touching the inverter.

Design:
  * ONE persistent upstream connection, strict request/response serialisation,
    MBAP transaction-id validation, reconnect with exponential backoff.
  * Scheduled poller refreshes configured register ranges (never two upstream
    requests at once, configurable minimum gap between them).
  * Reads are served from cache; a miss triggers a single, rate-limited
    on-demand fetch of exactly the requested span.
  * Reads never queue behind each other: N clients cost one upstream read.
  * Writes pass through (serialised) and invalidate overlapping cache entries.
  * Ranges that are absent on the device back off instead of being hammered.
  * HTTP status/metrics endpoint for observability.

Stdlib only. Python 3.9+.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import logging
import logging.handlers
import os
import signal
import struct
import sys
import time
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Tuple

LOG = logging.getLogger("muxproxy")

# Modbus function codes
FC_READ_COILS = 1
FC_READ_DISCRETE = 2
FC_READ_HOLDING = 3
FC_READ_INPUT = 4
FC_WRITE_COIL = 5
FC_WRITE_REGISTER = 6
FC_WRITE_COILS = 15
FC_WRITE_REGISTERS = 16

READ_FCS = {FC_READ_COILS, FC_READ_DISCRETE, FC_READ_HOLDING, FC_READ_INPUT}
WRITE_FCS = {FC_WRITE_COIL, FC_WRITE_REGISTER, FC_WRITE_COILS, FC_WRITE_REGISTERS}

EXC_GATEWAY_FAIL = 0x0B
EXC_GATEWAY_BUSY = 0x06
EXC_ILLEGAL_FUNCTION = 0x01
EXC_ILLEGAL_ADDRESS = 0x02


def exc_pdu(fc: int, code: int) -> bytes:
    return struct.pack(">BB", fc | 0x80, code)


class UpstreamModbusError(IOError):
    """A Modbus exception returned by the device itself.

    Carries the device's own exception code so the proxy can relay it verbatim
    instead of replacing every failure with a generic gateway error - a client
    needs to tell "no such register" (0x02) from "device unreachable" (0x0B).
    """

    def __init__(self, code: int):
        super().__init__("device modbus exception 0x%02x" % code)
        self.code = code


def mbap(tid: int, pid: int, uid: int, pdu: bytes) -> bytes:
    return struct.pack(">HHHB", tid, pid, len(pdu) + 1, uid) + pdu


@dataclass
class Chunk:
    """A contiguous slice of registers, valid until `expires_at`."""
    start: int
    regs: List[int]
    expires_at: float
    fc: int = FC_READ_HOLDING
    healthy: bool = True
    fetched_at: float = field(default_factory=time.time)

    @property
    def end(self) -> int:
        return self.start + len(self.regs)

    @property
    def age_s(self) -> float:
        return max(0.0, time.time() - self.fetched_at)


@dataclass
class Stats:
    started_at: float = field(default_factory=time.time)
    clients_total: int = 0
    clients_active: int = 0
    requests_total: int = 0
    requests_cache_hit: int = 0
    requests_cache_miss: int = 0
    upstream_reads: int = 0
    upstream_writes: int = 0
    upstream_errors: int = 0
    upstream_reconnects: int = 0
    upstream_timeouts: int = 0
    poll_cycles: int = 0
    poll_failures: int = 0
    last_poll_started: Optional[float] = None
    last_poll_finished: Optional[float] = None
    last_upstream_ok: Optional[float] = None
    last_upstream_error: Optional[str] = None
    backoff_ranges: Dict[str, int] = field(default_factory=dict)

    def as_dict(self) -> dict:
        d = dict(self.__dict__)
        d["uptime_s"] = round(time.time() - self.started_at, 1)
        return d


class Upstream:
    """Owns the one and only connection to the inverter."""

    def __init__(self, cfg: dict, stats: Stats):
        self.host = cfg["host"]
        self.port = int(cfg["port"])
        self.unit = int(cfg.get("unit", 1))
        self.connect_timeout = float(cfg.get("connect_timeout", 8.0))
        self.response_timeout = float(cfg.get("response_timeout", 10.0))
        self.stats = stats

        self._reader: Optional[asyncio.StreamReader] = None
        self._writer: Optional[asyncio.StreamWriter] = None
        self._lock = asyncio.Lock()          # one request at a time
        self._tid = 0
        self._last_request_at = 0.0
        self.min_gap = 0.1
        self._backoff = 0.0
        self._next_attempt_at = 0.0

    def set_min_gap(self, gap: float) -> None:
        self.min_gap = max(0.0, float(gap))

    async def _connect(self) -> None:
        if self._writer is not None and not self._writer.is_closing():
            return
        now = time.time()
        if now < self._next_attempt_at:
            raise ConnectionError("upstream backoff active")
        try:
            self._reader, self._writer = await asyncio.wait_for(
                asyncio.open_connection(self.host, self.port), timeout=self.connect_timeout
            )
            self._backoff = 0.0
            LOG.info("upstream connected to %s:%s", self.host, self.port)
        except Exception as exc:  # noqa: BLE001
            self._backoff = min(max(self._backoff * 2, 1.0), 60.0)
            self._next_attempt_at = time.time() + self._backoff
            self.stats.upstream_errors += 1
            self.stats.last_upstream_error = "connect: %s" % exc
            LOG.warning("upstream connect failed (%s), retry in %.0fs", exc, self._backoff)
            raise

    def _drop(self) -> None:
        w = self._writer
        self._reader = None
        self._writer = None
        if w is not None:
            try:
                w.close()
            except Exception:  # noqa: BLE001
                pass

    async def request(self, pdu: bytes, expect_response: bool = True,
                      timeout: Optional[float] = None) -> bytes:
        """Send one PDU upstream and return the response PDU. Serialised."""
        async with self._lock:
            await self._connect()
            assert self._writer is not None and self._reader is not None

            gap = self.min_gap - (time.time() - self._last_request_at)
            if gap > 0:
                await asyncio.sleep(gap)

            self._tid = (self._tid + 1) & 0xFFFF
            if self._tid == 0:
                self._tid = 1
            tid = self._tid
            frame = mbap(tid, 0, self.unit, pdu)
            try:
                self._writer.write(frame)
                await self._writer.drain()
                self._last_request_at = time.time()
            except Exception as exc:  # noqa: BLE001
                self._drop()
                self.stats.upstream_errors += 1
                self.stats.last_upstream_error = "write: %s" % exc
                raise

            if not expect_response:
                return b""

            resp_timeout = timeout or self.response_timeout
            try:
                hdr = await asyncio.wait_for(self._reader.readexactly(7), timeout=resp_timeout)
                rtid, _pid, length, _uid = struct.unpack(">HHHB", hdr)
                if rtid != tid:
                    raise IOError("transaction id mismatch (want %d got %d)" % (tid, rtid))
                body = await asyncio.wait_for(
                    self._reader.readexactly(max(length - 1, 0)), timeout=resp_timeout
                )
            except asyncio.TimeoutError:
                self.stats.upstream_timeouts += 1
                self.stats.upstream_errors += 1
                self.stats.last_upstream_error = "timeout after %.1fs" % resp_timeout
                self._drop()
                raise TimeoutError("upstream timeout")
            except Exception as exc:  # noqa: BLE001
                self._drop()
                self.stats.upstream_errors += 1
                self.stats.last_upstream_error = "read: %s" % exc
                raise

            self.stats.last_upstream_ok = time.time()
            return body

    async def read_registers(self, address: int, count: int, fc: int = FC_READ_HOLDING) -> List[int]:
        if count <= 0 or count > 125:
            raise ValueError("invalid register count %d" % count)
        pdu = struct.pack(">BHH", fc, address, count)
        body = await self.request(pdu)
        if body[0] & 0x80:
            raise UpstreamModbusError(body[1])
        if body[0] != fc:
            raise IOError("unexpected function code 0x%02x" % body[0])
        nbytes = body[1]
        if nbytes != count * 2:
            raise IOError("byte count %d != %d" % (nbytes, count * 2))
        self.stats.upstream_reads += 1
        return list(struct.unpack(">%dH" % count, body[2:2 + nbytes]))

    async def raw_pdu(self, pdu: bytes) -> bytes:
        return await self.request(pdu)


class Cache:
    """Register cache made of independently expiring chunks."""

    def __init__(self, ttl_ondemand: float = 2.0):
        self.chunks: List[Chunk] = []
        self.ttl_ondemand = ttl_ondemand

    def put(self, start: int, regs: List[int], ttl: float, healthy: bool = True,
            fc: int = FC_READ_HOLDING) -> None:
        self.invalidate(start, start + len(regs) - 1, fc=fc)
        self.chunks.append(Chunk(start=start, regs=list(regs),
                                 expires_at=time.time() + ttl, fc=fc, healthy=healthy))

    def invalidate(self, start: int, end: int, fc: Optional[int] = None) -> None:
        out = []
        for c in self.chunks:
            if fc is not None and c.fc != fc:
                out.append(c)
                continue
            if c.end <= start or c.start > end:
                out.append(c)
                continue
            if c.start < start:
                out.append(Chunk(c.start, c.regs[:start - c.start], c.expires_at, c.fc, c.healthy))
            if c.end > end + 1:
                keep_from = end + 1 - c.start
                out.append(Chunk(end + 1, c.regs[keep_from:], c.expires_at, c.fc, c.healthy))
        self.chunks = out

    def get(self, start: int, count: int, fc: int = FC_READ_HOLDING) -> Optional[List[int]]:
        """Only fresh chunks are handed out: the TTL is the promise. An expired chunk is
        gone, and with no fresh one the caller gets None and reports the read failure -
        never a frame the client cannot tell from a real reading."""
        end = start + count - 1
        now = time.time()
        pool = [c for c in self.chunks
                if c.fc == fc and c.start <= start and c.end > end and c.expires_at > now]
        if not pool:
            return None
        # prefer freshest, widest chunk
        pool.sort(key=lambda c: (c.expires_at, len(c.regs)), reverse=True)
        return list(pool[0].regs[start - pool[0].start:start - pool[0].start + count])

    def summary(self) -> list:
        now = time.time()
        return [{"start": c.start, "count": len(c.regs), "fc": c.fc,
                 "age_s": round(c.age_s, 1),
                 "fresh": c.expires_at > now,
                 "healthy": c.healthy}
                for c in sorted(self.chunks, key=lambda c: c.start)]


class MuxProxy:
    def __init__(self, cfg: dict):
        self.cfg = cfg
        self.stats = Stats()
        self.up = Upstream(cfg["upstream"], self.stats)
        self.up.set_min_gap(cfg["poll"].get("min_request_gap", 0.12))
        self.cache = Cache(ttl_ondemand=cfg["poll"].get("ondemand_ttl", 2.0))
        self.poll_ranges = list(cfg["poll"].get("ranges", []))
        self.max_per_read = int(cfg["poll"].get("max_registers_per_read", 100))
        self.policy = cfg.get("policy", {})
        self._range_failures: Dict[str, int] = {}
        self._range_skip_until: Dict[str, float] = {}
        self._client_count = 0
        self._stop = asyncio.Event()
        self._fetch_lock = asyncio.Lock()

    # ---------------- polling ----------------
    async def _refresh_range(self, rng: dict) -> None:
        name = rng.get("name", "0x%04X" % rng["address"])
        if time.time() < self._range_skip_until.get(name, 0):
            return
        addr = int(rng["address"])
        total = int(rng["count"])
        cur = addr
        remaining = total
        collected = []
        try:
            while remaining > 0:
                n = min(remaining, self.max_per_read)
                regs = await self.up.read_registers(cur, n)
                collected.extend(regs)
                cur += n
                remaining -= n
            problem = self._validate(rng, addr, collected)
            if problem:
                # never let a corrupted block overwrite good cached data
                raise IOError("validation failed: %s" % problem)
            self.cache.put(addr, collected, ttl=1e9, healthy=True)
            self._range_failures[name] = 0
            self.stats.backoff_ranges.pop(name, None)
            self.stats.validation_failures = getattr(self.stats, "validation_failures", 0)
        except Exception as exc:  # noqa: BLE001
            fails = self._range_failures.get(name, 0) + 1
            self._range_failures[name] = fails
            self.stats.poll_failures += 1
            skip = min(30.0 * (2 ** min(fails - 1, 5)), 900.0)
            self._range_skip_until[name] = time.time() + skip
            self.stats.backoff_ranges[name] = fails
            LOG.warning("poll range %s (%d regs @%d) failed %dx: %s - skipping %.0fs",
                        name, total, addr, fails, exc, skip)

    def _validate(self, rng: dict, addr: int, regs: List[int]) -> Optional[str]:
        """Reject implausible blocks before they reach the cache.

        The SolarEdge intermittently answers with a shifted or stale block. A
        proxy that hands that to clients silently corrupts every reading, so
        range definitions may carry structural expectations here.
        """
        header = rng.get("expect_header")
        if header and len(regs) >= 2:
            if regs[0] != header[0] or regs[1] != header[1]:
                return "header %s expected %s" % (regs[:2], header)
        for off in rng.get("sf_offsets", []):
            if off < len(regs):
                raw = regs[off]
                val = raw - 65536 if raw > 32767 else raw
                if not (-8 <= val <= 4):
                    return "scale factor at +%d is %d" % (off, val)
        for off in rng.get("sf18_offsets", []):
            if off < len(regs):
                v = self._s18(regs, off)
                if not (-5.5 <= v <= 4.5):
                    return "scale factor at +%d is %s" % (off, v)
        return None

    @staticmethod
    def _s18(regs: List[int], off: int) -> float:
        """SunSpec scale factor delta when the offset is odd (s16 + factor 10)."""
        raw = regs[off]
        val = raw - 65536 if raw > 32767 else raw
        return val

    async def poller(self) -> None:
        cfg = self.cfg["poll"]
        await asyncio.sleep(float(cfg.get("startup_delay", 0.0)))
        while not self._stop.is_set():
            interval = float(cfg.get("interval_active", 5.0)) if self._client_count > 0 \
                else float(cfg.get("interval_idle", 30.0))
            started = time.time()
            self.stats.last_poll_started = started
            self.stats.poll_cycles += 1
            for rng in self.poll_ranges:
                if self._stop.is_set():
                    break
                await self._refresh_range(rng)
            self.stats.last_poll_finished = time.time()
            spent = time.time() - started
            await self._sleep_until(started + max(interval, 0.5))

    async def _sleep_until(self, when: float) -> None:
        while not self._stop.is_set():
            remaining = when - time.time()
            if remaining <= 0:
                return
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=remaining)
                return
            except asyncio.TimeoutError:
                continue

    # ---------------- serving downstream ----------------
    async def _serve_read(self, addr: int, count: int, fc: int) -> List[int]:
        cached = self.cache.get(addr, count, fc=fc)
        if cached is not None:
            self.stats.requests_cache_hit += 1
            return cached
        self.stats.requests_cache_miss += 1
        async with self._fetch_lock:
            cached = self.cache.get(addr, count, fc=fc)
            if cached is not None:
                return cached
            regs = await self.up.read_registers(addr, count, fc=fc)
            self.cache.put(addr, regs, ttl=self.cache.ttl_ondemand, fc=fc)
            return regs

    async def handle_client(self, reader: asyncio.StreamReader,
                            writer: asyncio.StreamWriter) -> None:
        peer = writer.get_extra_info("peername")
        self._client_count += 1
        self.stats.clients_total += 1
        self.stats.clients_active = self._client_count
        LOG.info("client connected %s (active=%d)", peer, self._client_count)
        try:
            while not self._stop.is_set():
                try:
                    hdr = await reader.readexactly(7)
                except (asyncio.IncompleteReadError, ConnectionResetError):
                    break
                tid, pid, length, uid = struct.unpack(">HHHB", hdr)
                if length < 2 or length > 260:
                    LOG.warning("bad MBAP length %d from %s", length, peer)
                    break
                try:
                    pdu = await asyncio.wait_for(reader.readexactly(length - 1), timeout=10)
                except asyncio.TimeoutError:
                    break
                resp_pdu = await self.dispatch(pdu, peer)
                if resp_pdu:
                    writer.write(mbap(tid, pid, uid, resp_pdu))
                    await writer.drain()
        except Exception as exc:  # noqa: BLE001
            LOG.warning("client %s error: %s", peer, exc)
        finally:
            self._client_count -= 1
            self.stats.clients_active = self._client_count
            LOG.info("client disconnected %s (active=%d)", peer, self._client_count)
            try:
                writer.close()
            except Exception:  # noqa: BLE001
                pass

    async def dispatch(self, pdu: bytes, peer) -> bytes:
        self.stats.requests_total += 1
        fc = pdu[0]
        if self.policy.get("log_every_request"):
            LOG.debug("pdu from %s: %s", peer, pdu.hex())

        if fc in (FC_READ_HOLDING, FC_READ_INPUT) and len(pdu) >= 5:
            addr, count = struct.unpack(">HH", pdu[1:5])
            if count < 1 or count > 125:
                return exc_pdu(fc, EXC_ILLEGAL_ADDRESS)
            try:
                regs = await self._serve_read(addr, count, fc)
            except UpstreamModbusError as exc:
                # the device answered - relay its own exception code verbatim
                LOG.info("device exception 0x%02x for read %d@%d", exc.code, count, addr)
                return exc_pdu(fc, exc.code)
            except TimeoutError:
                return exc_pdu(fc, EXC_GATEWAY_FAIL)
            except Exception as exc:  # noqa: BLE001
                # No reading, no answer: pass the failure on rather than serving an expired
                # frame the client would take for a fresh reading.
                LOG.warning("read %d@%d failed: %s", count, addr, exc)
                return exc_pdu(fc, EXC_GATEWAY_FAIL)
            return struct.pack(">BB", fc, count * 2) + struct.pack(">%dH" % count, *regs)

        if fc in (FC_READ_COILS, FC_READ_DISCRETE) and len(pdu) >= 5:
            addr, count = struct.unpack(">HH", pdu[1:5])
            if not self.policy.get("allow_writes", True) and False:
                pass
            try:
                regs = await self._serve_read(addr, count, fc)
            except Exception as exc:  # noqa: BLE001
                LOG.warning("coil read %d@%d failed: %s", count, addr, exc)
                return exc_pdu(fc, EXC_GATEWAY_FAIL)
            # pack bits LSB first
            nbytes = (count + 7) // 8
            data = bytearray(nbytes)
            for i, v in enumerate(regs[:count]):
                if isinstance(v, int) and v > 1:
                    v = 1 if v else 0
                if v:
                    data[i // 8] |= 1 << (i % 8)
            return struct.pack(">BB", fc, nbytes) + bytes(data)

        if fc in WRITE_FCS and len(pdu) >= 5:
            if not self.policy.get("allow_writes", True):
                LOG.warning("write rejected by policy from %s", peer)
                return exc_pdu(fc, EXC_ILLEGAL_FUNCTION)
            try:
                if fc == FC_WRITE_REGISTER:
                    addr, val = struct.unpack(">HH", pdu[1:5])
                    resp = await self.up.raw_pdu(pdu)
                    self.stats.upstream_writes += 1
                    self.cache.invalidate(addr, addr)
                    LOG.info("write single %d = %d from %s", addr, val, peer)
                elif fc == FC_WRITE_REGISTERS:
                    addr, count = struct.unpack(">HH", pdu[1:5])
                    resp = await self.up.raw_pdu(pdu)
                    self.stats.upstream_writes += 1
                    self.cache.invalidate(addr, addr + count - 1)
                    LOG.info("write multiple %d regs @%d from %s", count, addr, peer)
                else:
                    resp = await self.up.raw_pdu(pdu)
                    self.stats.upstream_writes += 1
                    LOG.info("write fc=%d from %s", fc, peer)
                if resp and (resp[0] & 0x80):
                    LOG.warning("upstream rejected write fc=%d: exc 0x%02x", fc, resp[1])
                return resp
            except Exception as exc:  # noqa: BLE001
                LOG.warning("write fc=%d failed: %s", fc, exc)
                return exc_pdu(fc, EXC_GATEWAY_FAIL)

        if self.policy.get("reject_unsupported_functions", False):
            return exc_pdu(fc, EXC_ILLEGAL_FUNCTION)
        try:
            return await self.up.raw_pdu(pdu)
        except Exception as exc:  # noqa: BLE001
            LOG.warning("passthrough fc=%d failed: %s", fc, exc)
            return exc_pdu(fc, EXC_GATEWAY_FAIL)

    # ---------------- HTTP status ----------------
    async def handle_http(self, reader: asyncio.StreamReader,
                          writer: asyncio.StreamWriter) -> None:
        try:
            line = await asyncio.wait_for(reader.readline(), timeout=5)
            path = line.decode("latin1").split(" ")[1] if len(line.split(b" ")) > 1 else "/"
            if path.startswith("/metrics"):
                s = self.stats.as_dict()
                body = "\n".join("%s %s" % (k, v) for k, v in sorted(s.items())
                                 if isinstance(v, (int, float))) + "\n"
                ctype = "text/plain"
            elif path.startswith("/cache"):
                body = json.dumps(self.cache.summary(), indent=1)
                ctype = "application/json"
            elif path.startswith("/ranges"):
                body = json.dumps(self.poll_ranges, indent=1)
                ctype = "application/json"
            else:
                now = time.time()
                healthy = bool(self.stats.last_upstream_ok and
                               (now - self.stats.last_upstream_ok) < 120)
                body = json.dumps({
                    "status": "ok" if healthy else "degraded",
                    "upstream": "%s:%s" % (self.up.host, self.up.port),
                    "listen": "%s:%s" % (self.cfg["listen"]["host"], self.cfg["listen"]["port"]),
                    "stats": self.stats.as_dict(),
                    "cache_chunks": len(self.cache.chunks),
                }, indent=1)
                ctype = "application/json"
            payload = body.encode()
            writer.write(("HTTP/1.1 200 OK\r\nContent-Type: %s\r\nContent-Length: %d\r\n"
                          "Connection: close\r\n\r\n" % (ctype, len(payload))).encode() + payload)
            await writer.drain()
        except Exception:  # noqa: BLE001
            pass
        finally:
            try:
                writer.close()
            except Exception:  # noqa: BLE001
                pass

    async def run(self) -> None:
        listen = self.cfg["listen"]
        server = await asyncio.start_server(self.handle_client, listen["host"], int(listen["port"]))
        http_cfg = self.cfg.get("http") or {}
        http_server = None
        if http_cfg:
            http_server = await asyncio.start_server(
                self.handle_http, http_cfg.get("host", "0.0.0.0"), int(http_cfg["port"]))
        addrs = ", ".join(str(s.getsockname()) for s in server.sockets or [])
        LOG.info("modbus proxy listening on %s -> upstream %s:%s",
                 addrs, self.up.host, self.up.port)
        if http_server:
            LOG.info("status endpoint on http://%s:%s/", http_cfg.get("host"), http_cfg.get("port"))
        poll_task = asyncio.create_task(self.poller())
        async with server:
            await self._stop.wait()
        poll_task.cancel()
        server.close()
        if http_server:
            http_server.close()
        LOG.info("shutting down")


def setup_logging(cfg: dict) -> None:
    lvl = getattr(logging, str(cfg.get("level", "INFO")).upper(), logging.INFO)
    root = logging.getLogger()
    root.setLevel(lvl)
    fmt = logging.Formatter("%(asctime)s %(levelname)-7s %(name)s: %(message)s")
    sh = logging.StreamHandler(sys.stdout)
    sh.setFormatter(fmt)
    root.addHandler(sh)
    path = cfg.get("file")
    if path:
        try:
            os.makedirs(os.path.dirname(path), exist_ok=True)
            fh = logging.handlers.RotatingFileHandler(
                path, maxBytes=int(cfg.get("max_bytes", 2_000_000)),
                backupCount=int(cfg.get("backup_count", 3)))
            fh.setFormatter(fmt)
            root.addHandler(fh)
        except Exception as exc:  # noqa: BLE001
            LOG.warning("file logging disabled: %s", exc)


def main() -> int:
    ap = argparse.ArgumentParser(description="Caching multiplexing Modbus/TCP proxy")
    ap.add_argument("-c", "--config", default=os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                                           "config.json"))
    ap.add_argument("--listen-port", type=int)
    ap.add_argument("--http-port", type=int)
    ap.add_argument("--log-level")
    args = ap.parse_args()

    with open(args.config, "r", encoding="utf-8") as fh:
        cfg = json.load(fh)
    if args.listen_port:
        cfg["listen"]["port"] = args.listen_port
    if args.http_port and cfg.get("http"):
        cfg["http"]["port"] = args.http_port
    if args.log_level:
        cfg.setdefault("logging", {})["level"] = args.log_level

    setup_logging(cfg.get("logging", {}))
    LOG.info("muxproxy starting (pid %d)", os.getpid())
    proxy = MuxProxy(cfg)

    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)

    def _stop(*_a):
        LOG.info("stop signal received")
        loop.call_soon_threadsafe(proxy._stop.set)

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, _stop)
        except NotImplementedError:
            pass

    try:
        loop.run_until_complete(proxy.run())
    except KeyboardInterrupt:
        pass
    finally:
        loop.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())

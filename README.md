# muxproxy (Rust)

A Rust reimplementation of [`../modbus-proxy/muxproxy.py`](../modbus-proxy/muxproxy.py):
the caching, multiplexing Modbus/TCP proxy that lets several readers share the
SolarEdge inverter, which accepts **one** Modbus/TCP client at a time.

It is behaviour-compatible on purpose. Same config file, same CLI, same HTTP
status endpoints, same bytes on the wire — verified, not assumed (see
[Verification](#verification)).

The Python implementation stays exactly as it is. This is an alternative, not a
replacement.

## Why

The Python version needs a Python 3 interpreter and, for the charger app next to
it, a virtualenv with third-party packages. That is a second thing that can break
on its own: a system upgrade, a moved interpreter, a rebuilt venv, and the
inverter loses its proxy. This build has nothing underneath it:

- **no dependencies** — `Cargo.toml` has an empty `[dependencies]` section. No
  crates.io, no vendored tree, and `cargo build --offline` works.
- **one self-contained binary** — the static build is linked against nothing at
  all, not even glibc:

  ```
  $ file target/x86_64-unknown-linux-musl/release/muxproxy
  ELF 64-bit LSB pie executable, x86-64, statically linked, 835 KB
  ```

  Copy it to another machine of the same architecture and it runs. Nothing to
  install, nothing to upgrade, no version to match.

The JSON parser, the HTTP server, the logger (with rotating files), the signal
handling and the Modbus framing are all written against the Rust standard
library. The one libc call is `signal()`, for a clean SIGTERM/SIGINT shutdown.

## Build

```sh
cargo build --release --offline                              # glibc build
rustup target add x86_64-unknown-linux-musl                   # once
cargo build --release --offline --target x86_64-unknown-linux-musl   # static build
```

Or just `make`, or `make static`. `make check` runs every verification below.

## Run

```sh
./target/release/muxproxy -c ../modbus-proxy/config.json
./target/release/muxproxy -c config.json --listen-port 1503 --http-port 1504 --log-level INFO
```

The config file is the *same* file the Python version reads, including the
SunSpec `expect_header` and `sf_offsets` validation rules. Pointing it at the
production config works unchanged (checked — see Verification).

Status endpoints, identical to the Python version:

| path | content |
|---|---|
| `/metrics` | counters plus `cache_chunks` / `cache_registers`, `key value` per line |
| `/cache` | JSON array of the cached chunks with age |
| `/ranges` | JSON array of the polled ranges and their validation rules |
| `/` | status (`ok` / `degraded`), upstream, listen address, stats |

## Verification

Everything below was actually run. No test ever connects to the real inverter:
the app tests use a stub device on loopback (`src/bin/stubmodbus.rs`), and the
production-config check redirects the upstream to that stub.

**1. `cargo test` — 38 tests, all passing**

- 27 unit tests: JSON parsing, config parsing, MBAP framing, bit packing,
  signed conversions, cache expiry/stale rules, all three validation rules,
  write policy, timestamp formatting.
- 11 wire-level integration tests (`tests/conformance.rs`) against the stub:
  framing and unit-id passthrough, device values not synthesised, repeat reads
  cost zero upstream requests, polled ranges answered from cache, on-demand
  fetch outside them, device exceptions relayed verbatim, count > 125 refused
  without contacting the device, FC6 write passthrough + cache invalidation,
  FC16 write multiple, writes refused under policy, unsupported functions per
  policy, and six concurrent clients sharing exactly one upstream connection.

**2. The original Python conformance suite, run against the Rust binary — 17/17**

```sh
python3 tools/cross_check_python_suite.py
```

That suite lives in the Python project and was written against the wire protocol,
not against an implementation, which makes it a real oracle. It hardcodes the
path to `muxproxy.py`, so the script generates a copy in a temp directory with
only that command line changed and runs it there — the original file is never
modified. Result: all 17 checks pass, including "exactly one upstream connection
for all clients" and "25 cached reads cost the device 0 requests".

**3. Differential test, Python proxy vs Rust proxy — 14/14 identical**

```sh
python3 tools/differential_test.py
```

Both proxies get their own stub and then receive an identical scripted sequence;
every response PDU is compared byte for byte. The interesting rows are the ones
nobody would think to assert:

```
read holding 0..4                              MATCH  03080000000a0014001e
read forbidden range -> device exception       MATCH  8302
write single reg 5 = 1234                      MATCH  06000504d2
read back 4..2 (cache must be invalidated)     MATCH  0304002804d2
read coils (FC1) unsupported by device         MATCH  810b
read input regs (FC4) unsupported by device    MATCH  8401
device identification (FC 0x41) unsupported    MATCH  c101
read holding, unit id 7                        MATCH  03040000000a
```

**4. Against the real production config — 13/13**

```sh
python3 tools/check_production_config.py        # add --musl for the static build
```

Loads `../modbus-proxy/config.json` as it really is — 19 poll ranges, the SunSpec
header expectations, the scale-factor offsets — with the upstream host/port
redirected to the stub. Checks that all 19 ranges parse identically, that
`expect_header` and `sf_offsets` survive the parser, that the poller runs, that
validation is genuinely enforced (the stub does not return SunSpec data, so the
header checks must fail), and that the real inverter's address appears nowhere in
the log. All upstream traffic is confirmed on one stub connection.

## Deliberate equivalences

These look like bugs and are preserved, because a drop-in replacement that
"fixes" them breaks whoever depended on the old behaviour:

- **The requested unit id is ignored.** A downstream client asking for unit 7
  gets an answer although the device is unit 1. The proxy sends its own configured
  unit upstream and echoes the client's back.
- **FC1/FC2 failures return exception `0x0B`** (gateway target failed to respond),
  while FC4 relays the device's own exception (verified: `810b` vs `8401`).
- **`policy.max_clients` is parsed but not enforced** — the Python version does not
  enforce it either. Present in the production config, unused in both.
- **Writes to an unsupported function are relayed as-is**, so the device's own
  exception PDU reaches the client unchanged (`c101`).

One real difference, in the implementation only: concurrency is threads rather
than asyncio, so there is no event loop and no single-threaded blocking hazard.
The observable behaviour is unchanged, which is what the differential test above
is for.

## Layout

```
src/main.rs        CLI, config load, signal handling, accept loop
src/json.rs        JSON parser + serialiser (no serde)
src/config.rs      typed config, same keys and defaults as the Python version
src/modbus.rs      function codes, exception codes, MBAP framing
src/cache.rs       independently expiring register chunks
src/upstream.rs    the single persistent connection and its backoff
src/proxy.rs       dispatch, range validation, the poller
src/httpd.rs       the status/metrics endpoints
src/logging.rs     stdout + rotating file
src/stats.rs       shared counters, same names as the Python version
src/bin/stubmodbus.rs   stub device used by the tests
tests/conformance.rs    wire-level integration tests
tools/                  verification, differential and cross-check scripts
```

## Deployment

`deploy/muxproxy-rs.service` is a systemd **user** unit. It is installed and
running in front of a Modbus consumer (here the plant's EV charging app,
optionally Home Assistant), which points its SolarEdge meters (grid/pv/battery)
at this proxy instead of at the inverter.

### In front of a Modbus consumer (installed 2026-09-14)

```sh
make install                      # static binary -> bin/muxproxy
cp deploy/muxproxy-rs.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now muxproxy-rs.service
```

`config/muxproxy.json` is the config it runs with. Two values matter:

- **`response_timeout: 1.0`** — a SunSpec client's model scan probes addresses this
  inverter never answers (50000). With a longer upstream timeout the proxy sits on
  such a probe for the whole timeout while the client gives up first, and the client
  reports `i/o timeout` and then `not a SunSpec device`. Real reads from this inverter
  take ~50-100 ms, so 1 s is 10x headroom. **The Python reference's `config.json`
  still says 10 s: do not put it in front of a SunSpec client without lowering this.**
- **`ranges: []`** — the proxy issues no requests of its own; the inverter only ever
  sees what the consumer asks for. Add ranges only if you want warm caches, at the cost of
  extra device traffic.

Only one Modbus client may hold the inverter. Two proxies, or a proxy plus a direct
reader, produces `transaction id mismatch` in this proxy's log and errors in the consumer.
Switch all consumers in one step, never partially.

### What a consumer does through it (measured)

- one poll cycle every **30 s** (median 29.99 s over 14 cycles) — the client's own
  default; this proxy imposes no cadence
- ~7 requests per cycle, up to ~26 when it re-walks the SunSpec model list:
  model 101 `40071..40120` (inverter), model 203 `40190..40294` (grid meter),
  vendor battery registers `57344..57733`, vendor meter block `190..294`
- proxy answers in ~100 ms median (p95 161 ms), zero timeouts

### Rollback

```sh
systemctl --user disable --now muxproxy-rs.service
```

Then point the consumer's meter config back at the inverter (`192.168.178.84:1502`)
and restart the consumer. Do it with the consumer stopped and move every meter in one
step: a client that validates a meter by connecting to it cannot save a half-changed
config while the other meters still point elsewhere.

### Note the dependency

The plant's grid/PV/battery readings depend on this proxy being up. If it is down,
the consumer loses the inverter and logs read errors — it retries, so a short gap
self-heals. After a reboot the unit starts via lingering (`loginctl enable-linger
adermake`), but a containerised consumer is not ordered against it, so it may briefly
fail its first reads. Check with:

```sh
systemctl --user status muxproxy-rs.service
curl -s http://127.0.0.1:1504/metrics | grep -E "upstream_errors|upstream_timeouts"
```


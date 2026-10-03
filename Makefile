# muxproxy (Rust) - build and verification
#
# `make`       release build (glibc)
# `make static`  fully static build, no libc dependency at all
# `make check`   every test and verification, in one shot
# `make install` copy the static binary to bin/ (what the systemd unit expects)
# `make baseline` save the INSTALLED binary as the baseline for the next comparison -
#                do this before installing a new build, then `make check` compares
#                the fresh build against the one that was in service

CARGO          ?= cargo
TARGET_TRIPLE  ?= x86_64-unknown-linux-musl
BIN            := target/release/muxproxy
STATIC_BIN     := target/$(TARGET_TRIPLE)/release/muxproxy

.PHONY: all build static test unit check cross differential prodcheck prodcheck-static install baseline clean help

all: build

build:
	$(CARGO) build --release --offline
	@ls -l $(BIN)

static:
	$(CARGO) build --release --offline --target $(TARGET_TRIPLE)
	@file $(STATIC_BIN)

test unit:
	$(CARGO) test --offline

# the original Python suite, pointed at the Rust binary (original file untouched)
cross:
	python3 tools/cross_check_python_suite.py

# one version against the next: the fresh build vs the saved baseline, byte for byte
differential:
	python3 tools/differential_test.py

# the real production config, upstream redirected to a stub
prodcheck:
	python3 tools/check_production_config.py

prodcheck-static:
	python3 tools/check_production_config.py --musl

# The comparison needs something to compare against, and a fresh clone has no binary in
# it (they are not committed). `make check` therefore tolerates a missing baseline with a
# loud note, while an explicit `make differential` fails - a comparison that silently did
# not happen is worse than one that says so.
check: test cross
	@python3 tools/differential_test.py --allow-missing-baseline
	python3 tools/check_production_config.py
	@echo
	@echo "=== all checks passed ==="

install: static
	mkdir -p bin
	cp $(STATIC_BIN) bin/muxproxy
	@echo "installed bin/muxproxy (statically linked) - now: make baseline, BEFORE the next install"

# What was in service is the yardstick for what comes next.
baseline:
	@test -f bin/muxproxy || { echo "nothing installed at bin/muxproxy yet"; exit 2; }
	mkdir -p baseline
	cp bin/muxproxy baseline/muxproxy
	@echo "baseline saved:"; sha256sum baseline/muxproxy

clean:
	$(CARGO) clean

help:
	@sed -n '2,10p' Makefile

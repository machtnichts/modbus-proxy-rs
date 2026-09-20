# muxproxy (Rust) - build and verification
#
# `make`       release build (glibc)
# `make static`  fully static build, no libc dependency at all
# `make check`   every test and verification, in one shot
# `make install` copy the static binary to bin/ (what the systemd unit expects)

CARGO          ?= cargo
TARGET_TRIPLE  ?= x86_64-unknown-linux-musl
BIN            := target/release/muxproxy
STATIC_BIN     := target/$(TARGET_TRIPLE)/release/muxproxy

.PHONY: all build static test unit check cross differential prodcheck prodcheck-static install clean help

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

# Python proxy vs Rust proxy, byte for byte
differential:
	python3 tools/differential_test.py

# the real production config, upstream redirected to a stub
prodcheck:
	python3 tools/check_production_config.py

prodcheck-static:
	python3 tools/check_production_config.py --musl

check: test cross differential prodcheck
	@echo
	@echo "=== all checks passed ==="

install: static
	mkdir -p bin
	cp $(STATIC_BIN) bin/muxproxy
	@echo "installed bin/muxproxy (statically linked)"

clean:
	$(CARGO) clean

help:
	@sed -n '2,10p' Makefile

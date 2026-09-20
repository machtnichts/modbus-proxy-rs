#!/usr/bin/env python3
"""Run the original Python conformance suite against the Rust binary.

Why this exists: the Python suite in ../modbus-proxy/tools/ was written against
the wire protocol, not against an implementation, which makes it a genuine
oracle. It hardcodes the path to muxproxy.py, so this script generates a copy in
a temp directory with only that one command line changed and runs it there. The
original file is never modified - the Python implementation stays as it is.

Usage: python3 tools/cross_check_python_suite.py [--debug]
"""

import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
RS_PROJECT = os.path.dirname(HERE)
PY_PROJECT = os.path.join(os.path.dirname(RS_PROJECT), "modbus-proxy")
ORIGINAL = os.path.join(PY_PROJECT, "tools", "test_protocol_conformance.py")

OLD_SPAWN = '[sys.executable, PROXY, "-c", path]'
NEW_SPAWN = '[BINARY, "-c", path]'


def main() -> int:
    profile = "debug" if "--debug" in sys.argv else "release"
    binary = os.path.join(RS_PROJECT, "target", profile, "muxproxy")

    if not os.path.exists(binary):
        print("build it first: cargo build --release (or --offline)")
        return 2
    if not os.path.exists(ORIGINAL):
        print("cannot find the Python suite at %s" % ORIGINAL)
        return 2

    with open(ORIGINAL) as fh:
        source = fh.read()

    if OLD_SPAWN not in source:
        print("the Python suite changed shape; expected this line:")
        print("   " + OLD_SPAWN)
        return 2

    patched = source.replace(OLD_SPAWN, NEW_SPAWN)
    inject = 'PROXY = %r\nBINARY = %r\n' % (ORIGINAL, binary)
    # keep the module intact, just add the binary path next to PROXY
    marker = 'PROXY_PORT = 15030'
    patched = patched.replace(marker, inject + marker, 1)

    print("oracle : %s" % ORIGINAL)
    print("target : %s" % binary)
    print("-" * 60)

    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "test_protocol_conformance_rust.py")
        with open(path, "w") as fh:
            fh.write(patched)
        proc = subprocess.run([sys.executable, path])
        print("-" * 60)
        print("exit code: %d" % proc.returncode)
        return proc.returncode


if __name__ == "__main__":
    sys.exit(main())

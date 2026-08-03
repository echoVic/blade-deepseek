#!/usr/bin/env bash
# Root-cause probe: proves whether Node's spawn(stdio:"inherit") flips
# O_NONBLOCK onto an inherited fd, AS SEEN BY THE CHILD, on a real TTY.
#
# MUST be run directly in an interactive terminal (a TTY), NOT piped:
#     bash scripts/repro/check-nonblock.sh
#
# Interpreting output (checking fd 1 = stdout):
#   direct   O_NONBLOCK on fd1: 0   <- your shell's tty starts blocking (normal)
#   via-node O_NONBLOCK on fd1: 1   <- Node made the inherited tty non-blocking
#                                      => this is the condition the fix defends against
# If via-node prints 0 too, your Node/libuv build doesn't set it here and the
# resize crash likely won't reproduce on this machine.

set -euo pipefail

if ! [ -t 1 ]; then
  echo "WARNING: stdout is not a TTY. Run this directly in a terminal, not via a pipe." >&2
fi

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

# Python probe: print 1 if fd 1 has O_NONBLOCK set, else 0.
cat > "$workdir/probe.py" <<'PY'
import fcntl, os
flags = fcntl.fcntl(1, fcntl.F_GETFL)
print(1 if flags & os.O_NONBLOCK else 0)
PY

# Child ES module: run the probe with inherited stdio (so it sees the same fd).
cat > "$workdir/child.mjs" <<MJS
import { execFileSync } from "node:child_process";
execFileSync("python3", ["$workdir/probe.py"], { stdio: ["inherit", "inherit", "inherit"] });
MJS

printf 'direct   O_NONBLOCK on fd1: '
python3 "$workdir/probe.py"

printf 'via-node O_NONBLOCK on fd1: '
node "$workdir/child.mjs"

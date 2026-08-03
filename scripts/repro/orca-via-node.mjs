// Reproduces the npm wrapper's launch behavior against the locally-built
// debug binary. Same mechanism as npm/orca/bin/orca.js:78 — spawn with
// stdio:"inherit" — which is what flips O_NONBLOCK onto the inherited tty
// fds and triggers `os error 35` (EAGAIN) during resize redraw storms.
//
// Usage:  node scripts/repro/orca-via-node.mjs [orca args...]
// Run it in a REAL terminal, then drag-resize the window rapidly.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..", "..");
const binary = path.join(repoRoot, "target", "debug", "orca");

if (!existsSync(binary)) {
  console.error(`Missing ${binary}. Build it first: cargo build --bin orca`);
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
child.on("error", (e) => {
  console.error(e);
  process.exit(1);
});
child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 0);
});

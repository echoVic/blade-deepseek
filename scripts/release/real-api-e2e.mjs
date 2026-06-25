#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const cliSentinel = "ORCA_REAL_E2E_OK";
const serverSentinel = "ORCA_SERVER_REAL_OK";

function parseArgs(argv) {
  const args = {
    orcaBin: path.join(repoRoot, "target", "debug", "orca"),
    maxBudget: "0.02",
    timeoutMs: 180000,
    skipBuild: false,
    skipProviderSummary: false,
    skipCli: false,
    skipServer: false,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--orca-bin") {
      args.orcaBin = argv[++index];
    } else if (arg === "--max-budget") {
      args.maxBudget = argv[++index];
    } else if (arg === "--timeout-ms") {
      args.timeoutMs = Number.parseInt(argv[++index], 10);
    } else if (arg === "--skip-build") {
      args.skipBuild = true;
    } else if (arg === "--skip-provider-summary") {
      args.skipProviderSummary = true;
    } else if (arg === "--skip-cli") {
      args.skipCli = true;
    } else if (arg === "--skip-server") {
      args.skipServer = true;
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  if (!args.orcaBin) {
    throw new Error("--orca-bin must not be empty");
  }
  if (!args.maxBudget || Number.isNaN(Number(args.maxBudget)) || Number(args.maxBudget) <= 0) {
    throw new Error("--max-budget must be a positive number");
  }
  if (!Number.isInteger(args.timeoutMs) || args.timeoutMs <= 0) {
    throw new Error("--timeout-ms must be a positive integer");
  }

  return args;
}

function commandLabel(command, args) {
  return [command, ...args].join(" ");
}

function run(command, args, options = {}) {
  try {
    return execFileSync(command, args, {
      cwd: repoRoot,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      timeout: options.timeoutMs,
      ...options,
    });
  } catch (error) {
    const stdout = error.stdout ? `\nstdout:\n${error.stdout}` : "";
    const stderr = error.stderr ? `\nstderr:\n${error.stderr}` : "";
    throw new Error(`Command failed: ${commandLabel(command, args)}${stdout}${stderr}`);
  }
}

function runBuild(args) {
  if (args.skipBuild) {
    console.log("Build skipped");
    return;
  }

  run("cargo", ["build", "--bin", "orca"], { timeoutMs: args.timeoutMs });
  console.log("Build verified");
}

function runProviderSummary(args) {
  if (args.skipProviderSummary) {
    console.log("Provider summary real API e2e skipped");
    return;
  }

  const output = run("cargo", ["run", "-p", "orca-provider", "--example", "summary_render_realapi"], {
    timeoutMs: args.timeoutMs,
  });
  if (!output.includes("ALL TARGETS MET")) {
    throw new Error(`Provider summary real API e2e did not report ALL TARGETS MET:\n${output}`);
  }
  console.log("Provider summary real API e2e verified");
}

function parseJsonLines(output, label) {
  return output
    .split(/\r?\n/)
    .filter((line) => line.trim().length > 0)
    .map((line) => {
      try {
        return JSON.parse(line);
      } catch (error) {
        throw new Error(`Unable to parse ${label} JSONL line: ${error.message}\n${line}`);
      }
    });
}

function assertStatus(events, typeField, expectedType, statusPath, label) {
  const completed = events.find((event) => event[typeField] === expectedType);
  if (!completed) {
    throw new Error(`${label} did not emit ${expectedType}`);
  }

  let status = completed;
  for (const key of statusPath) {
    status = status?.[key];
  }
  if (status !== "success") {
    throw new Error(`${label} completed with unexpected status: ${JSON.stringify(completed)}`);
  }
}

function runCli(args) {
  if (args.skipCli) {
    console.log("CLI real API e2e skipped");
    return;
  }

  const output = run(
    args.orcaBin,
    [
      "exec",
      "--output-format",
      "jsonl",
      "--no-history",
      "--mode",
      "suggest",
      "--max-budget",
      args.maxBudget,
      `Reply with exactly: ${cliSentinel}`,
    ],
    { timeoutMs: args.timeoutMs },
  );
  const events = parseJsonLines(output, "CLI");
  const text = events
    .filter((event) => event.type === "assistant.message.delta")
    .map((event) => event.payload?.text ?? "")
    .join("");

  if (!text.includes(cliSentinel)) {
    throw new Error(`CLI real API e2e missing sentinel ${cliSentinel}:\n${output}`);
  }
  assertStatus(events, "type", "session.completed", ["payload", "status"], "CLI real API e2e");
  console.log(`CLI real API e2e verified: ${cliSentinel}`);
}

function runServer(args) {
  if (args.skipServer) {
    console.log("Server real API e2e skipped");
    return;
  }

  const request = JSON.stringify({
    id: 101,
    op: "submit",
    prompt: `Reply with exactly: ${serverSentinel}`,
  });
  const output = run(args.orcaBin, ["--mode", "server"], {
    input: `${request}\n`,
    stdio: ["pipe", "pipe", "pipe"],
    timeoutMs: args.timeoutMs,
  });
  const events = parseJsonLines(output, "server");
  const text = events
    .filter((event) => event.event === "message_delta")
    .map((event) => event.text ?? "")
    .join("");

  if (!text.includes(serverSentinel)) {
    throw new Error(`Server real API e2e missing sentinel ${serverSentinel}:\n${output}`);
  }
  assertStatus(events, "event", "turn_completed", ["status"], "Server real API e2e");
  console.log(`Server real API e2e verified: ${serverSentinel}`);
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  runBuild(args);
  runProviderSummary(args);
  runCli(args);
  runServer(args);
}

try {
  main();
} catch (error) {
  console.error(error.message);
  process.exit(1);
}

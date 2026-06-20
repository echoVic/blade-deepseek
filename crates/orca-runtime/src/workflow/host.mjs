import { readFile } from "node:fs/promises";
import vm from "node:vm";

const scriptPath = process.argv[2];
const argsJson = process.argv[3] ?? "null";
const workflowArgs = JSON.parse(argsJson);

let callSeq = 0;
let currentPhase = null;

function emit(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

async function agent(prompt, opts = {}) {
  callSeq += 1;
  const callId = `agent-${callSeq}`;
  const callPath = `${currentPhase ?? "root"}:${callSeq}`;
  emit({
    type: "agent_call",
    call_id: callId,
    call_path: callPath,
    phase: currentPhase,
    prompt,
    opts,
  });
  return { callId, prompt, cached: false };
}

async function parallel(items) {
  return Promise.all(items);
}

async function pipeline(items) {
  let previous;
  for (const item of items) {
    previous = typeof item === "function" ? await item(previous) : await item;
  }
  return previous;
}

async function phase(name, body) {
  const prior = currentPhase;
  currentPhase = name;
  emit({ type: "phase_started", name });
  try {
    const result = typeof body === "function" ? await body() : undefined;
    emit({ type: "phase_completed", name });
    return result;
  } finally {
    currentPhase = prior;
  }
}

async function loadWorkflowModule() {
  const source = await readFile(scriptPath, "utf8");
  const transformed = source
    .replace(/\bexport\s+const\s+meta\s*=/, "const meta =")
    .replace(/\bexport\s+default\b/, "__workflow_default__ =");

  const context = vm.createContext({
    args: workflowArgs,
    agent,
    parallel,
    pipeline,
    phase,
  });
  const runner = vm.compileFunction(
    `
      "use strict";
      return (async () => {
        let __workflow_default__ = null;
        ${transformed}
        return { meta, default: __workflow_default__ };
      })();
    `,
    [],
    {
      parsingContext: context,
      filename: scriptPath,
      importModuleDynamically() {
        throw new Error("Dynamic import is not available in workflow scripts");
      },
    },
  );

  return runner();
}

try {
  const namespace = await loadWorkflowModule();
  emit({ type: "workflow_completed", result: namespace.default ?? null });
} catch (error) {
  emit({ type: "workflow_failed", error: error?.stack ?? String(error) });
  process.exitCode = 1;
}

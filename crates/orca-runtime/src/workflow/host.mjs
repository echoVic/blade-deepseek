const scriptPath = process.argv[2];
const argsJson = process.argv[3] ?? "null";
globalThis.args = JSON.parse(argsJson);

let callSeq = 0;
let currentPhase = null;

function emit(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

globalThis.agent = async function agent(prompt, opts = {}) {
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
};

globalThis.parallel = async function parallel(items) {
  return Promise.all(items);
};

globalThis.pipeline = async function pipeline(items) {
  let previous;
  for (const item of items) {
    previous = typeof item === "function" ? await item(previous) : await item;
  }
  return previous;
};

globalThis.phase = async function phase(name, body) {
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
};

try {
  const module = await import(`file://${scriptPath}`);
  emit({ type: "workflow_completed", result: module.default ?? null });
} catch (error) {
  emit({ type: "workflow_failed", error: error?.stack ?? String(error) });
  process.exitCode = 1;
}

import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { dirname } from "node:path";
import vm from "node:vm";

const scriptPath = process.argv[2];
const argsJson = process.argv[3] ?? "null";
const mailboxPath = process.argv[4] ?? null;
const taskListsPath = process.argv[5] ?? null;
const workflowArgs = JSON.parse(argsJson);
const FORBIDDEN_IDENTIFIERS = new Set([
  "process",
  "require",
  "constructor",
  "__proto__",
  "prototype",
  "eval",
  "Function",
  "globalThis",
]);
const FORBIDDEN_COMPUTED_PROPERTY_NAMES = new Set([
  "constructor",
  "__proto__",
  "prototype",
  "getBuiltinModule",
]);
const FORBIDDEN_MODULE_SPECIFIERS = new Set(["node:fs", "child_process"]);
const MODULE_SPECIFIER_CALLEES = new Set(["import", "require", "getBuiltinModule"]);

let callSeq = 0;
let currentPhase = null;
let activeMarkerPhase = null;
let stdinBuffer = "";
const pendingAgentResolvers = new Map();
const mailboxState = loadMailboxState();
let messageSeq = mailboxState.nextSeq;
const messageChannels = new Map(Object.entries(mailboxState.channels));
const taskListState = loadTaskListState();
let taskSeq = taskListState.nextTaskSeq;
const taskLists = new Map(Object.entries(taskListState.lists));
let stdinClosed = false;
let workflowTerminal = false;

process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  stdinBuffer += chunk;
  flushStdinResolvers();
});
process.stdin.on("end", () => {
  stdinClosed = true;
  if (workflowTerminal) {
    pendingAgentResolvers.clear();
    return;
  }
  flushStdinResolvers();
});

function emit(value) {
  writeFileSync(1, `${JSON.stringify(value)}\n`);
}

async function agent(prompt, opts = {}) {
  callSeq += 1;
  const callId = `agent-${callSeq}`;
  const callPath = `${currentPhase ?? "root"}:${callSeq}`;
  const resultPromise = readProtocolMessage(callId);
  emit({
    type: "agent_call",
    call_id: callId,
    call_path: callPath,
    phase: currentPhase,
    prompt,
    opts,
  });

  const message = await resultPromise;
  if (message.type === "agent_result") {
    return message.result;
  }
  if (message.type === "agent_error") {
    throw new Error(message.error ?? `Agent ${callId} failed`);
  }

  throw new Error(`Unexpected workflow host protocol message: ${message.type}`);
}

async function parallel(items) {
  const results = await Promise.allSettled(items);
  const firstError = results.find((r) => r.status === "rejected");
  if (firstError) {
    throw firstError.reason;
  }
  return results.map((r) => r.value);
}

async function pipeline(items) {
  let previous;
  for (const item of items) {
    previous = typeof item === "function" ? await item(previous) : await item;
  }
  return previous;
}

function sendMessage(channel, message, opts = {}) {
  const channelName = normalizeMessageChannel(channel);
  messageSeq += 1;
  const entry = {
    seq: messageSeq,
    channel: channelName,
    from: normalizeMessageSender(opts?.from),
    phase: currentPhase,
    message: cloneMessageValue(message),
  };
  const messages = messageChannels.get(channelName) ?? [];
  messages.push(entry);
  messageChannels.set(channelName, messages);
  persistMailboxState();
  return cloneMessageValue(entry);
}

function readMessages(channel) {
  const channelName = normalizeMessageChannel(channel);
  return (messageChannels.get(channelName) ?? []).map(cloneMessageValue);
}

function clearMessages(channel) {
  const channelName = normalizeMessageChannel(channel);
  const count = (messageChannels.get(channelName) ?? []).length;
  messageChannels.delete(channelName);
  persistMailboxState();
  return count;
}

function createTaskList(name, items = []) {
  const listName = normalizeTaskListName(name);
  if (!Array.isArray(items)) {
    throw new Error("Workflow task list items must be an array");
  }

  const tasks = items.map((item) => {
    taskSeq += 1;
    return {
      id: `workflow-task-${taskSeq}`,
      status: "pending",
      value: cloneMessageValue(item),
      claimedBy: null,
      completedBy: null,
      result: null,
    };
  });
  taskLists.set(listName, tasks);
  persistTaskListState();
  return listTasks(listName);
}

function claimTask(name, opts = {}) {
  const tasks = taskLists.get(normalizeTaskListName(name)) ?? [];
  const task = tasks.find((item) => item.status === "pending");
  if (!task) {
    return null;
  }

  task.status = "running";
  task.claimedBy = normalizeTaskWorker(opts?.by ?? opts?.from);
  persistTaskListState();
  return cloneMessageValue(task);
}

function completeTask(name, taskId, result = null, opts = {}) {
  const task = findTask(name, taskId);
  task.status = "completed";
  task.completedBy = normalizeTaskWorker(opts?.by ?? opts?.from);
  task.result = cloneMessageValue(result);
  persistTaskListState();
  return cloneMessageValue(task);
}

function listTasks(name) {
  return (taskLists.get(normalizeTaskListName(name)) ?? []).map(cloneMessageValue);
}

function normalizeMessageChannel(channel) {
  const channelName = String(channel ?? "").trim();
  if (channelName.length === 0) {
    throw new Error("Workflow message channel must be a non-empty string");
  }
  return channelName;
}

function normalizeMessageSender(from) {
  const sender = String(from ?? currentPhase ?? "workflow").trim();
  return sender.length === 0 ? "workflow" : sender;
}

function normalizeTaskListName(name) {
  const listName = String(name ?? "").trim();
  if (listName.length === 0) {
    throw new Error("Workflow task list name must be a non-empty string");
  }
  return listName;
}

function normalizeTaskWorker(worker) {
  const workerName = String(worker ?? currentPhase ?? "workflow").trim();
  return workerName.length === 0 ? "workflow" : workerName;
}

function findTask(name, taskId) {
  const listName = normalizeTaskListName(name);
  const id = String(taskId ?? "").trim();
  if (id.length === 0) {
    throw new Error("Workflow task id must be a non-empty string");
  }

  const task = (taskLists.get(listName) ?? []).find((item) => item.id === id);
  if (!task) {
    throw new Error(`Workflow task not found: ${id}`);
  }
  return task;
}

function cloneMessageValue(value) {
  if (typeof value === "undefined") {
    return null;
  }
  return JSON.parse(JSON.stringify(value));
}

function loadMailboxState() {
  if (mailboxPath === null || !existsSync(mailboxPath)) {
    return { nextSeq: 0, channels: Object.create(null) };
  }
  const state = JSON.parse(readFileSync(mailboxPath, "utf8"));
  return {
    nextSeq: Number(state?.nextSeq ?? 0),
    channels: state?.channels && typeof state.channels === "object" ? state.channels : Object.create(null),
  };
}

function persistMailboxState() {
  if (mailboxPath === null) {
    return;
  }
  writeJsonAtomic(mailboxPath, {
    nextSeq: messageSeq,
    channels: Object.fromEntries(messageChannels),
  });
}

function loadTaskListState() {
  if (taskListsPath === null || !existsSync(taskListsPath)) {
    return { nextTaskSeq: 0, lists: Object.create(null) };
  }
  const state = JSON.parse(readFileSync(taskListsPath, "utf8"));
  return {
    nextTaskSeq: Number(state?.nextTaskSeq ?? 0),
    lists: state?.lists && typeof state.lists === "object" ? state.lists : Object.create(null),
  };
}

function persistTaskListState() {
  if (taskListsPath === null) {
    return;
  }
  writeJsonAtomic(taskListsPath, {
    nextTaskSeq: taskSeq,
    lists: Object.fromEntries(taskLists),
  });
}

function writeJsonAtomic(path, value) {
  mkdirSync(dirname(path), { recursive: true });
  const tempPath = `${path}.tmp-${process.pid}-${Date.now()}`;
  writeFileSync(tempPath, JSON.stringify(value, null, 2));
  try {
    renameSync(tempPath, path);
  } catch (error) {
    try {
      rmSync(tempPath, { force: true });
    } catch {
      // Ignore cleanup errors; the original rename error is more useful.
    }
    throw error;
  }
}

async function runWorkflowPhases(phaseDefinitions) {
  let previousPhaseResults = [];
  const allPhaseResults = [];

  for (const phaseDefinition of phaseDefinitions) {
    const phaseName = String(phaseDefinition?.name ?? phaseDefinition?.description ?? `phase-${allPhaseResults.length + 1}`);
    const tasks = Array.isArray(phaseDefinition?.tasks) ? phaseDefinition.tasks : [];
    const phaseResults = await phase(
      phaseName,
      async () => runWorkflowTasks(tasks, previousPhaseResults, phaseDefinition),
      phaseDefinition
    );
    previousPhaseResults = phaseResults;
    allPhaseResults.push({ name: phaseName, results: phaseResults });
  }

  return { phases: allPhaseResults };
}

async function runWorkflowTasks(tasks, previousPhaseResults, phaseDefinition) {
  if (tasks.length === 0) {
    return [];
  }

  if (phaseDefinition?.parallel === true) {
    return Promise.all(tasks.map((task) => runWorkflowTask(task, previousPhaseResults)));
  }

  const results = [];
  for (const task of tasks) {
    results.push(await runWorkflowTask(task, previousPhaseResults));
  }
  return results;
}

async function runWorkflowTask(task, previousPhaseResults) {
  if (task?.type && task.type !== "agent") {
    throw new Error(`Unsupported workflow task type: ${task.type}`);
  }
  const prompt = task?.prompt;
  if (typeof prompt !== "string" || prompt.trim().length === 0) {
    throw new Error("Workflow task missing `prompt`");
  }

  const opts = Object.assign(Object.create(null), task);
  delete opts.type;
  delete opts.prompt;

  return agent(enrichTaskPrompt(prompt, previousPhaseResults), opts);
}

function enrichTaskPrompt(prompt, previousPhaseResults) {
  if (!Array.isArray(previousPhaseResults) || previousPhaseResults.length === 0) {
    return prompt;
  }

  return `${prompt}\n\n[Previous phase outputs]\n${JSON.stringify(previousPhaseResults, null, 2)}`;
}

async function phase(name, body, opts = {}) {
  if (typeof body !== "function") {
    if (activeMarkerPhase === name) {
      currentPhase = name;
      return undefined;
    }
    completeActiveMarkerPhase();
    currentPhase = name;
    activeMarkerPhase = name;
    emit({ type: "phase_started", name });
    return undefined;
  }

  const prior = currentPhase;
  currentPhase = name;
  emit({ type: "phase_started", name });
  try {
    const result = typeof body === "function" ? await body() : undefined;
    emit({ type: "phase_completed", name });
    return result;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (opts?.fallback === "continue") {
      emit({ type: "phase_failed", name, error: message, fallback: "continue" });
      return { fallback: "continue", error: message };
    }
    if (opts?.fallback && Object.prototype.hasOwnProperty.call(opts.fallback, "value")) {
      emit({ type: "phase_failed", name, error: message, fallback: "value" });
      return opts.fallback.value;
    }
    if (typeof opts?.fallback === "function") {
      emit({ type: "phase_failed", name, error: message, fallback: "function" });
      return await opts.fallback({ name, error: message });
    }
    throw error;
  } finally {
    currentPhase = prior;
  }
}

function completeActiveMarkerPhase() {
  if (activeMarkerPhase === null) {
    return;
  }
  const name = activeMarkerPhase;
  activeMarkerPhase = null;
  emit({ type: "phase_completed", name });
}

function readProtocolMessage(callId) {
  return new Promise((resolve, reject) => {
    pendingAgentResolvers.set(callId, { resolve, reject });
    flushStdinResolvers();
  });
}

function flushStdinResolvers() {
  while (true) {
    const newlineIndex = stdinBuffer.indexOf("\n");
    if (newlineIndex === -1) {
      if (!stdinClosed) {
        return;
      }

      const trailing = stdinBuffer.trim();
      stdinBuffer = "";
      for (const [callId, pending] of pendingAgentResolvers) {
        if (trailing.length > 0) {
          pending.reject(new Error(`Workflow host protocol ended with partial JSON: ${trailing}`));
        } else {
          pending.reject(new Error(`Workflow host protocol closed before result for ${callId}`));
        }
      }
      pendingAgentResolvers.clear();
      return;
    }

    const line = stdinBuffer.slice(0, newlineIndex).trim();
    stdinBuffer = stdinBuffer.slice(newlineIndex + 1);
    if (line.length === 0) {
      continue;
    }

    try {
      const message = JSON.parse(line);
      const pending = pendingAgentResolvers.get(message.call_id);
      if (!pending) {
        throw new Error(`Workflow host protocol received result for unknown call ${message.call_id}`);
      }
      pendingAgentResolvers.delete(message.call_id);
      pending.resolve(message);
    } catch (error) {
      for (const pending of pendingAgentResolvers.values()) {
        pending.reject(error);
      }
      pendingAgentResolvers.clear();
    }
  }
}

async function loadWorkflowModule() {
  const source = await readFile(scriptPath, "utf8");
  guardWorkflowSource(source);
  const transformed = transformWorkflowSource(source);

  const context = vm.createContext(
    Object.assign(Object.create(null), {
      args: workflowArgs,
      agent,
      parallel,
      pipeline,
      phase,
      sendMessage,
      readMessages,
      clearMessages,
      createTaskList,
      claimTask,
      completeTask,
      listTasks,
    }),
    {
      codeGeneration: {
        strings: false,
        wasm: false,
      },
    },
  );
  const runner = vm.compileFunction(
    `
      "use strict";
      return (async () => {
        let __workflow_default__ = null;
        let __workflow_default_set__ = false;
        ${transformed}
        return {
          meta,
          phases: typeof phases === "undefined" ? null : phases,
          default: __workflow_default__,
          defaultSet: __workflow_default_set__,
        };
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

function transformWorkflowSource(source) {
  const replacements = findWorkflowExportReplacements(source).sort((left, right) => right.start - left.start);
  let transformed = source;

  for (const replacement of replacements) {
    transformed =
      transformed.slice(0, replacement.start) +
      replacement.text +
      transformed.slice(replacement.end);
  }

  return transformed;
}

function findWorkflowExportReplacements(source) {
  const replacements = [];
  scanWorkflowExports(source, 0, null, replacements);
  return replacements;
}

function scanWorkflowExports(source, startIndex, terminator, replacements) {
  let index = startIndex;

  while (index < source.length) {
    const char = source[index];
    const next = source[index + 1];

    if (terminator && char === terminator) {
      return index + 1;
    }

    if (isWhitespace(char)) {
      index += 1;
      continue;
    }

    if (char === "/" && next === "/") {
      index = skipLineComment(source, index + 2);
      continue;
    }

    if (char === "/" && next === "*") {
      index = skipBlockComment(source, index + 2);
      continue;
    }

    if (char === "'" || char === "\"") {
      index = readQuotedString(source, index, char).nextIndex;
      continue;
    }

    if (char === "`") {
      index = scanTemplateLiteralForWorkflowExports(source, index + 1, replacements);
      continue;
    }

    if (isIdentifierStart(char)) {
      const exportMatch = matchWorkflowExport(source, index);
      if (exportMatch) {
        replacements.push(exportMatch);
        index = exportMatch.end;
        continue;
      }

      index = readIdentifierEnd(source, index + 1);
      continue;
    }

    index += 1;
  }

  if (terminator) {
    throw new Error(`Unterminated workflow syntax before ${terminator}`);
  }

  return index;
}

function scanTemplateLiteralForWorkflowExports(source, startIndex, replacements) {
  let index = startIndex;

  while (index < source.length) {
    const char = source[index];
    const next = source[index + 1];

    if (char === "\\") {
      index += 2;
      continue;
    }

    if (char === "`") {
      return index + 1;
    }

    if (char === "$" && next === "{") {
      index = scanWorkflowExports(source, index + 2, "}", replacements);
      continue;
    }

    index += 1;
  }

  throw new Error("Unterminated template literal in workflow script");
}

function matchWorkflowExport(source, startIndex) {
  const exportEnd = readIdentifierEnd(source, startIndex + 1);
  if (source.slice(startIndex, exportEnd) !== "export") {
    return null;
  }

  const firstTokenStart = skipIgnorable(source, exportEnd);
  if (firstTokenStart >= source.length || !isIdentifierStart(source[firstTokenStart])) {
    return null;
  }

  const firstTokenEnd = readIdentifierEnd(source, firstTokenStart + 1);
  const firstToken = source.slice(firstTokenStart, firstTokenEnd);

  if (firstToken === "const") {
    const secondTokenStart = skipIgnorable(source, firstTokenEnd);
    if (secondTokenStart >= source.length || !isIdentifierStart(source[secondTokenStart])) {
      return null;
    }

    const secondTokenEnd = readIdentifierEnd(source, secondTokenStart + 1);
    const secondToken = source.slice(secondTokenStart, secondTokenEnd);
    if (secondToken !== "meta" && secondToken !== "phases" && secondToken !== "args") {
      return null;
    }

    const equalsIndex = skipIgnorable(source, secondTokenEnd);
    if (source[equalsIndex] !== "=") {
      return null;
    }

    if (secondToken === "args") {
      return {
        start: startIndex,
        end: secondTokenEnd,
        text: "const __workflow_args_schema__",
      };
    }

    return {
      start: startIndex,
      end: firstTokenStart,
      text: source.slice(exportEnd, firstTokenStart),
    };
  }

  if (firstToken === "default") {
    return {
      start: startIndex,
      end: firstTokenEnd,
      text: `${source.slice(exportEnd, firstTokenStart)}__workflow_default_set__ = true; __workflow_default__ =`,
    };
  }

  return null;
}

function guardWorkflowSource(source) {
  scanExecutableTokens(source, 0, null, [], []);
}

function scanExecutableTokens(source, startIndex, terminator, callStack, bracketStack) {
  let index = startIndex;
  let pendingCallee = null;
  let lastTokenType = null;

  while (index < source.length) {
    const char = source[index];
    const next = source[index + 1];

    if (terminator && char === terminator) {
      return index + 1;
    }

    if (isWhitespace(char)) {
      index += 1;
      continue;
    }

    if (char === "/" && next === "/") {
      index = skipLineComment(source, index + 2);
      continue;
    }

    if (char === "/" && next === "*") {
      index = skipBlockComment(source, index + 2);
      continue;
    }

    if (char === "'") {
      const stringResult = readQuotedString(source, index, "'");
      checkComputedPropertyName(source, index, stringResult.value, stringResult.nextIndex, bracketStack);
      checkModuleSpecifier(callStack, stringResult.value);
      index = stringResult.nextIndex;
      pendingCallee = null;
      lastTokenType = "string";
      continue;
    }

    if (char === "\"") {
      const stringResult = readQuotedString(source, index, "\"");
      checkComputedPropertyName(source, index, stringResult.value, stringResult.nextIndex, bracketStack);
      checkModuleSpecifier(callStack, stringResult.value);
      index = stringResult.nextIndex;
      pendingCallee = null;
      lastTokenType = "string";
      continue;
    }

    if (char === "`") {
      index = scanTemplateLiteral(source, index + 1, callStack);
      pendingCallee = null;
      lastTokenType = "string";
      continue;
    }

    if (isIdentifierStart(char)) {
      const identEnd = readIdentifierEnd(source, index + 1);
      const ident = source.slice(index, identEnd);
      if (FORBIDDEN_IDENTIFIERS.has(ident)) {
        throw new Error(`Workflow script contains prohibited syntax: ${ident}`);
      }
      if (ident === "import" && nextNonWhitespaceChar(source, identEnd) === "(") {
        throw new Error("Workflow script contains prohibited syntax: import(");
      }
      pendingCallee = ident;
      lastTokenType = "identifier";
      index = identEnd;
      continue;
    }

    if (char === "(") {
      callStack.push({ callee: pendingCallee, argIndex: 0 });
      pendingCallee = null;
      lastTokenType = "open_paren";
      index += 1;
      continue;
    }

    if (char === ")") {
      callStack.pop();
      pendingCallee = null;
      lastTokenType = "close_paren";
      index += 1;
      continue;
    }

    if (char === "[") {
      bracketStack.push({ computedProperty: startsComputedProperty(lastTokenType) });
      pendingCallee = null;
      lastTokenType = "open_bracket";
      index += 1;
      continue;
    }

    if (char === "]") {
      bracketStack.pop();
      pendingCallee = null;
      lastTokenType = "close_bracket";
      index += 1;
      continue;
    }

    if (char === "{") {
      pendingCallee = null;
      lastTokenType = "open_brace";
      index += 1;
      continue;
    }

    if (char === "}") {
      pendingCallee = null;
      lastTokenType = "close_brace";
      index += 1;
      continue;
    }

    if (char === ",") {
      if (callStack.length > 0) {
        callStack[callStack.length - 1].argIndex += 1;
      }
      pendingCallee = null;
      lastTokenType = "comma";
      index += 1;
      continue;
    }

    if (char !== ".") {
      pendingCallee = null;
    }
    lastTokenType = "other";
    index += 1;
  }

  if (terminator) {
    throw new Error(`Unterminated workflow syntax before ${terminator}`);
  }
  return index;
}

function scanTemplateLiteral(source, startIndex, callStack) {
  let index = startIndex;

  while (index < source.length) {
    const char = source[index];
    const next = source[index + 1];

    if (char === "\\") {
      index += 2;
      continue;
    }

    if (char === "`") {
      return index + 1;
    }

    if (char === "$" && next === "{") {
      index = scanExecutableTokens(source, index + 2, "}", [], []);
      continue;
    }

    index += 1;
  }

  throw new Error("Unterminated template literal in workflow script");
}

function readQuotedString(source, startIndex, quote) {
  let index = startIndex + 1;
  let value = "";

  while (index < source.length) {
    const char = source[index];
    if (char === "\\") {
      value += source.slice(index, index + 2);
      index += 2;
      continue;
    }
    if (char === quote) {
      return { value, nextIndex: index + 1 };
    }
    value += char;
    index += 1;
  }

  throw new Error("Unterminated string literal in workflow script");
}

function checkModuleSpecifier(callStack, value) {
  const currentCall = callStack[callStack.length - 1];
  if (
    currentCall &&
    currentCall.argIndex === 0 &&
    MODULE_SPECIFIER_CALLEES.has(currentCall.callee) &&
    FORBIDDEN_MODULE_SPECIFIERS.has(value)
  ) {
    throw new Error(`Workflow script contains prohibited module specifier: ${value}`);
  }
}

function checkComputedPropertyName(source, startIndex, value, nextIndex, bracketStack) {
  const currentBracket = bracketStack[bracketStack.length - 1];
  if (
    currentBracket?.computedProperty &&
    previousNonWhitespaceChar(source, startIndex) === "[" &&
    nextNonWhitespaceChar(source, nextIndex) === "]" &&
    FORBIDDEN_COMPUTED_PROPERTY_NAMES.has(value)
  ) {
    throw new Error(`Workflow script contains prohibited computed property: ${value}`);
  }
}

function skipLineComment(source, startIndex) {
  let index = startIndex;
  while (index < source.length && source[index] !== "\n") {
    index += 1;
  }
  return index;
}

function skipBlockComment(source, startIndex) {
  let index = startIndex;
  while (index < source.length - 1) {
    if (source[index] === "*" && source[index + 1] === "/") {
      return index + 2;
    }
    index += 1;
  }
  throw new Error("Unterminated block comment in workflow script");
}

function readIdentifierEnd(source, startIndex) {
  let index = startIndex;
  while (index < source.length && isIdentifierPart(source[index])) {
    index += 1;
  }
  return index;
}

function previousNonWhitespaceChar(source, startIndex) {
  let index = startIndex - 1;
  while (index >= 0 && isWhitespace(source[index])) {
    index -= 1;
  }
  return source[index] ?? null;
}

function nextNonWhitespaceChar(source, startIndex) {
  let index = startIndex;
  while (index < source.length && isWhitespace(source[index])) {
    index += 1;
  }
  return source[index] ?? null;
}

function skipIgnorable(source, startIndex) {
  let index = startIndex;

  while (index < source.length) {
    const char = source[index];
    const next = source[index + 1];

    if (isWhitespace(char)) {
      index += 1;
      continue;
    }

    if (char === "/" && next === "/") {
      index = skipLineComment(source, index + 2);
      continue;
    }

    if (char === "/" && next === "*") {
      index = skipBlockComment(source, index + 2);
      continue;
    }

    break;
  }

  return index;
}

function startsComputedProperty(lastTokenType) {
  return (
    lastTokenType === "identifier" ||
    lastTokenType === "string" ||
    lastTokenType === "close_paren" ||
    lastTokenType === "close_bracket" ||
    lastTokenType === "close_brace"
  );
}

function isWhitespace(char) {
  return char === " " || char === "\n" || char === "\r" || char === "\t";
}

function isIdentifierStart(char) {
  return (
    (char >= "A" && char <= "Z") ||
    (char >= "a" && char <= "z") ||
    char === "_" ||
    char === "$"
  );
}

function isIdentifierPart(char) {
  return isIdentifierStart(char) || (char >= "0" && char <= "9");
}

try {
  const namespace = await loadWorkflowModule();
  const phaseDefinitions = Array.isArray(namespace.phases)
    ? namespace.phases
    : Array.isArray(namespace.meta?.phases)
      ? namespace.meta.phases
      : null;
  const result = namespace.defaultSet
    ? namespace.default
    : Array.isArray(phaseDefinitions) && phaseDefinitions.some((phaseDefinition) => typeof phaseDefinition === "object")
      ? await runWorkflowPhases(phaseDefinitions)
      : null;
  completeActiveMarkerPhase();
  workflowTerminal = true;
  emit({ type: "workflow_completed", result: result ?? null });
} catch (error) {
  workflowTerminal = true;
  emit({ type: "workflow_failed", error: error?.stack ?? String(error) });
  process.exitCode = 1;
}

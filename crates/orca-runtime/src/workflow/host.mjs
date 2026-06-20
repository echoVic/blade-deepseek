import { readFile } from "node:fs/promises";
import vm from "node:vm";

const scriptPath = process.argv[2];
const argsJson = process.argv[3] ?? "null";
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
const FORBIDDEN_MODULE_SPECIFIERS = new Set(["node:fs", "child_process"]);
const MODULE_SPECIFIER_CALLEES = new Set(["import", "require", "getBuiltinModule"]);

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
  guardWorkflowSource(source);
  const transformed = source
    .replace(/\bexport\s+const\s+meta\s*=/, "const meta =")
    .replace(/\bexport\s+default\b/, "__workflow_default__ =");

  const context = vm.createContext(
    Object.assign(Object.create(null), {
      args: workflowArgs,
      agent,
      parallel,
      pipeline,
      phase,
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

function guardWorkflowSource(source) {
  scanExecutableTokens(source, 0, null, []);
}

function scanExecutableTokens(source, startIndex, terminator, callStack) {
  let index = startIndex;
  let pendingCallee = null;

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
      checkModuleSpecifier(callStack, stringResult.value);
      index = stringResult.nextIndex;
      pendingCallee = null;
      continue;
    }

    if (char === "\"") {
      const stringResult = readQuotedString(source, index, "\"");
      checkModuleSpecifier(callStack, stringResult.value);
      index = stringResult.nextIndex;
      pendingCallee = null;
      continue;
    }

    if (char === "`") {
      index = scanTemplateLiteral(source, index + 1, callStack);
      pendingCallee = null;
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
      index = identEnd;
      continue;
    }

    if (char === "(") {
      callStack.push({ callee: pendingCallee, argIndex: 0 });
      pendingCallee = null;
      index += 1;
      continue;
    }

    if (char === ")") {
      callStack.pop();
      pendingCallee = null;
      index += 1;
      continue;
    }

    if (char === ",") {
      if (callStack.length > 0) {
        callStack[callStack.length - 1].argIndex += 1;
      }
      pendingCallee = null;
      index += 1;
      continue;
    }

    if (char !== ".") {
      pendingCallee = null;
    }
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
      index = scanExecutableTokens(source, index + 2, "}", []);
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

function nextNonWhitespaceChar(source, startIndex) {
  let index = startIndex;
  while (index < source.length && isWhitespace(source[index])) {
    index += 1;
  }
  return source[index] ?? null;
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
  emit({ type: "workflow_completed", result: namespace.default ?? null });
} catch (error) {
  emit({ type: "workflow_failed", error: error?.stack ?? String(error) });
  process.exitCode = 1;
}

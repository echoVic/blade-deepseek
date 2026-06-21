import { useEffect, useRef, useState, type KeyboardEvent } from "react";

const npmCommand = "npm install -g @blade-ai/orca";
const curlCommand = "curl -fsSL https://orcaagent.dev/install.sh | sh";

const links = {
  github: "https://github.com/echoVic/blade-deepseek",
  npm: "https://www.npmjs.com/package/@blade-ai/orca",
  releases: "https://github.com/echoVic/blade-deepseek/releases/latest",
};

const features = [
  {
    title: "DeepSeek-native",
    body: "Built around DeepSeek reasoning, streaming, tool-use semantics, and local CLI workflows.",
  },
  {
    title: "Dynamic workflows",
    body: "Run Orca JavaScript workflows from project or user directories.",
  },
  {
    title: "Subagents",
    body: "Delegate focused tasks to child agent loops while keeping the same workspace context.",
  },
  {
    title: "Approval modes",
    body: "Move from suggested edits to full-auto execution with explicit control over risk.",
  },
  {
    title: "Verification",
    body: "Attach post-run commands such as cargo test so the agent has to face the build.",
  },
  {
    title: "Resumable history",
    body: "Persist local JSONL transcripts, resume sessions, fork runs, search, and compress history.",
  },
];

type InstallMode = "npm" | "curl";
type CopyState = "idle" | "copied" | "failed";

const installTabIds = {
  npm: "install-tab-npm",
  curl: "install-tab-curl",
} as const;

const installPanelId = "install-panel";

function fallbackCopyText(command: string) {
  const textarea = document.createElement("textarea");

  textarea.value = command;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.top = "0";
  textarea.style.left = "0";
  textarea.style.width = "1px";
  textarea.style.height = "1px";
  textarea.style.padding = "0";
  textarea.style.border = "0";
  textarea.style.outline = "0";
  textarea.style.boxShadow = "none";
  textarea.style.background = "transparent";
  textarea.style.opacity = "0";

  document.body.appendChild(textarea);

  try {
    textarea.focus();
    textarea.select();
    textarea.setSelectionRange(0, textarea.value.length);

    return document.execCommand("copy");
  } finally {
    document.body.removeChild(textarea);
  }
}

async function copyCommandText(command: string) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(command);
      return true;
    }
  } catch {
    // Fall through to the legacy clipboard path.
  }

  try {
    return fallbackCopyText(command);
  } catch {
    return false;
  }
}

function App() {
  const [mode, setMode] = useState<InstallMode>("npm");
  const [copyState, setCopyState] = useState<CopyState>("idle");
  const resetTimerRef = useRef<number | null>(null);
  const copyRequestRef = useRef(0);
  const tabRefs = useRef<Record<InstallMode, HTMLButtonElement | null>>({
    npm: null,
    curl: null,
  });
  const command = mode === "npm" ? npmCommand : curlCommand;

  function clearCopyResetTimer() {
    if (resetTimerRef.current !== null) {
      window.clearTimeout(resetTimerRef.current);
      resetTimerRef.current = null;
    }
  }

  function setInstallMode(nextMode: InstallMode) {
    setMode(nextMode);
    copyRequestRef.current += 1;
    setCopyState("idle");
    clearCopyResetTimer();
  }

  useEffect(() => {
    if (copyState === "idle") {
      return;
    }

    clearCopyResetTimer();

    resetTimerRef.current = window.setTimeout(() => {
      setCopyState("idle");
      resetTimerRef.current = null;
    }, 1400);

    return clearCopyResetTimer;
  }, [copyState]);

  async function copyCommand() {
    const requestId = ++copyRequestRef.current;
    const copied = await copyCommandText(command);
    if (requestId !== copyRequestRef.current) {
      return;
    }

    setCopyState(copied ? "copied" : "failed");
  }

  function handleInstallTabKeyDown(
    event: KeyboardEvent<HTMLButtonElement>,
    currentMode: InstallMode,
  ) {
    let nextMode: InstallMode | null = null;

    switch (event.key) {
      case "ArrowLeft":
      case "ArrowUp":
        nextMode = currentMode === "npm" ? "curl" : "npm";
        break;
      case "ArrowRight":
      case "ArrowDown":
        nextMode = currentMode === "npm" ? "curl" : "npm";
        break;
      case "Home":
        nextMode = "npm";
        break;
      case "End":
        nextMode = "curl";
        break;
      default:
        return;
    }

    event.preventDefault();
    setInstallMode(nextMode);
    tabRefs.current[nextMode]?.focus();
  }

  return (
    <main>
      <header className="nav">
        <a className="brand" href="#top" aria-label="Orca home">
          <span className="brand-mark">O</span>
          <span>Orca</span>
        </a>
        <nav aria-label="Main navigation">
          <a href="#install">Install</a>
          <a href="#features">Features</a>
          <a href="#workflow">Workflow</a>
          <a href={links.github}>GitHub</a>
        </nav>
      </header>

      <section className="hero" id="top">
        <div className="hero-copy">
          <p className="eyebrow">Blade AI developer runtime</p>
          <h1>Orca</h1>
          <p className="subtitle">A DeepSeek-native coding agent runtime by Blade.</p>
          <p className="hero-text">
            Run local coding tasks with streaming reasoning, workflow scripts,
            subagents, approvals, verification commands, and resumable history.
          </p>

          <div className="install-card" id="install" aria-label="Install Orca">
            <div className="tabs" role="tablist" aria-label="Install method">
              <button
                id={installTabIds.npm}
                className={mode === "npm" ? "active" : ""}
                onClick={() => setInstallMode("npm")}
                onKeyDown={(event) => handleInstallTabKeyDown(event, "npm")}
                aria-selected={mode === "npm"}
                aria-controls={installPanelId}
                role="tab"
                tabIndex={mode === "npm" ? 0 : -1}
                type="button"
                ref={(element) => {
                  tabRefs.current.npm = element;
                }}
              >
                npm
              </button>
              <button
                id={installTabIds.curl}
                className={mode === "curl" ? "active" : ""}
                onClick={() => setInstallMode("curl")}
                onKeyDown={(event) => handleInstallTabKeyDown(event, "curl")}
                aria-selected={mode === "curl"}
                aria-controls={installPanelId}
                role="tab"
                tabIndex={mode === "curl" ? 0 : -1}
                type="button"
                ref={(element) => {
                  tabRefs.current.curl = element;
                }}
              >
                curl
              </button>
            </div>
            <div
              className="command-row"
              id={installPanelId}
              role="tabpanel"
              aria-labelledby={mode === "npm" ? installTabIds.npm : installTabIds.curl}
              tabIndex={0}
            >
              <code>{command}</code>
              <button type="button" onClick={copyCommand} className="copy">
                {copyState === "copied"
                  ? "Copied"
                  : copyState === "failed"
                    ? "Failed"
                    : "Copy"}
              </button>
            </div>
          </div>

          <div className="actions">
            <a className="primary" href={links.github}>
              GitHub
            </a>
            <a className="secondary" href={links.npm}>
              npm package
            </a>
          </div>
        </div>

        <div className="terminal" aria-label="Orca terminal preview">
          <div className="terminal-bar">
            <span></span>
            <span></span>
            <span></span>
          </div>
          <pre>{`$ orca exec --verifier "cargo test" "fix the failing workflow test"

session.started  model=deepseek-v4-pro
tool.call        grep "workflow"
tool.call        read_file tests/workflow_runtime_contract.rs
assistant        patching runtime state transition
verification     cargo test
session.done     tests passed`}</pre>
        </div>
      </section>

      <section className="features" id="features">
        <div className="section-heading">
          <p className="eyebrow">Runtime surface</p>
          <h2>Everything a local coding agent needs to take a serious pass.</h2>
        </div>
        <div className="feature-grid">
          {features.map((feature) => (
            <article key={feature.title}>
              <h3>{feature.title}</h3>
              <p>{feature.body}</p>
            </article>
          ))}
        </div>
      </section>

      <section className="workflow" id="workflow">
        <div>
          <p className="eyebrow">Workflow scripts</p>
          <h2>Project-scoped automation without leaving the CLI.</h2>
          <p>
            Orca resolves named workflows from `.orca/workflows/` first, then
            user workflows in `~/.orca/workflows/`. Keep repeatable agent routines close to the code
            they operate on.
          </p>
        </div>
        <pre className="code-sample">{`export const meta = {
  name: "audit",
  description: "Audit code",
  phases: ["scan"]
};

$ orca workflow run audit`}</pre>
      </section>

      <section className="install-repeat" aria-label="Install commands">
        <div>
          <p className="eyebrow">Install</p>
          <h2>Use npm, or install the native binary directly.</h2>
        </div>
        <div className="install-list">
          <code>{npmCommand}</code>
          <code>{curlCommand}</code>
          <p>
            Supported platforms: macOS arm64/x64 and Linux arm64/x64.
            Downloads are available on{" "}
            <a href={links.releases}>GitHub Releases</a>.
          </p>
        </div>
      </section>

      <footer>
        <span>Orca by Blade</span>
        <div>
          <a href={links.github}>GitHub</a>
          <a href={links.npm}>npm</a>
          <a href={links.releases}>Releases</a>
        </div>
      </footer>
    </main>
  );
}

export default App;

import { useState } from "react";

const npmCommand = "npm install -g @blade-ai/orca";
const curlCommand =
  "curl -fsSL https://raw.githubusercontent.com/echoVic/blade-deepseek/main/install.sh | sh";

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
    body: "Run Claude Code-style JavaScript workflows from project or user directories.",
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

function App() {
  const [mode, setMode] = useState<InstallMode>("npm");
  const [copied, setCopied] = useState(false);
  const command = mode === "npm" ? npmCommand : curlCommand;

  async function copyCommand() {
    await navigator.clipboard.writeText(command);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1400);
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
                className={mode === "npm" ? "active" : ""}
                onClick={() => setMode("npm")}
                type="button"
              >
                npm
              </button>
              <button
                className={mode === "curl" ? "active" : ""}
                onClick={() => setMode("curl")}
                type="button"
              >
                curl
              </button>
            </div>
            <div className="command-row">
              <code>{command}</code>
              <button type="button" onClick={copyCommand} className="copy">
                {copied ? "Copied" : "Copy"}
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
            Orca resolves named workflows from `.claude/workflows/` first, then
            user workflows. Keep repeatable agent routines close to the code
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

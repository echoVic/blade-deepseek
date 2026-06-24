import {
    useEffect,
    useMemo,
    useRef,
    useState,
    type KeyboardEvent,
    type ReactNode,
} from "react";
import {
    applySeoHead,
    canonicalOrigin,
    detectInitialLocale,
    links,
    localeStorageKey,
    releaseVersion,
    type Locale,
    type SeoEntry
} from "./shared";

const npmCommand = "npm install -g @blade-ai/orca";
const curlCommand = "curl -fsSL https://orcaagent.dev/install.sh | sh";

const canonicalUrl = `${canonicalOrigin}/`;

const seoCopy = {
  en: {
    title: "Orca - DeepSeek-native terminal coding agent",
    description:
      "Orca is a DeepSeek-native local coding agent for terminal workflows, spec-driven tools, Markdown skills, TUI user input, persistent goals, resumable history, and verifier-gated automation.",
    ogTitle: "Orca - DeepSeek-native terminal coding agent",
    ogDescription:
      "Run DeepSeek-native coding agent workflows locally with spec-driven tools, Markdown skills, TUI user input, persistent goals, resumable history, and verifier-gated automation.",
    imageAlt: "Orca terminal coding agent product preview",
    locale: "en_US",
  },
  zh: {
    title: "Orca - DeepSeek 原生终端代码智能体",
    description:
      "Orca 是 DeepSeek 原生的本地终端代码智能体，支持规格驱动工具、Markdown skills、TUI 用户输入、持久 goal、可恢复历史、工作流和 verifier 校验自动化。",
    ogTitle: "Orca - DeepSeek 原生终端代码智能体",
    ogDescription:
      "在本地终端运行 DeepSeek 原生代码智能体工作流，覆盖规格驱动工具、Markdown skills、TUI 用户输入、持久 goal、可恢复历史和 verifier 校验自动化。",
    imageAlt: "Orca 终端代码智能体产品预览",
    locale: "zh_CN",
  },
} as const;

const copy = {
  en: {
    langName: "English",
    aria: {
      home: "Orca home",
      nav: "Main navigation",
      language: "Language",
      tui: "Orca TUI preview",
      commands: "Command examples",
      install: "Install commands",
    },
    nav: {
      features: "Features",
      capabilities: "Capabilities",
      workflow: "Workflow",
      install: "Install",
      changelog: "Changelog",
      github: "GitHub",
    },
    hero: {
      pill: "v0.1.31 · Rust-native",
      titlePrefix: "A",
      titleHighlight: "DeepSeek-native",
      titleSuffix: "coding agent, in your terminal.",
      subtitle:
        "Orca is a local terminal coding agent built in Rust around DeepSeek's reasoning and tool-use semantics. Multi-turn agent loop, spec-driven tools, Markdown skills, TUI clarification prompts, SSE streaming, and 1M context — one command, hand it the task.",
      primary: "Get started",
      secondary: "View on GitHub",
      meta: {
        context: "context window",
        turns: "max turns",
        tools: "tool surfaces",
        platforms: "platforms",
        cache: "prefix-cache hit",
      },
    },
    featuresEyebrow: "What you'll notice",
    featuresTitle: "Not a wrapper — built for DeepSeek semantics.",
    features: [
      {
        title: "DeepSeek-native",
        body: "Built around DeepSeek reasoning, SSE streaming, and tool-use semantics. One orca exec hands off the task — no context switch.",
      },
      {
        title: "1M context, self-managed",
        body: "A 1M token window with automatic compaction past the 80% threshold, preserving the system prompt and recent turns. Long tasks keep their context.",
      },
      {
        title: "Persistent goal mode",
        body: "Set a long-running objective with /goal; it auto-continues after each successful turn, survives restarts, and only lets the model complete or block after a goal audit.",
      },
      {
        title: "Approval modes",
        body: "Tool specs declare capabilities; reads run directly, while write, shell, network, and agent actions follow your configured approval policy.",
      },
      {
        title: "Skills and user input",
        body: "Markdown skills can be explicitly injected with $skill ids, and the TUI can answer structured request_user_input questions without ending the turn.",
      },
      {
        title: "Resumable history",
        body: "Local JSONL transcripts support list, search, --resume, --fork, archive, and optional zstd compression.",
      },
    ],
    cacheCard: {
      eyebrow: "Prefix cache",
      title: "Tuned for DeepSeek prefix cache, end-to-end.",
      body: "Nine rounds of real-API tuning. The wire prompt stays append-only at the byte level — system, tools, history, summary baseline — so multi-turn loops, compaction, and resume hold a stable prefix instead of redoing it each turn.",
      stats: [
        { k: "99%", l: "post-compaction main-loop hit (real API)" },
        { k: "92%", l: "non-compacted short-task hit" },
        { k: "0", l: "duplicate remote summary calls (hashed cache)" },
      ],
    },
    capabilitiesEyebrow: "Inside the engine",
    capabilitiesTitle: "Every turn stays in your control.",
    capabilitiesSubtitle:
      "From prompt to tool call to result, Orca exposes the whole agent loop as a readable, verifiable, resumable flow — not a black box.",
    builtInToolsLabel: "Built-in tools",
    capabilities: [
      {
        title: "Sessions & resume",
        body: "Local JSONL transcripts under ~/.orca/sessions/, with --resume to continue and --fork to branch a new run.",
      },
      {
        title: "Verifier loop",
        body: "Pass --verifier \"cargo test\" to gate a run on a real command; pass/fail is reported with exit code 2 on failure.",
      },
      {
        title: "Hooks & custom tools",
        body: "Lifecycle hooks return structured JSON to deny, modify, or inject context; built-ins, MCP tools, and TOML descriptors share one registry.",
      },
      {
        title: "Structured event stream",
        body: "--output-format jsonl emits versioned events from session.started to tool.call.completed for your orchestration layer.",
      },
    ],
    workflowEyebrow: "Command surface",
    workflowTitle: "One set of verbs across the real dev loop.",
    codeTabs: {
      exec: "orca exec",
      goal: "Persistent goal",
      history: "History / resume",
      config: "Config",
    },
    code: {
      execTask: "Hand it a task",
      execRefactor: "Refactor in full-auto",
      execModel: "Pick a model + gate on a verifier",
      execDone: "  ✓ 3 files edited · cargo test passed · exit 0",
      goalSet: "Set a long-running objective; it auto-continues",
      goalEdit: "update + reactivate",
      goalPause: "stop auto-continuation",
      goalResume: "continue when idle",
      goalStored: "  Stored by session id in ~/.orca/goals_1.json — survives restarts.",
      historyBrowse: "Browse, search, and resume transcripts",
      historyStored:
        "  Stored under ~/.orca/sessions/YYYY/MM/DD/; large runs can be zstd-compressed.",
      configMainLoop: "main loop v4-pro, aux tasks v4-flash",
      configPriority: "Priority: env vars > CLI args > config file > defaults",
    },
    specsEyebrow: "Technical specs",
    specsLabel: "Technical specs",
    specs: {
      context: "Context window, auto-compacted past the 80% threshold.",
      platforms: "Native binaries: macOS and Linux, arm64 and x64.",
      tools: "Built-in, MCP, and external tools share one spec-driven registry.",
      rust: "Written in Rust, running as a single local binary.",
    },
    install: {
      eyebrow: "Install",
      title: "Use npm, or install the native binary directly.",
      cardLabel: "Install Orca",
      methodLabel: "Install method",
      copy: "Copy",
      copied: "✓ Copied",
      failed: "Failed",
      platforms:
        "Supported platforms: macOS arm64/x64 and Linux arm64/x64. Downloads are available on",
      releases: "GitHub Releases",
    },
    community: {
      qq: "QQ Group 472309526",
      telegram: "Telegram",
    },
    tui: {
      user: "fix the failing auth test",
      reasoning: "reasoning",
      reason1: "locating the failing case, checking the token-expiry comparison…",
      reason2Prefix: "expiry uses",
      reason2Middle: "; the boundary second is wrongly valid — should be",
      approve: "⚑ approval · edit src/auth/token.rs",
      approved: "approved",
      grepResult: "→ 3 assertions matched",
      readResult: "→ read 86 lines",
      editResult: "→ 1 change written",
      bashResult: "→ running 4 tests…",
      ok: "✓ test auth::token_expiry ... ok · 4 passed",
      done: "✓ done · 1 file changed · cargo test passed · exit 0",
      footerBacktrack: "backtrack",
      footerGoal: "goal",
      footerExit: "exit",
      statusContext: "context",
    },
  },
  zh: {
    langName: "中文",
    aria: {
      home: "Orca 首页",
      nav: "主导航",
      language: "语言",
      tui: "Orca TUI 预览",
      commands: "命令示例",
      install: "安装命令",
    },
    nav: {
      features: "特性",
      capabilities: "能力",
      workflow: "工作流",
      install: "安装",
      changelog: "更新日志",
      github: "GitHub",
    },
    hero: {
      pill: "v0.1.31 · Rust 原生",
      titlePrefix: "面向终端的",
      titleHighlight: "DeepSeek 原生",
      titleSuffix: "代码智能体。",
      subtitle:
        "Orca 是一个用 Rust 构建的本地终端代码智能体，围绕 DeepSeek 的推理与工具调用语义设计。多轮智能体循环、规格驱动工具、Markdown skills、TUI 澄清问题、SSE 流式输出与 1M 上下文，一个命令就能把任务交给它。",
      primary: "开始使用",
      secondary: "查看 GitHub",
      meta: {
        context: "上下文窗口",
        turns: "最大轮次",
        tools: "工具面",
        platforms: "支持平台",
        cache: "前缀缓存命中",
      },
    },
    featuresEyebrow: "你会注意到",
    featuresTitle: "不是包装器，而是为 DeepSeek 语义构建。",
    features: [
      {
        title: "DeepSeek 原生",
        body: "围绕 DeepSeek 推理、SSE 流式输出和工具调用语义构建。一个 orca exec 就能交付任务，不必切换上下文。",
      },
      {
        title: "1M 上下文，自主管理",
        body: "1M token 上下文窗口，超过 80% 阈值后自动压缩，同时保留系统提示词和最近对话。长任务也能持续推进。",
      },
      {
        title: "持久化 goal 模式",
        body: "用 /goal 设置长期目标；每轮成功后自动继续，跨进程重启保留，并要求模型经过 goal 审计后才能完成或阻塞。",
      },
      {
        title: "审批模式",
        body: "工具规格声明能力；读取直接运行，写入、shell、网络和 agent 操作按你的审批策略执行。",
      },
      {
        title: "Skills 与用户输入",
        body: "Markdown skills 可通过 $skill id 显式注入；TUI 能回答结构化 request_user_input 问题，并继续同一轮任务。",
      },
      {
        title: "可恢复历史",
        body: "本地 JSONL 会话支持 list、search、--resume、--fork、archive，以及可选 zstd 压缩。",
      },
    ],
    cacheCard: {
      eyebrow: "前缀缓存",
      title: "为 DeepSeek 前缀缓存做了端到端调优。",
      body: "经过九轮真实 API 验证。Wire 层提示词字节级 append-only —— system、tools、history、summary baseline 全部稳定 —— 多轮循环、压缩与 resume 都复用同一段前缀，而不是每轮重发。",
      stats: [
        { k: "99%", l: "压缩后主链路命中（真实 API）" },
        { k: "92%", l: "非压缩短任务命中" },
        { k: "0", l: "重复 remote summary 调用（哈希缓存）" },
      ],
    },
    capabilitiesEyebrow: "引擎内部",
    capabilitiesTitle: "每一轮都在你的控制之下。",
    capabilitiesSubtitle:
      "从提示词到工具调用再到结果，Orca 把整个智能体循环呈现为可读、可验证、可恢复的流程，而不是黑箱。",
    builtInToolsLabel: "内置工具",
    capabilities: [
      {
        title: "会话与恢复",
        body: "JSONL 会话记录存放在 ~/.orca/sessions/，可用 --resume 继续，也可用 --fork 分支出新的运行。",
      },
      {
        title: "验证器循环",
        body: "通过 --verifier \"cargo test\" 用真实命令约束运行；失败会以 exit code 2 报告。",
      },
      {
        title: "Hooks 与自定义工具",
        body: "生命周期 hooks 返回结构化 JSON，可拒绝、修改或注入上下文；内置工具、MCP 工具和 TOML 描述符共用同一注册表。",
      },
      {
        title: "结构化事件流",
        body: "--output-format jsonl 会从 session.started 到 tool.call.completed 输出版本化事件，便于接入编排系统。",
      },
    ],
    workflowEyebrow: "命令界面",
    workflowTitle: "用同一套动词覆盖真实开发循环。",
    codeTabs: {
      exec: "orca exec",
      goal: "持久 goal",
      history: "历史 / 恢复",
      config: "配置",
    },
    code: {
      execTask: "交给它一个任务",
      execRefactor: "full-auto 模式重构",
      execModel: "指定模型并用 verifier 校验",
      execDone: "  ✓ 修改 3 个文件 · cargo test 通过 · exit 0",
      goalSet: "设置长期目标；它会自动继续",
      goalEdit: "更新并重新激活",
      goalPause: "停止自动继续",
      goalResume: "空闲时继续",
      goalStored: "  按 session id 存在 ~/.orca/goals_1.json — 重启后仍保留。",
      historyBrowse: "浏览、搜索和恢复会话记录",
      historyStored: "  存放在 ~/.orca/sessions/YYYY/MM/DD/；大型运行可用 zstd 压缩。",
      configMainLoop: "主循环 v4-pro，辅助任务 v4-flash",
      configPriority: "优先级：环境变量 > CLI 参数 > 配置文件 > 默认值",
    },
    specsEyebrow: "技术规格",
    specsLabel: "技术规格",
    specs: {
      context: "上下文窗口，超过 80% 阈值后自动压缩。",
      platforms: "原生二进制：macOS 与 Linux，arm64 / x64。",
      tools: "内置、MCP 和 external 工具共用规格驱动注册表。",
      rust: "Rust 编写，以单个本地二进制运行。",
    },
    install: {
      eyebrow: "安装",
      title: "使用 npm，或直接安装原生二进制。",
      cardLabel: "安装 Orca",
      methodLabel: "安装方式",
      copy: "复制",
      copied: "✓ 已复制",
      failed: "复制失败",
      platforms: "支持平台：macOS arm64/x64 和 Linux arm64/x64。下载文件位于",
      releases: "GitHub Releases",
    },
    community: {
      qq: "QQ 群 472309526",
      telegram: "Telegram",
    },
    tui: {
      user: "修复失败的 auth 测试",
      reasoning: "推理",
      reason1: "定位失败用例，检查 token 过期时间比较…",
      reason2Prefix: "过期判断使用了",
      reason2Middle: "；边界秒被错误地视为有效，应该改为",
      approve: "⚑ 审批 · edit src/auth/token.rs",
      approved: "已批准",
      grepResult: "→ 匹配到 3 个断言",
      readResult: "→ 读取 86 行",
      editResult: "→ 写入 1 处修改",
      bashResult: "→ 正在运行 4 个测试…",
      ok: "✓ test auth::token_expiry ... ok · 4 passed",
      done: "✓ 完成 · 修改 1 个文件 · cargo test 通过 · exit 0",
      footerBacktrack: "回退",
      footerGoal: "目标",
      footerExit: "退出",
      statusContext: "上下文",
    },
  },
} as const;

const capabilityIcons = [
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7">
    <path d="M21 12a9 9 0 1 1-6.2-8.5" />
    <path d="M21 4v5h-5" />
  </svg>,
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7">
    <path d="M9 11l3 3L22 4" />
    <path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11" />
  </svg>,
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7">
    <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
    <path d="M14 2v6h6M9 13h6M9 17h6" />
  </svg>,
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7">
    <path d="M4 4h16v12H4z" />
    <path d="M8 20h8M12 16v4M7 9l2 2 6-6" />
  </svg>,
];

const builtinTools = [
  "read_file",
  "glob",
  "edit",
  "grep",
  "bash",
  "write_file",
  "git_status",
  "web_search",
  "subagent",
  "Workflow",
  "update_plan",
  "get_goal",
  "create_goal",
  "update_goal",
  "MCP",
  "external",
];

type InstallMode = "npm" | "curl";
type CopyState = "idle" | "copied" | "failed";
type CodeTab = "exec" | "goal" | "history" | "config";

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

type TuiBlock = {
  kind: "user" | "reason" | "tool" | "approve" | "ok" | "done";
  content: ReactNode;
  ctx: number;
};

function makeTuiBlocks(t: (typeof copy)[Locale]): TuiBlock[] {
  return [
    { kind: "user", ctx: 12, content: <><span className="who">you ›</span> {t.tui.user}</> },
    {
      kind: "reason",
      ctx: 21,
      content: (
        <div className="tb-reason">
          <span className="lbl">{t.tui.reasoning}</span>
          {t.tui.reason1}
        </div>
      ),
    },
    {
      kind: "tool",
      ctx: 30,
      content: (
        <div className="tb-tool">
          <div className="th">
            <span className="ic" />
            grep <span className="arg">"assert_eq" tests/auth.rs</span>
          </div>
          <div className="res">{t.tui.grepResult}</div>
        </div>
      ),
    },
    {
      kind: "tool",
      ctx: 38,
      content: (
        <div className="tb-tool">
          <div className="th">
            <span className="ic" />
            read_file <span className="arg">src/auth/token.rs</span>
          </div>
          <div className="res">{t.tui.readResult}</div>
        </div>
      ),
    },
    {
      kind: "reason",
      ctx: 47,
      content: (
        <div className="tb-reason">
          <span className="lbl">{t.tui.reasoning}</span>
          {t.tui.reason2Prefix} <span style={{ color: "var(--warn)" }}>&lt;=</span>
          {t.tui.reason2Middle} <span style={{ color: "var(--accent-2)" }}>&lt;</span>.
        </div>
      ),
    },
    {
      kind: "approve",
      ctx: 53,
      content: (
        <div className="tb-approve">
          {t.tui.approve}
          <span className="chip">{t.tui.approved}</span>
        </div>
      ),
    },
    {
      kind: "tool",
      ctx: 61,
      content: (
        <div className="tb-tool">
          <div className="th">
            <span className="ic" />
            edit <span className="arg">src/auth/token.rs</span>
          </div>
          <div className="res">{t.tui.editResult}</div>
        </div>
      ),
    },
    {
      kind: "tool",
      ctx: 72,
      content: (
        <div className="tb-tool">
          <div className="th">
            <span className="ic" />
            bash <span className="arg">cargo test auth</span>
          </div>
          <div className="res">{t.tui.bashResult}</div>
        </div>
      ),
    },
    { kind: "ok", ctx: 78, content: <div className="tb-ok">{t.tui.ok}</div> },
    { kind: "done", ctx: 80, content: <div className="tb-done">{t.tui.done}</div> },
  ];
}

function renderCodeTab(tab: CodeTab, t: (typeof copy)[Locale]) {
  const c = t.code;
  const tabs: Record<CodeTab, ReactNode> = {
    exec: (
      <pre>
        <span className="k-com"># {c.execTask}</span>
        {"\n"}
        <span className="k-cmd">orca</span> exec <span className="k-str">"fix this test"</span>
        {"\n\n"}
        <span className="k-com"># {c.execRefactor}</span>
        {"\n"}
        <span className="k-cmd">orca</span> exec <span className="k-flag">--approval-mode</span>{" "}
        full-auto <span className="k-str">"refactor the auth module"</span>
        {"\n\n"}
        <span className="k-com"># {c.execModel}</span>
        {"\n"}
        <span className="k-cmd">orca</span> exec <span className="k-flag">--model</span>{" "}
        deepseek-v4-pro <span className="k-flag">--verifier</span>{" "}
        <span className="k-str">"cargo test"</span>{" "}
        <span className="k-str">"fix the failing test"</span>
        {"\n"}
        <span className="k-out">{c.execDone}</span>
      </pre>
    ),
    goal: (
      <pre>
        <span className="k-com"># {c.goalSet}</span>
        {"\n"}
        <span className="k-cmd">/goal</span> ship the refactor
        {"\n"}
        <span className="k-cmd">/goal</span> edit finish the parser{" "}
        <span className="k-com"># {c.goalEdit}</span>
        {"\n"}
        <span className="k-cmd">/goal</span> pause <span className="k-com"># {c.goalPause}</span>
        {"\n"}
        <span className="k-cmd">/goal</span> resume <span className="k-com"># {c.goalResume}</span>
        {"\n\n"}
        <span className="k-out">{c.goalStored}</span>
      </pre>
    ),
    history: (
      <pre>
        <span className="k-com"># {c.historyBrowse}</span>
        {"\n"}
        <span className="k-cmd">orca</span> history list
        {"\n"}
        <span className="k-cmd">orca</span> history search <span className="k-str">"needle"</span>
        {"\n"}
        <span className="k-cmd">orca</span> exec <span className="k-flag">--resume</span> latest{" "}
        <span className="k-str">"continue the refactor"</span>
        {"\n"}
        <span className="k-cmd">orca</span> exec <span className="k-flag">--fork</span> latest{" "}
        <span className="k-str">"try another approach"</span>
        {"\n"}
        <span className="k-out">{c.historyStored}</span>
      </pre>
    ),
    config: (
      <pre>
        <span className="k-com"># ~/.orca/config.toml</span>
        {"\n"}
        model = <span className="k-str">"auto"</span>{" "}
        <span className="k-com"># {c.configMainLoop}</span>
        {"\n"}
        api_key = <span className="k-str">"sk-..."</span>
        {"\n"}
        base_url = <span className="k-str">"https://api.deepseek.com"</span>
        {"\n\n"}
        <span className="k-com"># {c.configPriority}</span>
      </pre>
    ),
  };

  return tabs[tab];
}

function ctxBar(pct: number) {
  const filled = Math.round(pct / 10);
  return "█".repeat(filled) + "░".repeat(10 - filled);
}

function useTuiAnimation(tuiBlocks: TuiBlock[], tuiUserMsg: string) {
  const [visibleCount, setVisibleCount] = useState(0);
  const [typed, setTyped] = useState("");
  const [phase, setPhase] = useState<"typing" | "streaming">("typing");

  useEffect(() => {
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    if (reduce) {
      setTyped("");
      setPhase("streaming");
      setVisibleCount(tuiBlocks.length);
      return;
    }

    const timers: number[] = [];
    let cancelled = false;

    function run() {
      setVisibleCount(0);
      setTyped("");
      setPhase("typing");

      // Type the user message into the composer.
      let i = 0;
      const type = () => {
        if (cancelled) return;
        setTyped(tuiUserMsg.slice(0, i));
        if (i <= tuiUserMsg.length) {
          i += 1;
          timers.push(window.setTimeout(type, 46));
        } else {
          timers.push(window.setTimeout(stream, 420));
        }
      };

      // Reveal conversation blocks one by one.
      const stream = () => {
        if (cancelled) return;
        setTyped("");
        setPhase("streaming");
        let n = 0;
        const reveal = () => {
          if (cancelled) return;
          n += 1;
          setVisibleCount(n);
          if (n < tuiBlocks.length) {
            timers.push(window.setTimeout(reveal, 560));
          } else {
            timers.push(window.setTimeout(run, 4200));
          }
        };
        reveal();
      };

      type();
    }

    run();

    return () => {
      cancelled = true;
      timers.forEach((t) => window.clearTimeout(t));
    };
  }, [tuiBlocks, tuiUserMsg]);

  const ctx = visibleCount > 0 ? tuiBlocks[visibleCount - 1].ctx : 8;
  return { visibleCount, typed, phase, ctx };
}

function App() {
  const [mode, setMode] = useState<InstallMode>("npm");
  const [copyState, setCopyState] = useState<CopyState>("idle");
  const [codeTab, setCodeTab] = useState<CodeTab>("exec");
  const [locale, setLocale] = useState<Locale>(detectInitialLocale);
  const resetTimerRef = useRef<number | null>(null);
  const copyRequestRef = useRef(0);
  const tabRefs = useRef<Record<InstallMode, HTMLButtonElement | null>>({
    npm: null,
    curl: null,
  });
  const command = mode === "npm" ? npmCommand : curlCommand;
  const t = copy[locale];
  const tuiBlocks = useMemo(() => makeTuiBlocks(t), [t]);
  const tui = useTuiAnimation(tuiBlocks, t.tui.user);

  useEffect(() => {
    window.localStorage.setItem(localeStorageKey, locale);
    applySeoHead(locale, seoCopy[locale] satisfies SeoEntry, canonicalUrl);
  }, [locale]);

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

  const copyLabel =
    copyState === "copied"
      ? t.install.copied
      : copyState === "failed"
        ? t.install.failed
        : t.install.copy;

  return (
    <main>
      <header className="nav">
        <a className="brand" href="#top" aria-label={t.aria.home}>
          <img className="brand-mark" src="/orca-icon.svg" alt="" aria-hidden="true" />
          <span>Orca</span>
        </a>
        <div className="nav-actions">
          <nav aria-label={t.aria.nav}>
            <a href="#features">{t.nav.features}</a>
            <a href="#capabilities">{t.nav.capabilities}</a>
            <a href="#workflow">{t.nav.workflow}</a>
            <a href="#install">{t.nav.install}</a>
            <a href={links.changelog}>{t.nav.changelog}</a>
            <a className="nav-cta" href={links.github} rel="noreferrer">
              {t.nav.github}
            </a>
          </nav>
          <div className="locale-switch" role="group" aria-label={t.aria.language}>
            <button
              type="button"
              aria-pressed={locale === "en"}
              aria-label={copy.en.langName}
              onClick={() => setLocale("en")}
            >
              EN
            </button>
            <button
              type="button"
              aria-pressed={locale === "zh"}
              aria-label={copy.zh.langName}
              onClick={() => setLocale("zh")}
            >
              中文
            </button>
          </div>
        </div>
      </header>

      <section className="hero" id="top">
        <div className="hero-copy">
          <span className="pill">
            <span className="dot" />
            {t.hero.pill}
          </span>
          <h1>
            {t.hero.titlePrefix} <span className="grad">{t.hero.titleHighlight}</span>{" "}
            {t.hero.titleSuffix}
          </h1>
          <p className="subtitle">{t.hero.subtitle}</p>

          <div className="actions">
            <a className="primary" href="#install">
              {t.hero.primary}
            </a>
            <a className="secondary" href={links.github} rel="noreferrer">
              {t.hero.secondary}
            </a>
          </div>

          <div className="hero-meta">
            <div>
              <span className="k">1M</span>
              <span className="l">{t.hero.meta.context}</span>
            </div>
            <div>
              <span className="k">99%</span>
              <span className="l">{t.hero.meta.cache}</span>
            </div>
            <div>
              <span className="k">128</span>
              <span className="l">{t.hero.meta.turns}</span>
            </div>
            <div>
              <span className="k">14</span>
              <span className="l">{t.hero.meta.tools}</span>
            </div>
            <div>
              <span className="k">4</span>
              <span className="l">{t.hero.meta.platforms}</span>
            </div>
          </div>
        </div>

        <div className="terminal" aria-label={t.aria.tui}>
          <div className="terminal-bar">
            <span />
            <span />
            <span />
            <span className="terminal-title">orca · TUI — ~/projects/api</span>
          </div>
          <div className="tui-status">
            <span>
              <span className="accent">orca</span>{" "}
              <span className="dim">deepseek-v4-pro</span> ·{" "}
              <span className="dim">full-auto</span>
            </span>
            <span className="tui-ctx">
              {t.tui.statusContext} <span className="bar">{ctxBar(tui.ctx)}</span> {tui.ctx}%
            </span>
          </div>
          <div className="tui-body" aria-hidden="true">
            {tuiBlocks.slice(0, tui.visibleCount).map((block, index) => (
              <div key={index} className={`tb tb-${block.kind}`}>
                {block.content}
              </div>
            ))}
          </div>
          <div className="tui-composer">
            <span className="prompt">›</span>
            <span className="input">{tui.phase === "typing" ? tui.typed : ""}</span>
            <span className="tui-cur" />
          </div>
          <div className="tui-foot">
            <span>
              <span className="key">esc</span> {t.tui.footerBacktrack} ·{" "}
              <span className="key">/goal</span> {t.tui.footerGoal} ·{" "}
              <span className="key">^c</span> {t.tui.footerExit}
            </span>
            <span className="dim">1M · {releaseVersion}</span>
          </div>
        </div>
      </section>

      <section className="features" id="features" aria-labelledby="features-heading">
        <div className="section-heading">
          <p className="eyebrow">{t.featuresEyebrow}</p>
          <h2 id="features-heading">{t.featuresTitle}</h2>
        </div>
        <div className="feature-grid">
          {t.features.map((feature) => (
            <article key={feature.title}>
              <h3>{feature.title}</h3>
              <p>{feature.body}</p>
            </article>
          ))}
        </div>
        <article className="cache-card" aria-labelledby="cache-card-heading">
          <div className="cache-card-copy">
            <p className="eyebrow">{t.cacheCard.eyebrow}</p>
            <h3 id="cache-card-heading">{t.cacheCard.title}</h3>
            <p>{t.cacheCard.body}</p>
          </div>
          <div className="cache-card-stats" aria-hidden="true">
            {t.cacheCard.stats.map((stat) => (
              <div key={stat.l}>
                <span className="num">{stat.k}</span>
                <span className="l">{stat.l}</span>
              </div>
            ))}
          </div>
        </article>
      </section>

      <section className="capabilities" id="capabilities" aria-labelledby="capabilities-heading">
        <div className="cap-lead">
          <p className="eyebrow">{t.capabilitiesEyebrow}</p>
          <h2 id="capabilities-heading">{t.capabilitiesTitle}</h2>
          <p className="subtitle">{t.capabilitiesSubtitle}</p>
          <div className="tools-wrap" aria-label={t.builtInToolsLabel}>
            {builtinTools.map((tool) => (
              <span className="tool-chip" key={tool}>
                <span className="tc-dot" />
                {tool}
              </span>
            ))}
          </div>
        </div>
        <div className="cap-rows">
          {t.capabilities.map((cap, index) => (
            <div className="cap-row" key={cap.title}>
              <span className="ci">{capabilityIcons[index]}</span>
              <div>
                <h3>{cap.title}</h3>
                <p>{cap.body}</p>
              </div>
            </div>
          ))}
        </div>
      </section>

      <section className="workflow" id="workflow" aria-labelledby="workflow-heading">
        <div className="section-heading" style={{ marginBottom: 40 }}>
          <p className="eyebrow">{t.workflowEyebrow}</p>
          <h2 id="workflow-heading">{t.workflowTitle}</h2>
        </div>
        <div className="code-panel">
          <div className="code-tabs" role="tablist" aria-label={t.aria.commands}>
            {(Object.keys(t.codeTabs) as CodeTab[]).map((tab) => (
              <button
                key={tab}
                role="tab"
                type="button"
                aria-selected={codeTab === tab}
                onClick={() => setCodeTab(tab)}
              >
                {t.codeTabs[tab]}
              </button>
            ))}
          </div>
          {renderCodeTab(codeTab, t)}
        </div>
      </section>

      <section className="specs" aria-label={t.specsLabel}>
        <p className="eyebrow">{t.specsEyebrow}</p>
        <div className="spec-grid">
          <div>
            <div className="num">
              1<span className="u">M</span>
            </div>
            <p>{t.specs.context}</p>
          </div>
          <div>
            <div className="num">
              4<span className="u">×</span>
            </div>
            <p>{t.specs.platforms}</p>
          </div>
          <div>
            <div className="num">14</div>
            <p>{t.specs.tools}</p>
          </div>
          <div>
            <div className="num">
              100<span className="u">%</span>
            </div>
            <p>{t.specs.rust}</p>
          </div>
        </div>
      </section>

      <section className="install-repeat" id="install" aria-labelledby="install-heading">
        <div>
          <p className="eyebrow">{t.install.eyebrow}</p>
          <h2 id="install-heading">{t.install.title}</h2>
        </div>
        <div className="install-list">
          <div className="install-card" aria-label={t.install.cardLabel}>
            <div className="tabs" role="tablist" aria-label={t.install.methodLabel}>
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
              <button
                type="button"
                onClick={copyCommand}
                className="copy"
                data-state={copyState}
              >
                {copyLabel}
              </button>
            </div>
          </div>
          <p>
            {t.install.platforms}{" "}
            <a href={links.releases} rel="noreferrer">
              {t.install.releases}
            </a>
            .
          </p>
        </div>
      </section>

      <footer>
        <a className="foot-brand" href="#top">
          <img className="brand-mark" src="/orca-icon.svg" alt="" aria-hidden="true" />
          <span>Orca</span>
        </a>
        <div className="links">
          <a href={links.github} rel="noreferrer">
            GitHub
          </a>
          <a href={links.npm} rel="noreferrer">
            npm
          </a>
          <a href={links.releases} rel="noreferrer">
            {t.install.releases}
          </a>
          <span>{t.community.qq}</span>
          <a href={links.telegram} rel="noreferrer">
            {t.community.telegram}
          </a>
        </div>
      </footer>
    </main>
  );
}

export default App;

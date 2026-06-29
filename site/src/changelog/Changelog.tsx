import { useEffect, useState } from "react";
import {
    type Locale,
    type SeoEntry,
    applySeoHead,
    canonicalOrigin,
    detectInitialLocale,
    links,
    localeStorageKey,
    releaseVersion,
    releases,
} from "../shared";

const canonicalUrl = `${canonicalOrigin}/changelog/`;

const seoCopy: Record<Locale, SeoEntry> = {
  en: {
    title: "Orca changelog",
    description:
      "Every Orca release in one place: what shipped, when, and a link to the full GitHub release notes.",
    ogTitle: "Orca changelog",
    ogDescription:
      "Every Orca release in one place: what shipped, when, and a link to the full GitHub release notes.",
    imageAlt: "Orca terminal coding agent product preview",
    locale: "en_US",
  },
  zh: {
    title: "Orca 更新日志",
    description: "Orca 每一个版本：发了什么、何时发布，附 GitHub Release 完整说明链接。",
    ogTitle: "Orca 更新日志",
    ogDescription: "Orca 每一个版本：发了什么、何时发布，附 GitHub Release 完整说明链接。",
    imageAlt: "Orca 终端代码智能体产品预览",
    locale: "zh_CN",
  },
};

const copy = {
  en: {
    langName: "English",
    aria: {
      home: "Orca home",
      language: "Language",
    },
    nav: {
      home: "Home",
      install: "Install",
      github: "GitHub",
    },
    header: {
      eyebrow: "Changelog",
      title: "Every Orca release, in order.",
      subtitle:
        "Versions follow semver; each entry links to the full GitHub Release notes for verification commands, breaking changes, and migration tips.",
      latestLabel: "latest",
      readNotes: "Release notes",
    },
    summaries: {
      "v0.1.54":
        "Realtime server streams and persisted thread projections now share MCP tool parsing, JSON argument parsing, MCP result shaping, and camelCase tool error helpers to reduce item schema drift.",
      "v0.1.53":
        "Realtime mcpToolCall and dynamicToolCall item errors now include exitCode when tool completion reports one, keeping server streams aligned with persisted thread item projections.",
      "v0.1.52":
        "MCP initialize capabilities are now cached per server, so all-server resource/template discovery skips tools-only servers while explicit server filters still report that server's real error.",
      "v0.1.51":
        "MCP resource and template discovery now includes registry-level startup errors in all-server results, so failed MCP servers stay visible alongside healthy resource context.",
      "v0.1.50":
        "MCP resource templates are now model-visible through list_mcp_resource_templates, with resources/templates/list wired through stdio/SSE and partial per-server error reporting.",
      "v0.1.49":
        "MCP resource discovery now returns available resources even when another server fails, with per-server errors surfaced in the list_mcp_resources result.",
      "v0.1.48":
        "MCP resource tools ship with a hardened server-mode JSONL test harness, so noisy child-process output no longer flakes task_stop shell-session release coverage.",
      "v0.1.47":
        "MCP resources are now model-visible through read-only list_mcp_resources and read_mcp_resource tools, with stdio/SSE resources/list and resources/read support wired through the shared registry.",
      "v0.1.46":
        "Structured hook JSON stdout now validates declared actions and required string fields, so typoed or malformed hook outputs fail visibly instead of being silently injected or ignored.",
      "v0.1.45":
        "Tool argument validation now evaluates JSON Schema oneOf and anyOf branches before execution, keeping runtime rejection behavior aligned with advertised provider schemas.",
      "v0.1.44":
        "Model-facing file discovery now supports fuzzy path queries through glob mode=fuzzy, while preserving existing glob pattern behavior and list_files compatibility.",
      "v0.1.43":
        "Runtime turn orchestration now lives behind lifecycle-owned turn opening, provider cycle, iteration, loop, and loop-input boundaries, shrinking the agent loop entrypoint while preserving behavior.",
      "v0.1.42":
        "Claude Code-style workflow parity loop: generated drafts, edit/save/run controls, reusable workflow commands, evidence-bound reports, and process-tree timeout cleanup.",
      "v0.1.41":
        "Workflow concurrency control rewrite (Promise.allSettled with fail-fast), structured failure taxonomy (tool/MCP/token/schema), concurrency metrics in evidence bundles, and stress-test coverage.",
      "v0.1.40":
        "Workflow evidence bundles with standardized reporting (Markdown + JSON), automatic evidence capture at lifecycle checkpoints, and contract validation tests.",
      "v0.1.39":
        "Workflow child task list tools, typed output schemas for subagents, team tool allowlists, durable IPC state, and agent lifecycle observability.",
      "v0.1.38":
        "History/session persistence now flows through a dedicated SessionStore boundary, with runtime session/controller call sites aligned to the same entry point.",
      "v0.1.37":
        "Shell execution now honors the configurable timeout, with timeout-aware child process waiting shared by bash and external tools.",
      "v0.1.36":
        "Workflow agent runs now support worktree isolation, async handle recovery, and continue-on-failure phase fallback in the TUI workflow view.",
      "v0.1.35":
        "Bracketed paste support in TUI input; textarea soft-wrap rendering rewritten with accurate height calculation.",
      "v0.1.34":
        "Add a reusable real API release gate that verifies provider summary costs, CLI JSONL output, and server-mode streaming before tagging.",
      "v0.1.33":
        "Centralize runtime tool invocation records, approval request construction, and hook-modified request validation across built-in, MCP, and external tools.",
      "v0.1.32":
        "Add a typed runtime protocol boundary for server submissions and events while preserving the existing flat JSON wire format.",
      "v0.1.31":
        "Runtime-owned interactive sessions now centralize conversation, history, instructions, memory, hooks, MCP, cost tracking, and workflow task state before the protocol split.",
      "v0.1.30":
        "Workflow DSL and multi-stage runtime overhaul; TUI now shows workflow/task progress, elapsed time, notifications, and clearer approval choices.",
      "v0.1.29":
        "Refactor TUI session preloading for clarity; extract goal session ID helper; add unit tests for session restoration and goal control flow.",
      "v0.1.28":
        "Drop legacy deepseek-chat / deepseek-reasoner; tool arguments are JSON-Schema validated before any call; TUI text-wrap rewritten for wide chars and ANSI.",
      "v0.1.27":
        "Kill the cache-compaction storm: wire-equivalent gating + 60% hysteresis, persist inherited summary state across --continue and --fork.",
      "v0.1.26":
        "Update check falls back to npm registry (no rate limit); table rendering rewritten with progressive degradation down to narrow terminals.",
    },
    foot: {
      releases: "GitHub Releases",
      qq: "QQ Group 472309526",
      telegram: "Telegram",
    },
  },
  zh: {
    langName: "中文",
    aria: {
      home: "Orca 首页",
      language: "语言",
    },
    nav: {
      home: "首页",
      install: "安装",
      github: "GitHub",
    },
    header: {
      eyebrow: "更新日志",
      title: "Orca 历次发布。",
      subtitle:
        "版本遵循 semver；每条记录都链接到 GitHub Release 的完整说明，含校验命令、breaking change 与迁移提示。",
      latestLabel: "最新",
      readNotes: "查看发布说明",
    },
    summaries: {
      "v0.1.54":
        "实时 server stream 与持久化 thread projection 现在共享 MCP tool 解析、JSON 参数解析、MCP result shaping 和 camelCase tool error helper，减少 item schema drift。",
      "v0.1.53":
        "实时 mcpToolCall / dynamicToolCall item error 现在会在工具完成事件提供 exit_code 时携带 exitCode，与持久化 thread item 投影保持一致。",
      "v0.1.52":
        "MCP initialize capabilities 现在会按 server 缓存；all-server resource/template 发现会跳过 tools-only server，显式 server 查询仍返回该 server 的真实错误。",
      "v0.1.51":
        "MCP resource / template 发现现在会在 all-server 结果里带上 registry 级启动错误，让失败的 MCP server 和健康资源上下文一起可见。",
      "v0.1.50":
        "MCP resource templates 现在通过 list_mcp_resource_templates 暴露给模型，stdio/SSE 已接入 resources/templates/list，并支持按 server 聚合部分失败错误。",
      "v0.1.49":
        "MCP resource 发现现在会保留可用 server 的资源，并把失败 server 的错误聚合到 list_mcp_resources 结果里，不再因为单点失败丢掉全部上下文。",
      "v0.1.48":
        "MCP resource 工具随更稳的 server-mode JSONL 测试 harness 一起发布，子进程噪声不再让 task_stop shell-session 覆盖在 CI 中偶发失败。",
      "v0.1.47":
        "MCP resources 现在通过只读的 list_mcp_resources / read_mcp_resource 暴露给模型，stdio/SSE 的 resources/list 与 resources/read 也接入了统一工具注册表。",
      "v0.1.46":
        "结构化 hook JSON stdout 现在会校验声明的 action 与必需字符串字段，拼错或格式错误的 hook 输出会显式失败，不再被静默注入或忽略。",
      "v0.1.45":
        "工具参数执行前校验现在支持 JSON Schema 的 oneOf / anyOf 分支，runtime 拒绝行为与暴露给模型的 provider schema 更一致。",
      "v0.1.44":
        "模型侧文件发现补齐 fuzzy path query：`glob` 可通过 mode=fuzzy 按路径片段/首字母查找，同时保留原有 glob pattern 行为和 list_files 兼容入口。",
      "v0.1.43":
        "Runtime turn 编排继续内聚到 lifecycle 边界：turn opening、provider cycle、iteration、loop 与 loop input 都由 runtime 持有，agent loop 入口更薄且行为保持兼容。",
      "v0.1.42":
        "补齐 Claude Code 风格 workflow 闭环：生成草稿、编辑/保存/运行控制、可复用 workflow 命令、证据绑定报告，以及进程树级超时清理。",
      "v0.1.41":
        "重写工作流并发控制（Promise.allSettled + 首错快速失败）、结构化失败分类（工具/MCP/令牌/Schema）、证据包并发指标及压力测试覆盖。",
      "v0.1.40":
        "新增工作流证据包（Evidence Bundle）与标准化报告生成（Markdown + JSON），生命周期各节点自动写入证据，配套合约校验测试。",
      "v0.1.39":
        "工作流子任务列表工具、subagent 强类型输出 schema、团队工具白名单、持久化 IPC 状态及 agent 生命周期可观测性。",
      "v0.1.38":
        "历史 / 会话持久化现在经过专门的 SessionStore 边界，runtime 的 session/controller 调用点也统一到了同一入口。",
      "v0.1.37":
        "Shell 执行现在会遵守可配置超时，bash 和外部工具共享统一的超时等待子进程逻辑。",
      "v0.1.36":
        "工作流 agent 运行现在支持 worktree 隔离、异步句柄恢复，以及在 TUI 工作流视图中继续执行失败后续 phase。",
      "v0.1.35":
        "TUI 输入框支持括号粘贴（Bracketed Paste）；重写文本区域软换行渲染，修复高度计算不准确问题。",
      "v0.1.34":
        "新增可重复执行的真实 API 发布闸门，发版前统一验证 provider summary 成本、CLI JSONL 输出和 server-mode 流式事件。",
      "v0.1.33":
        "统一 runtime 工具调用记录、审批请求构造与 hook 修改后的请求校验，覆盖内置工具、MCP 工具和外部工具。",
      "v0.1.32":
        "新增 runtime 侧强类型 protocol 边界，server submission 与 event 映射不再散落在松散 JSON 中，同时保持现有扁平 JSON wire 格式兼容。",
      "v0.1.31":
        "交互会话状态改由 runtime 统一持有，集中管理 conversation、历史、instructions、memory、hooks、MCP、成本统计和 workflow task 状态，为 protocol 拆分打基础。",
      "v0.1.30":
        "重构 workflow DSL 与多阶段运行时；TUI 现在展示 workflow/task 进度、运行时长、通知和更清晰的审批选项。",
      "v0.1.29":
        "重构 TUI 会话预加载逻辑，提取 goal session ID 辅助函数，新增会话恢复与目标控制流的单元测试。",
      "v0.1.28":
        "移除旧版 deepseek-chat / deepseek-reasoner；工具参数在调用前按 JSON Schema 校验；重写 TUI 文本换行，支持宽字符与 ANSI 段。",
      "v0.1.27":
        "终结缓存压缩风暴：按真实 wire 提示词触发 + 60% 压缩滞后，--continue 与 --fork 现在会持久化继承的 summary 状态。",
      "v0.1.26":
        "版本更新检查优先走 npm registry（无限流），表格渲染重写为渐进降级，窄终端也能读。",
    },
    foot: {
      releases: "GitHub Releases",
      qq: "QQ 群 472309526",
      telegram: "Telegram",
    },
  },
} as const;

function Changelog() {
  const [locale, setLocale] = useState<Locale>(detectInitialLocale);
  const t = copy[locale];

  useEffect(() => {
    window.localStorage.setItem(localeStorageKey, locale);
    applySeoHead(locale, seoCopy[locale], canonicalUrl);
  }, [locale]);

  return (
    <main>
      <header className="nav">
        <a className="brand" href={links.home} aria-label={t.aria.home}>
          <img className="brand-mark" src="/orca-icon.svg" alt="" aria-hidden="true" />
          <span>Orca</span>
        </a>
        <div className="nav-actions">
          <nav aria-label="Main navigation">
            <a href={links.home}>{t.nav.home}</a>
            <a href={`${links.home}#install`}>{t.nav.install}</a>
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

      <section className="changelog-hero">
        <span className="pill">
          <span className="dot" />
          {releaseVersion} · {t.header.latestLabel}
        </span>
        <p className="eyebrow">{t.header.eyebrow}</p>
        <h1>{t.header.title}</h1>
        <p className="subtitle">{t.header.subtitle}</p>
      </section>

      <section className="changelog-page" aria-labelledby="changelog-heading">
        <h2 id="changelog-heading" className="visually-hidden">
          {t.header.eyebrow}
        </h2>
        <ol className="changelog-list">
          {releases.map((release, idx) => (
            <li key={release.version} className="changelog-item">
              <a
                href={release.url}
                rel="noreferrer"
                aria-label={`${release.version} ${t.header.readNotes}`}
              >
                <div className="changelog-meta">
                  <span className="changelog-version">{release.version}</span>
                  {idx === 0 ? (
                    <span className="changelog-latest">{t.header.latestLabel}</span>
                  ) : null}
                  <time className="changelog-date" dateTime={release.date}>
                    {release.date}
                  </time>
                </div>
                <p className="changelog-summary">{t.summaries[release.version]}</p>
                <span className="changelog-link">{t.header.readNotes}</span>
              </a>
            </li>
          ))}
        </ol>
      </section>

      <footer>
        <a className="foot-brand" href={links.home}>
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
            {t.foot.releases}
          </a>
          <span>{t.foot.qq}</span>
          <a href={links.telegram} rel="noreferrer">
            {t.foot.telegram}
          </a>
        </div>
      </footer>
    </main>
  );
}

export default Changelog;

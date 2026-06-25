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

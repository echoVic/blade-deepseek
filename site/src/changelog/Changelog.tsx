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
      "v0.1.123":
        "Runtime turn setup now lives in a focused runtime_turn_setup module. Agent loop still delegates through RuntimeTurnSetupStep, and the new module owns context budget setup, tool approval policy construction, and provider config composition while lifecycle.rs keeps actor/lifecycle primitives.",
      "v0.1.122":
        "Runtime conversation bootstrap now lives in a focused runtime_conversation_bootstrap module. Agent loop still delegates through RuntimeConversationBootstrapStep, and the new module owns RuntimePreparedConversation, borrowed-or-owned conversation storage, session bootstrap composition, and initial history recording while lifecycle.rs keeps actor/lifecycle primitives.",
      "v0.1.121":
        "Runtime steer application now lives in a focused runtime_steer module with a grouped RuntimeSteerInput boundary. RuntimeTurnOpeningStep and RuntimeProviderTurnStep still drain pending steer inputs into the conversation and history before the model call, while lifecycle.rs keeps ThreadSteerHandle storage and sheds another reducer slice.",
      "v0.1.120":
        "Runtime model-route orchestration now lives in a focused runtime_model_route module with a grouped RuntimeModelRouteInput boundary. RuntimeTurnOpeningStep still composes compaction, turn start, model routing, and steering in the same order, while lifecycle.rs keeps the actor/lifecycle primitives and sheds another reducer slice without adding a new long-argument surface.",
      "v0.1.119":
        "Runtime turn-start orchestration now lives in a focused runtime_turn_start module instead of lifecycle.rs. RuntimeTurnOpeningStep still composes compaction, turn start, model routing, and steering in the same order, while lifecycle.rs keeps the actor/lifecycle primitives and sheds another lower-level reducer slice.",
      "v0.1.118":
        "Runtime turn-opening orchestration now lives in a focused runtime_turn_opening module with a grouped RuntimeTurnOpeningInput boundary. RuntimeTurnIterationStep still composes opening and provider-cycle execution in the same order, while lifecycle.rs keeps the lower-level start/model-route/steer steps and sheds another reducer-sized layer.",
      "v0.1.117":
        "Runtime turn-iteration orchestration now lives in a focused runtime_turn_iteration module instead of lifecycle.rs. The outer runtime_turn_loop still delegates through RuntimeTurnIterationStep, provider-cycle behavior still lives in provider_turn, and lifecycle.rs keeps the opening/start/model-route pieces while getting smaller for the next reducer-style split.",
      "v0.1.116":
        "Runtime turn-loop orchestration now lives in a focused runtime_turn_loop module instead of lifecycle.rs. Agent loop still delegates through RuntimeTurnLoopStep with the same grouped input/executor objects and the same iteration retry/return behavior, while lifecycle.rs gets smaller for the next Codex/package-3-inspired reducer split.",
      "v0.1.115":
        "Shell-session bash execution now receives one grouped RuntimeBashInvocationContext instead of a long execute_bash_with_shell_session argument list. RuntimeNormalToolExecutor still owns the bash branch, permission overlays, cancellation, output truncation, task registry handoff, and network/filesystem permission retries keep the same behavior, while the bash boundary gets smaller for the next shell/session and async-subagent slices.",
      "v0.1.114":
        "Filesystem sandbox denials now recover more clearly across server command/exec and model-visible bash. Orca diagnoses macOS Seatbelt write blocks such as nested .git/index.lock failures, explains when they are sandbox scope issues rather than stale locks, requests a turn-scoped filesystem write grant when an approval handler is available, and retries the original command with the granted root.",
      "v0.1.113":
        "Tool-turn dispatch now receives one grouped RuntimeToolTurnsContext from provider response handling instead of a long run_tool_turns call. RuntimeStepContext, events, sink, conversation, history writer, tool requests, cost tracking, background workflow state, and child executors still flow unchanged while the provider-to-tool boundary gets smaller.",
      "v0.1.112":
        "Normal tool-turn execution now receives one grouped RuntimeNormalToolTurnContext instead of a long run_normal_tool_turn argument list. Tool execution, approval, result recording, plan-state recording, permission overlays, workflow/background state, and child executor handoff keep the same runtime behavior while the tool-turn boundary gets smaller.",
      "v0.1.111":
        "Tool approval gate inputs now move through one grouped ToolApprovalGateContext instead of a long handle_approval argument list. Config, events, sink, tool request, invocation, policy, strict auto-review, and delta emission still flow unchanged, while approval allow/ask/deny behavior and tool-call item emission keep the same public shape.",
      "v0.1.110":
        "Historical projected tool completions now rebuild through the shared complete_projected_tool_item helper in tool_item_projection.rs instead of thread_store/projection.rs calling MCP, dynamic, commandExecution, and fileChange completed-item constructors directly. Realtime and persisted history stay behavior-compatible while the remaining tool-item schema drift has one smaller ownership point.",
      "v0.1.109":
        "Runtime normal-tool routing now passes a grouped RuntimeNormalToolInvocation from the router into lifecycle actors instead of calling the long roots/cancel method directly. Bash shell-session execution, MCP/external fallback, permission overlays, cancellation, and output truncation keep the same behavior while the common tool path gets a smaller call surface for later shell and async-subagent work.",
      "v0.1.108":
        "Normal tool invocation now funnels through one runtime_normal_tool helper instead of letting lifecycle.rs instantiate the executor directly. RuntimeTaskActor and RuntimeToolActorContext still preserve the same bash, MCP, external, cancellation, and permission-overlay behavior, but the next shell-session and async subagent slices have a smaller call surface to build on.",
      "v0.1.107":
        "Tool-call argument streaming now reports progress end to end: a new tool.call.progress event and ToolCallProgress provider step flow through runtime and server, and the TUI renders received-bytes progress with cache-friendly updates. Adds an SSE streaming idle-timeout guard and fixes environment-variable proxy configuration and hook-timeout output handling.",
      "v0.1.106":
        "The normal-tool fallback path is now injectable through a focused RuntimeNormalToolFallbackExecutor boundary. MCP, TOML external, and built-in tool execution still use the same default orca-tools path, but the runtime can now test fallback context handoff without hardcoding that implementation.",
      "v0.1.105":
        "Normal tool execution now lives behind a focused RuntimeNormalToolExecutor boundary. The shell-session bash branch and the MCP/external/built-in fallback path move out of lifecycle.rs, while CLI, TUI, server, workflow, permission, and model-visible tool behavior stay unchanged.",
      "v0.1.104":
        "Runtime tool invocation dispatch now lives behind a focused RuntimeToolRouter boundary. ToolExecutionActor keeps invocation prep, approval, hooks, and result finalization, while workflow, subagent, task, permission, workflow IPC, and normal-tool routing move into the router without changing model-visible behavior.",
      "v0.1.103":
        "Runtime turn execution now carries cleaner grouped inputs: turn iteration, provider cycle, provider response, and tool turns share request-scoped context boundaries. This Codex/package-3-inspired slice reduces repeated runtime state plumbing while preserving CLI, TUI, server, tool, workflow, and history behavior.",
      "v0.1.102":
        "TUI child-agent execution now flows through runtime-owned request construction, model/cost setup, loop orchestration, provider handling, tool request extraction, and tool-result folding while TUI keeps only the interactive tool adapter. This keeps the new reasoning-effort configuration intact across child provider calls.",
      "v0.1.101":
        "Reasoning effort is now configurable (high or max, default max) via env vars, config file, and CLI arguments, carried on DeepSeek API requests. The TUI /model command becomes a two-step picker — choose the model, then the reasoning effort — with deferred apply, clean Esc cancellation, and a status bar that shows both.",
      "v0.1.100":
        "TUI polish: inline scrolling now detects real overflow via rendered-line-info, keeps auto-follow armed until content actually overflows, fixes CJK-aware wrap heights, moves memory extraction off the render thread, adds a live activity bar, and debounces inertial mouse scroll right after a turn completes.",
      "v0.1.99":
        "Runtime-special tool dispatch and small executors now live in a focused runtime_special module, keeping request_permissions, workflow IPC, subagent status, task list/stop, and workflow draft preview behavior intact while shrinking lifecycle.rs.",
      "v0.1.98":
        "Server submit-family dispatch now routes through a focused submit processor, preserving legacy submit, thread-bound turns, thread/start, thread/resume, and thread/fork behavior while leaving the generic router as a pure operation-family dispatcher.",
      "v0.1.97":
        "Server permission/respond dispatch now routes through a focused permission processor, preserving turn/session grants, strict auto-review, filesystem overlays, and network allow/deny behavior while shrinking the generic router.",
      "v0.1.96":
        "Server command/exec dispatch now routes through a focused command-exec processor, preserving buffered, streaming, stdin, resize, terminate, sandbox, and permission-profile behavior while shrinking the generic router.",
      "v0.1.95":
        "Server shell-session dispatch now routes through a focused shell processor, preserving shell start, write, update, close, resize, list, read, and kill behavior while shrinking the generic router.",
      "v0.1.94":
        "Server turn-control dispatch now routes through a focused turn processor, keeping interrupt, resume, and steer behavior intact while shrinking the generic router.",
      "v0.1.93":
        "Synchronous server thread query and metadata operations now route through a focused thread processor, shrinking the generic router while preserving thread/read, list, search, turns, items, and metadata behavior.",
      "v0.1.92":
        "Server-mode operation dispatch now lives behind a focused router boundary, preserving every existing wire method while opening the next request-processor refactor path.",
      "v0.1.91":
        "Runtime permission requests now share one overlay merge path for file-system grants, network domain grants, and strict auto-review, keeping request_permissions and bash retry behavior aligned.",
      "v0.1.90":
        "Model-visible bash now inherits the active permission profile's managed network policy, turns eligible proxy blocks into permission requests, and retries after a turn-scoped network allow.",
      "v0.1.89":
        "Streaming command/exec processes now share the managed network permission flow: eligible proxy blocks request a session-scoped allow, then restart the same processId and stream output after the grant.",
      "v0.1.88":
        "Command/exec can now turn managed network proxy blocks into a network permission request and retry the command after a session-scoped allow response, while denylist blocks remain final diagnostics.",
      "v0.1.87":
        "Managed command/exec network blocks now include the normalized blocked host in proxy diagnostics, giving clients a stable attribution hook for upcoming automatic network permission prompts.",
      "v0.1.86":
        "Session-scoped request_permissions network denials now override permission-profile allow entries, so interactive deny decisions can tighten later command/exec proxy policy.",
      "v0.1.85":
        "Session-scoped request_permissions network domain grants now persist on server threads and feed command/exec's managed proxy, so later commands inherit interactive allowlist decisions.",
      "v0.1.84":
        "Permission-profile Unix socket allowlists now flow into command/exec sandboxing on macOS, allowing configured AF_UNIX socket paths without enabling full network access.",
      "v0.1.83":
        "The managed command/exec network proxy now checks resolved socket addresses before connecting, blocking DNS names that resolve to local, private, reserved, or otherwise non-public targets.",
      "v0.1.82":
        "The managed command/exec network proxy now blocks local and private IP targets unless they are explicitly allowlisted, matching Codex's local-network guard while keeping allowlisted loopback workflows working.",
      "v0.1.81":
        "Permission-profile network blocks now preserve Codex-style proxy reasons, so command/exec clients can distinguish denylist hits from allowlist misses instead of seeing only a generic policy 403.",
      "v0.1.80":
        "The TUI conversation session now owns RuntimeThread instead of rebuilding InteractiveSession and RuntimeSessionLifecycle locally, completing the first headless/server/TUI runtime-state convergence pass while preserving TUI behavior.",
      "v0.1.79":
        "Headless exec now creates and runs long-lived agent state through RuntimeThread, aligning CLI turns with server-mode ownership while preserving JSONL sequencing, session hooks, history, verifier, and npm behavior.",
      "v0.1.78":
        "Server-mode threads now store their long-lived agent state through RuntimeThread, removing duplicated session/lifecycle/executor assembly while preserving thread projection, resume/fork, cancellation, and permission behavior.",
      "v0.1.77":
        "RuntimeThread now groups the runtime-owned interactive session and lifecycle state behind one boundary, creating the next convergence point for server, TUI, and headless execution without changing public behavior.",
      "v0.1.76":
        "The runtime protocol boundary now uses a small facade backed by focused command_exec, events, permissions, shell, thread, turn, and wire modules, preserving the public protocol API while making the next server dispatch split easier.",
      "v0.1.75":
        "ThreadStore now has a focused storage facade backed by separate types, local JSONL, writer, projection, pagination, and live-thread modules, preserving the public runtime API while shrinking the monolithic store file.",
      "v0.1.74":
        "Permission-profile network domain policies now run through a managed loopback HTTP proxy for command/exec, so allowed hosts can pass and denied hosts return a policy 403.",
      "v0.1.73":
        "Permission-profile filesystem globs now support configurable scan depth through glob_scan_max_depth / globScanMaxDepth, with inherited profile defaults and child-profile overrides.",
      "v0.1.72":
        "Permission profiles now expand bounded read/write/read-write filesystem globs into concrete command sandbox roots, keeping Codex-style split filesystem policies usable without weakening broad-glob safety checks.",
      "v0.1.71":
        "Runtime compaction now lives in a dedicated module, keeping prompt-budget hooks, summary persistence, and prompt-too-long recovery out of the lifecycle orchestration module.",
      "v0.1.70":
        "TUI history splits into native terminal scrollback for settled transcript output and a live bottom viewport for streaming content, plans, input, status, and modal/full-panel states.",
      "v0.1.69":
        "Tool-turn execution now lives in a dedicated runtime module, separating provider tool schema/invocation preparation from cursoring, batching, execution, and result folding.",
      "v0.1.68":
        "TUI tool approval gating now lives in the runtime interaction adapter, keeping approval request construction, preview generation, and interactive waits out of bridge orchestration.",
      "v0.1.67":
        "TUI runtime approval and request-user-input handlers now live in a dedicated interaction adapter module, and the site build includes the server prerender entry used by crawler-visible HTML generation.",
      "v0.1.66":
        "TUI runtime event projection now lives in a dedicated module, keeping EventEnvelope-to-TuiEvent mapping and workflow notification prompt shaping out of bridge orchestration.",
      "v0.1.65":
        "Persisted edit and write_file history items now project as Codex-style fileChange items, aligning thread-read history with realtime server streams.",
      "v0.1.64":
        "Persisted commandExecution history items now use shared projection builders while preserving command metadata placeholders and failed-command diagnostics.",
      "v0.1.63":
        "Realtime commandExecution lifecycle items now use shared projection builders, closing another app-server item-shape drift point.",
      "v0.1.62":
        "Realtime agent, plan, and reasoning lifecycle items now use shared projection builders, further tightening the app-server protocol boundary.",
      "v0.1.61":
        "Realtime fileChange and workflow lifecycle items now use shared projection builders, and the tag release gate runs server-heavy Rust tests serially on CI.",
      "v0.1.59":
        "MCP/dynamic completed-item projection is shared across realtime streams and history, and CI stdio MCP fixtures now launch through /bin/sh to avoid Linux ETXTBSY release flakes.",
      "v0.1.58":
        "MCP and dynamic tool completed-item construction now uses shared projection builders across realtime streams and persisted history, with failed command projection guarded against output-shape regression.",
      "v0.1.57":
        "Realtime streams and persisted history now share MCP and dynamic tool started-item builders, keeping first-class tool-call item shape aligned at creation time.",
      "v0.1.56":
        "Realtime and persisted tool item projections now share exit-code error normalization and completed-status checks, reducing the remaining mcpToolCall/dynamicToolCall schema drift.",
      "v0.1.55":
        "Realtime server streams and persisted thread projections now share MCP tool parsing, JSON argument parsing, MCP result shaping, and camelCase tool error helpers, with CI JSONL polling hardened for active background turns.",
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
      "v0.1.123":
        "Runtime turn setup 现在从 lifecycle.rs 移到独立的 runtime_turn_setup 模块。Agent loop 仍通过 RuntimeTurnSetupStep 委托执行，新模块负责 context budget setup、工具审批 policy 构造和 provider config 组合，lifecycle.rs 则继续保留 actor/lifecycle 原语。",
      "v0.1.122":
        "Runtime conversation bootstrap 现在从 lifecycle.rs 移到独立的 runtime_conversation_bootstrap 模块。Agent loop 仍通过 RuntimeConversationBootstrapStep 委托执行，新模块负责 RuntimePreparedConversation、borrowed/owned conversation 存储、session bootstrap 组合和初始 history 记录，lifecycle.rs 则继续保留 actor/lifecycle 原语。",
      "v0.1.121":
        "Runtime steer application 现在从 lifecycle.rs 移到独立的 runtime_steer 模块，并通过分组后的 RuntimeSteerInput 传参。RuntimeTurnOpeningStep 和 RuntimeProviderTurnStep 仍会在模型调用前把待处理 steer input 注入 conversation 和 history，lifecycle.rs 保留 ThreadSteerHandle 存储，同时再拆掉一个 reducer 切片。",
      "v0.1.120":
        "Runtime model-route 编排现在从 lifecycle.rs 移到独立的 runtime_model_route 模块，并通过分组后的 RuntimeModelRouteInput 传参。RuntimeTurnOpeningStep 仍按原顺序组合 compaction、turn start、model routing 和 steering，lifecycle.rs 保留 actor/lifecycle 原语，同时再拆掉一个 reducer 切片，并避免新增长参数调用面。",
      "v0.1.119":
        "Runtime turn-start 编排现在从 lifecycle.rs 移到独立的 runtime_turn_start 模块。RuntimeTurnOpeningStep 仍按原顺序组合 compaction、turn start、model routing 和 steering，lifecycle.rs 则保留 actor/lifecycle 原语，同时再拆掉一个更底层的 reducer 切片。",
      "v0.1.118":
        "Runtime turn-opening 编排现在从 lifecycle.rs 移到独立的 runtime_turn_opening 模块，并通过分组后的 RuntimeTurnOpeningInput 传参。RuntimeTurnIterationStep 仍按原顺序组合 opening 与 provider-cycle 执行，lifecycle.rs 则继续保留更底层的 start/model-route/steer 步骤，同时再少一层 reducer 大小的职责。",
      "v0.1.117":
        "Runtime turn-iteration 编排现在从 lifecycle.rs 移到独立的 runtime_turn_iteration 模块。外层 runtime_turn_loop 仍通过 RuntimeTurnIterationStep 委托执行，provider-cycle 行为仍归 provider_turn，lifecycle.rs 继续保留 opening/start/model-route 这些步骤，同时为下一轮 reducer 风格拆分继续变小。",
      "v0.1.116":
        "Runtime turn-loop 编排现在从 lifecycle.rs 移到独立的 runtime_turn_loop 模块。agent loop 仍然通过 RuntimeTurnLoopStep 委托执行，分组后的 input/executor 对象和 iteration 重试/返回行为保持不变，同时 lifecycle.rs 进一步变小，为后续参考 Codex/package 3 的 reducer 拆分铺路。",
      "v0.1.115":
        "shell-session bash 执行现在统一接收分组后的 RuntimeBashInvocationContext，不再暴露 execute_bash_with_shell_session 的长参数列表。RuntimeNormalToolExecutor 仍然拥有 bash 分支，permission overlay、取消、输出截断、task registry 交接以及网络/文件系统权限重试行为保持不变，同时 bash 边界为后续 shell/session 和 async-subagent 切片继续收窄。",
      "v0.1.114":
        "文件系统 sandbox 拒绝现在在 server command/exec 和模型可见 bash 两条路径上都能更清楚地恢复。Orca 会诊断 macOS Seatbelt 写入阻断，例如嵌套工作区里的 .git/index.lock 失败，并说明这通常是 sandbox 范围问题而不是 stale lock；当存在审批处理器时，会请求 turn-scoped 文件系统写入授权，并用授权后的 root 重试原命令。",
      "v0.1.113":
        "工具回合 dispatch 现在从 provider response 处理处接收分组后的 RuntimeToolTurnsContext，不再暴露 run_tool_turns 的长参数调用。RuntimeStepContext、events、sink、conversation、history writer、tool requests、cost tracking、background workflow 状态和 child executors 的透传保持不变，同时 provider 到 tool-turn 的边界继续收窄。",
      "v0.1.112":
        "普通工具回合执行现在统一接收分组后的 RuntimeNormalToolTurnContext，不再让 run_normal_tool_turn 暴露长参数列表。工具执行、审批、结果记录、plan-state 记录、permission overlay、workflow/background 状态以及 child executor 交接都保持相同 runtime 行为，同时 tool-turn 边界继续收窄。",
      "v0.1.111":
        "工具审批 gate 的输入现在统一通过 ToolApprovalGateContext 传递，不再让 handle_approval 暴露长参数列表。config、events、sink、tool request、invocation、policy、strict auto-review 与 delta emission 的透传保持不变，同时 approval allow/ask/deny 行为和 tool-call item emission 继续保持相同的公开形状。",
      "v0.1.110":
        "历史投影工具完成态现在统一由 tool_item_projection.rs 里的 complete_projected_tool_item 重建，不再让 thread_store/projection.rs 直接调用 MCP、dynamic、commandExecution 和 fileChange completed-item 构造器。实时流和持久化 history 行为保持兼容，同时剩余 tool-item schema drift 又少了一个分散所有权点。",
      "v0.1.109":
        "runtime 普通工具路由现在从 router 向 lifecycle actor 传递分组后的 RuntimeNormalToolInvocation，不再直接调用带 roots/cancel 的长参数方法。bash shell-session、MCP/external fallback、permission overlay、取消和输出截断行为保持不变，同时公共工具路径为后续 shell 与 async-subagent 工作留下更窄的调用面。",
      "v0.1.108":
        "普通工具 invocation 现在统一经过 runtime_normal_tool 里的单一 helper，不再让 lifecycle.rs 直接实例化 executor。RuntimeTaskActor 和 RuntimeToolActorContext 仍保持同样的 bash、MCP、external、取消与 permission-overlay 行为，但后续 shell-session 和 async subagent 切片会有更窄的调用面可继续推进。",
      "v0.1.107":
        "工具调用参数的流式接收现在支持端到端进度上报：新增 tool.call.progress 事件与 ToolCallProgress provider 步骤，贯穿 runtime 与 server，TUI 以缓存友好的方式展示已接收字节进度。同时新增 SSE 流式空闲超时保护，并修复环境变量代理配置与 hook 超时输出处理的问题。",
      "v0.1.106":
        "普通工具 fallback 路径现在通过独立 RuntimeNormalToolFallbackExecutor 边界注入。MCP、TOML external 和 built-in 工具仍然走默认 orca-tools 实现，但 runtime 已经可以直接测试 fallback context 的透传，不再把具体实现硬编码在执行器里。",
      "v0.1.105":
        "普通工具执行现在进入独立 RuntimeNormalToolExecutor 边界。shell-session bash 分支，以及 MCP/external/built-in fallback 路径都从 lifecycle.rs 移出，同时 CLI、TUI、server、workflow、permission 与模型可见工具行为保持不变。",
      "v0.1.104":
        "runtime tool invocation dispatch 现在进入独立 RuntimeToolRouter 边界。ToolExecutionActor 只保留 invocation 准备、审批、hook 与结果收尾；workflow、subagent、task、permission、workflow IPC 和普通工具路由都移到 router，模型可见行为保持不变。",
      "v0.1.103":
        "runtime turn 执行现在使用更清晰的分组输入边界：turn iteration、provider cycle、provider response 与 tool turns 共享 request-scoped context。这个参考 Codex/package 3 的架构切片减少了重复的 runtime 状态传递，同时保持 CLI、TUI、server、tool、workflow 与 history 行为不变。",
      "v0.1.102":
        "TUI child-agent 执行现在通过 runtime 统一负责 request 构造、model/cost setup、loop 编排、provider 处理、tool request 提取与 tool-result folding；TUI 只保留交互式 tool adapter，并且新的 reasoning-effort 配置会继续传入 child provider 调用。",
      "v0.1.101":
        "推理强度现在可配置（high 或 max，默认 max），支持通过环境变量、配置文件和 CLI 参数设置，并在 DeepSeek API 请求中携带。TUI 的 /model 命令改为两步选择——先选模型，再选推理强度——选择过程中不立即应用，按 Esc 可完整取消，状态栏同时显示模型与推理强度。",
      "v0.1.100":
        "TUI 体验优化：inline 滚动现在通过 rendered-line-info 判断真实溢出，内容未溢出时保持自动跟随，修复 CJK 混排的换行高度计算，将内存提取移出渲染线程，新增实时活动指示栏，并在回合结束后对惯性鼠标滚动做防抖处理。",
      "v0.1.99":
        "runtime-special 工具分发和小型 executor 现在进入独立 runtime_special 模块，保持 request_permissions、workflow IPC、subagent status、task list/stop、workflow draft preview 行为不变，同时缩小 lifecycle.rs。",
      "v0.1.98":
        "server 的 submit-family dispatch 现在进入独立 submit processor，保持 legacy submit、thread-bound turn、thread/start、thread/resume、thread/fork 行为不变，同时让通用 router 只保留 operation-family 分发职责。",
      "v0.1.97":
        "server 的 permission/respond dispatch 现在进入独立 permission processor，保持 turn/session 授权、strict auto-review、文件系统 overlay 与网络 allow/deny 行为不变，同时继续缩小通用 router。",
      "v0.1.96":
        "server 的 command/exec dispatch 现在进入独立 command-exec processor，保持 buffered、streaming、stdin、resize、terminate、sandbox 与 permission-profile 行为不变，同时继续缩小通用 router。",
      "v0.1.95":
        "server 的 shell-session dispatch 现在进入独立 shell processor，保持 start、write、update、close、resize、list、read、kill 行为不变，同时继续缩小通用 router。",
      "v0.1.94":
        "server 的 turn-control dispatch 现在进入独立 turn processor，保持 interrupt、resume、steer 行为不变，同时继续缩小通用 router。",
      "v0.1.93":
        "server 里的同步 thread 查询和 metadata 操作现在进入独立 thread processor，缩小通用 router，同时保持 thread/read、list、search、turns、items 和 metadata 行为不变。",
      "v0.1.92":
        "server 模式的 operation dispatch 现在进入独立 router 边界，在保持所有现有 wire method 不变的同时，为后续 request processor 重构铺路。",
      "v0.1.91":
        "runtime 权限请求现在统一走同一个 overlay 合并路径，覆盖文件系统授权、网络域名授权和 strict auto-review，让 request_permissions 与 bash 重试行为保持一致。",
      "v0.1.90":
        "模型可见 bash 现在会继承 active permission profile 的托管网络策略：符合条件的代理阻断会转成权限请求，并在 turn 级网络 allow 后重试。",
      "v0.1.89":
        "streaming command/exec 进程现在也接入托管网络权限流：符合条件的代理阻断会请求 session 级 allow，授权后用同一个 processId 重启并继续流式输出。",
      "v0.1.88":
        "command/exec 现在可以把托管网络代理阻断转成网络权限请求，并在收到 session 级 allow 后重试命令；denylist 阻断仍保持为最终诊断。",
      "v0.1.87":
        "command/exec 托管网络代理的阻断诊断现在会包含规范化后的被拦截 host，为后续自动网络权限提示提供稳定归因点。",
      "v0.1.86":
        "session 级 request_permissions 网络拒绝现在会覆盖 permission profile 的 allow 条目，让交互式 deny 决策能收紧后续 command/exec 的代理策略。",
      "v0.1.85":
        "session 级 request_permissions 网络域名授权现在会持久化到 server thread，并传入 command/exec 的托管代理，让后续命令继承交互式 allowlist 决策。",
      "v0.1.84":
        "permission profile 中的 Unix socket allowlist 现在会传入 macOS command/exec 沙箱，允许显式配置的 AF_UNIX socket 路径，同时不需要开启完整网络访问。",
      "v0.1.83":
        "command/exec 的托管网络代理现在会在连接前检查 DNS 解析后的 socket 地址，阻止解析到本地、私网、保留地址或其他非公网目标的域名。",
      "v0.1.82":
        "command/exec 的托管网络代理现在默认阻止本地和私网 IP 目标，除非显式 allowlist；这对齐 Codex 的 local-network guard，同时保留已 allowlist 的 loopback 工作流。",
      "v0.1.81":
        "权限 profile 的网络拦截现在保留 Codex 风格的 proxy reason，command/exec 客户端可以区分 denylist 命中和 allowlist 未命中，而不是只看到泛化的 policy 403。",
      "v0.1.80":
        "TUI conversation session 现在直接拥有 RuntimeThread，不再本地重建 InteractiveSession 和 RuntimeSessionLifecycle，完成 headless/server/TUI 第一轮 runtime state ownership 收敛，同时保持 TUI 行为不变。",
      "v0.1.79":
        "Headless exec 现在也通过 RuntimeThread 创建并运行长期 agent state，让 CLI turn 与 server-mode 共享同一所有权边界，同时保留 JSONL 顺序、session hook、history、verifier 和 npm 行为。",
      "v0.1.78":
        "Server-mode thread 现在通过 RuntimeThread 保存长期 agent state，不再重复拼 session/lifecycle/executor，同时保持 thread projection、resume/fork、cancel 和权限行为不变。",
      "v0.1.77":
        "RuntimeThread 现在把 runtime-owned interactive session 和 lifecycle state 收到同一个边界里，为 server、TUI、headless 后续收敛提供新的承载点，同时不改变公开行为。",
      "v0.1.76":
        "Runtime protocol 边界现在变成小 facade，并由 command_exec、events、permissions、shell、thread、turn、wire 等专门模块支撑；公开 protocol API 保持不变，同时为下一步拆 server dispatch 铺路。",
      "v0.1.75":
        "ThreadStore 现在拆成清晰的存储 facade：types、local JSONL、writer、projection、pagination 和 live-thread 各自成模块，在保持公开 runtime API 不变的同时拆掉原来的巨型 store 文件。",
      "v0.1.74":
        "权限 profile 的 network domain policy 现在会通过 command/exec 的本地 HTTP 代理执行：允许的 host 可访问，被 deny 的 host 返回 policy 403。",
      "v0.1.73":
        "权限 profile 的文件系统 glob 现在支持通过 glob_scan_max_depth / globScanMaxDepth 配置扫描深度，并支持父 profile 默认值与子 profile 覆盖。",
      "v0.1.72":
        "权限 profile 现在会把有界 read/write/read-write 文件系统 glob 展开成具体 command sandbox roots，在保留过宽 glob 安全拒绝的同时补齐 Codex 风格 split filesystem policy。",
      "v0.1.71":
        "Runtime compaction 现在迁到专门模块，prompt budget hooks、summary 持久化和 prompt-too-long 恢复不再混在 lifecycle 编排里。",
      "v0.1.70":
        "TUI 历史拆成两层：已定稿 transcript 输出进入终端原生 scrollback，底部 live viewport 保留流式内容、计划、输入框、状态栏和模态/全屏面板。",
      "v0.1.69":
        "Tool-turn 执行现在迁到专门的 runtime 模块，provider 工具 schema / invocation 准备与游标、批处理、执行、结果折叠边界分开。",
      "v0.1.68":
        "TUI tool approval gate 现在由 runtime interaction adapter 负责，`bridge` 不再直接持有 approval request 构造、preview 生成和交互等待逻辑。",
      "v0.1.67":
        "TUI runtime approval 和 request_user_input handler 现在迁到专门的 interaction adapter 模块；站点构建也补齐了用于生成爬虫可见 HTML 的 server prerender entry。",
      "v0.1.66":
        "TUI runtime event projection 现在迁到专门模块，`bridge` 不再直接持有 EventEnvelope 到 TuiEvent 的映射和 workflow notification prompt 组装。",
      "v0.1.65":
        "持久化 edit / write_file history item 现在投影为 Codex 风格 fileChange item，让 thread-read 历史与实时 server stream 保持一致。",
      "v0.1.64":
        "持久化 commandExecution history item 现在也由共享 projection builder 构造，同时保留命令元数据占位字段和失败命令诊断语义。",
      "v0.1.63":
        "实时 commandExecution lifecycle item 现在也由共享 projection builder 构造，继续消除 app-server item shape 漂移点。",
      "v0.1.62":
        "实时 agent / plan / reasoning lifecycle item 现在也由共享 projection builder 构造，继续收紧 app-server protocol 边界。",
      "v0.1.61":
        "实时 fileChange / workflow lifecycle item 现在由共享 projection builder 构造，tag 发布关口也会在 CI 串行运行 server-heavy Rust 测试。",
      "v0.1.59":
        "MCP / dynamic completed-item projection 已在实时 stream 与 history 间共享；CI stdio MCP fixture 改为通过 /bin/sh 启动，避开 Linux ETXTBSY 发布抖动。",
      "v0.1.58":
        "MCP / dynamic tool completed-item 构造现在由实时 stream 与持久化 history 共享 projection builder，并补上失败 command projection 的输出形状回归守卫。",
      "v0.1.57":
        "实时 stream 与持久化 history 现在共享 MCP / dynamic tool started-item builder，让一等工具调用 item 从创建阶段就保持形状一致。",
      "v0.1.56":
        "实时与持久化 tool item projection 现在共享 exit-code 错误归一化和 completed 状态检查，继续减少 mcpToolCall / dynamicToolCall 的 schema drift。",
      "v0.1.55":
        "实时 server stream 与持久化 thread projection 现在共享 MCP tool 解析、JSON 参数解析、MCP result shaping 和 camelCase tool error helper，并加固后台 turn 活跃写入时的 CI JSONL 轮询测试。",
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

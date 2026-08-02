# Orca 架构、性能与设计全面审查报告

- **日期**:2026-08-03
- **审查基线**:main @ `c8d069c57`(v0.3.1,含会话生命周期命令)
- **方法**:5 个维度并行评审(架构分层 / 并发模型 / TUI / 端到端性能 / v0.3.1 会话生命周期改动),关键结论经主审交叉验证。所有 file:line 为审查时点的引用,后续重构会漂移。
- **范围**:只读审查,未修改任何生产代码。

---

## 总评

**这个代码库的底子明显好于它的第一印象。** 表面上"20 万行的 runtime、4.8 万行的单文件"很吓人,但大量行数是测试(orca-tui 的 app.rs 生产代码只占 12%;`acp/supervisor.rs` 是 2.4k 生产 + 4.1k 测试);并发纪律、流式增量渲染、transcript 虚拟化、shell/provider 两条取消路径的核心实现都相当好。

真正的问题集中在四处:

1. **两个能让整个进程卡死的并发缺陷**(goal actor 无超时阻塞、supervisor 异步循环里跑同步 IO);
2. **流式热路径上的一个 O(N²)**(reducer 每 delta 全量重拷累积文本,且 runtime/TUI 两侧各跑一遍);
3. **一组"做了一半"的架构债**(ThreadActor god object、源码文本断言测试、provider→tools 分层倒置、surface 门面被绕过);
4. **随 v0.3.1 发布的 5 个功能缺陷**(picker fork 转录残留、manifest 契约漂移、rename 标题回滚等)。

---

## 一、需要立即处理的缺陷

### 1.1 goal actor 无超时阻塞可挂死整个 host 【Critical｜并发】

`goal_actor.rs:1535-1545` 的 `reply_rx.recv()` 无超时;发送侧 `mpsc::sync_channel`(`goal_actor.rs:766`)有界,满时 `send()` 同样阻塞。而这条路径被 ThreadActor 的 **async 事件循环**同步调用(`runtime_host.rs:11917`、`:12295`、`:30337`,经 `mutate_goal_surface` → `ThreadActor::run` 的 select! 分支)。

host 只有一个 OS 线程驱动 runtime(`runtime_host.rs:3439`),ThreadActor 是单 task 串行事件循环。goal actor 线程一旦卡在 SQLite 的 5 秒 `busy_timeout`(`goal_store.rs:4186`),该会话**所有命令——包括 Esc 取消——全部失灵**。

**修复**:`request` 改 `recv_timeout` + 显式错误;更根本地把 goal store 访问移入 `spawn_blocking`,或让 goal actor 走 tokio channel 以便 `.await`。

### 1.2 supervisor 异步循环里直接跑磁盘/SQLite IO 【Critical｜并发】

`runtime_host.rs` 中 supervisor 的 7 处 store 调用**未包 `spawn_blocking`**:`:4358` `list_threads`、`:4377` `search_threads`、`:4394` `read_thread`、`:4405`、`:4422`、`:4436`、`:4244` `update_thread_metadata`。同函数 `:4186`/`:4220` 正确使用了 `spawn_blocking`,说明是遗漏而非设计。

代价不是轻量的:`list_threads` 调 `list_sessions_with_archived(usize::MAX, ..)`(`thread_store/local.rs:1035`),底层 `fs::read_dir` + 逐文件 `File::open` 解析,**无条数上限**;`GoalStore::load_default()`(`goal_store.rs:418`)每次都 `create_dir_all` + schema 初始化 + legacy 迁移检查。

**触发场景**:历史积累数千会话后打开 session picker,supervisor(全局单点)被同步 IO 占住,**所有会话的命令派发一起停顿**;SQLite 争用时单次可阻塞 5 秒。

**修复**:7 处统一包 `spawn_blocking`;`usize::MAX` 改按 cursor 分页。

### 1.3 流式 reducer 每 delta 全量重拷累积文本 —— O(N²) 【High｜性能】

`runtime_surface/reducer.rs:3622`:

```rust
stream.text = DisplayText::new(format!("{}{}", stream.text.as_str(), text.as_str()));
```

每个 delta 都把已累积的全部文本重新分配 + 拷贝一次。事件载荷本身是带 offset 的真增量(`AssistantPatch::Delta { stream_id, offset, text }`,`runtime_surface/projection.rs:302`),协议没问题,坏在归约实现。

且这段 reduce 在 **runtime 侧和 TUI 侧投影各跑一遍**(TUI 的 `surface_projection.rs` 持有 `reducer_state` 副本并对每个 batch 重放)。一个 100KB 的长回复(大段代码输出)按几百个 delta 计,总拷贝量达 GB 量级,发生在 ThreadActor 事件循环与 TUI 事件线程两个单点上。

**修复**:`DisplayText` 提供追加语义(内部 `push_str`),或流式期间用可变缓冲、完成时一次性冻结。

### 1.4 Esc 取消覆盖有两个洞 【High｜并发】

- **`WaitForTerminal` 语义工具启动后不可取消**:`runtime_tool_call.rs:680-684`,工具进入 STARTED 后仅 `CooperativeCancel` 才传递取消;20 个内置工具中只有 bash 是 cooperative(`orca-tools/src/registry.rs:606`),`web_search` 是 25 秒硬超时 + `reqwest::blocking`(`orca-tools/src/web_search.rs:82/142/146`),完全不接受取消。**症状:web_search 期间按 Esc,UI 最长干等 25 秒。**
- **Esc 不触达已 spawn 的异步 subagent 进程**:前台取消只调 `generation.cancel`(`runtime_host.rs:31719`),不调 `task_registry.request_stop()`;真正 kill 子进程的 `terminate_worker`(`tasks.rs:1400`)只由 `task_stop` 工具、workflow 停止和 surface host 触发。**症状:Esc 后后台 subagent 继续烧 API、继续改文件。** 若这是有意设计(后台任务不受前台 Esc 影响),应在 UI 明示;若否,是取消漏洞。

**修复方向**:为 conservative 工具增加"放弃等待"路径(结果丢弃、后台自然收尾),UI 立即回 idle;明确 Esc 对后台任务的语义并实现或文档化。

### 1.5 surface 契约校验器形同虚设 【High｜流程】

`scripts/validate-runtime-surface-contract.mjs`(约 3.6k 行)构建得很认真,但:

- **没有被任何 CI workflow 引用**(`.github/workflows/` 只有 npm-token-check / pages / release / verify-release / windows-ci,且 windows-ci 的测试过滤器不含 orca-tui 边界测试);
- **在 HEAD 上本来就跑不过**:`af99db763` 改了 manifest 但 commit message 缺 `private-sha256` trailer,校验器直接 fail;
- Rust 侧 `surface_boundary_tests.rs:423` 只对比 manifest 表行与硬编码的 `CURRENT_ACTIONS`,**从不读 `closed_inventory`** —— 两套校验对象不同,可以静默发散(v0.3.1 的 manifest 漂移正是这样漏掉的,见 1.6)。

**修复**:把校验器和 orca-tui 边界测试接进 CI;先解决 sha256 trailer 问题。这条防线要么真实存在,要么承认不存在。

### 1.6 随 v0.3.1 发布的会话生命周期缺陷 【发布时为 2 Critical + 3 High,均已复核证实未修】

| # | 缺陷 | 证据 | 症状 |
|---|---|---|---|
| C1 | 从 picker fork 另一会话后**旧会话转录残留** | `app.rs:7812` `ForkSavedSession` 分支只发 `SessionForked` 不发历史快照;该事件处理器(`types.rs:2386`)从不清空消息 | 屏幕内容与实际所在会话不符。`ResumeSavedSession` 分支有 `emit_typed_history_snapshot`,fork 分支应对称补齐;注意 `/fork`(当前会话)按设计保留转录,两条路径不该共用同一事件语义 |
| C2 | manifest `closed_inventory` 漂移 | `tui_actions` 中 current 行 30 个,`closed_inventory.current_tui_user_actions` 仅 23 个,缺的恰是 7 个新 action(Fork/Rename/Resume/Archive/Delete 系列) | JS 校验器 `assertExactArray` 会 fail(接 CI 后会红) |
| H1 | `/rename` 只写磁盘、不打 runtime surface patch | `TuiSurfaceActions` 无 `rename_current_session`;orca-tui 中 `UpdateSessionMetadata`/`SessionMetadataPatch` 出现 0 次,而 runtime 侧 API 存在(`runtime_surface/commands.rs:3494`)。设计文档明文要求带 revision 前置条件的 surface patch | rename 后任何 `announce_runtime_ready`(`app.rs:8595`,从 `snapshot.thread.title` 读)都会把标题**回滚成旧值**,磁盘与内存永久不一致 |
| H2 | picker 对当前会话仍提供 Archive/Delete | 设计说"not offered",实现只在 controller 兜底拒绝;`session_picker_actions.rs` 无任何 `current_session_id` 引用 | 用户走完破坏性确认流程才被拒——训练用户忽视确认框。UI 层应置灰,controller 兜底保留(防 TOCTOU) |
| H3 | 新增 picker 四相位渲染与 `/status` 零测试 | plan Task 4 Step 6 要求 80x24/44x18 双尺寸断言,未做;`tests/history_contract.rs` 完全未改,archive/delete 持久化零覆盖 | `session_picker_hit_index`(`ui.rs:3638`)的行号计算与非 Browsing 相位实际渲染已不一致,靠早退规避,脆弱 |

**Medium 级遗留**:`/status` 缺 Git identity/百分比/workflow 计数三个设计要求的字段;`/fork` 无名默认标题硬编码 `"Forked conversation"`(picker 路径是 `Fork of {源标题}`,两条路径不一致);picker 里管理**其他**会话的操作会向**当前**会话转录插入系统消息(若持久化则语义错误);`Renaming` 相位丢了设计中的 `cursor` 字段(光标只能在尾部);`/status` 的 approval mode 读 `config` 而非 `state`,`/mode` 切换后可能显示错误。

**已知限制(可暂记录不修)**:事件 channel 无会话代际标记,切换会话时旧会话已入队的后台投影事件(`WorkflowTasksUpdated`、`TaskUpdate`)会在新会话事件前被消费。`ensure_current_session_switchable` 保证切换时无活跃前台操作,所以不会有 delta/tool 事件残留,但后台投影类事件可能。设计文档 "Failure And Concurrency Invariants" 一节对此是空白。

**做对的部分**(避免误伤):start-new-then-shutdown-old 的失败回滚正确;活跃工作六类检查全覆盖;picker 相位捕获 session_id 防 TOCTOU 且有测试;rename 与并发写靠文件锁 + 原子重写不会撕裂 JSONL。

---

## 二、架构

### 2.1 `ThreadActor` 是真正的 god object 【Critical】

- `runtime_host.rs:8554` `struct ThreadActor`,25 个字段;`:11091-34796` 单个 impl 块 **23,705 行、220 个方法**。对比:`RuntimeHost` 本体只有 3 个字段、129 行 impl。
- 25 个字段中 8 个是 `pending_*` map(`:8567-8580`),每个都是独立状态机,却共享同一个 `&mut self` —— 任何一个状态机的不变量都无法在类型层面隔离验证。这是 ADR-0005 想解决的"多 owner"问题在单类型内部的复发。
- 方法域词频:`surface` 相关 137、`terminal` 35、`goal` 31、`background` 21、`provider` 19、`capability` 17、`interaction` 16、`workflow` 12。

**拆分缝隙(按字段聚类,全部沿现有类型边界,不需发明新抽象)**:

1. capability/terminal 子状态机(52 个方法 + `resident_surface` 字段)→ `runtime_host/capability.rs`,以 `&mut ResidentSurfaceSlot` 为接收者;
2. goal 子状态机(31 个方法 + `pending_goal_completion_recovery`)→ 下沉到已存在的 `goal_actor.rs`/`goal_store.rs`;
3. background/task(21 个方法 + 4 个字段)→ 内聚为 `BackgroundTaskSet`;
4. commit 编排(24 个方法,基本是 `runtime_surface::commit` 的调用包装)→ 移出为 free function。

四刀能把 impl 从 23.7k 降到约 8k。

### 2.2 源码文本断言测试锁死重构 【Critical,是 2.1 的前置障碍】

`crates/orca-runtime/src/lib.rs` 含 **254 个 `include_str!`** + `contains` 源码形状断言(5,849 行中 5,700 行是测试);TUI 侧另有约 10 个同类(如 `surface_boundary_tests.rs:506` 按 `fn {method}` 子串匹配)。ADR-0005(`docs/architecture/adr/0005:21-24`)已明确承认这类测试"能在不证明任何行为的情况下固化过时的命名与文件布局"。

它锁定的是**语法**而非**行为**:断言 `contains(".with_permission_overlay(...)")` 通过,不代表调用在正确分支上。每动一次 ThreadActor 就要修一批文本断言,重构成本人为翻倍。

**修复**:逐个分类——表达真实不变量的改写为编译期约束(可见性收紧、只接受借用的构造),纯描述文件布局的直接删除。ADR-0005 已授权,只是没做完。

### 2.3 `orca-provider → orca-tools` 分层倒置 【High】

- `orca-provider/Cargo.toml:10` 依赖 orca-tools;`tool_schema.rs:8` 用 `ToolRegistry`;`system_prompt.rs:156-166` 遍历工具注册表拼系统提示词;**`deepseek_http.rs:773-778` HTTP 传输层直接引用 `update_plan`/`update_goal` 两个具体工具的参数归一化**。
- 后果:每加一个需归一化的工具就要改 provider;provider 传递依赖了 MCP(orca-tools → orca-mcp);接第二个 LLM 后端时工具 schema 生成被困在 DeepSeek 专属 crate 里。
- 真正的消费者在 runtime(`tool_invocation.rs:11-15`、`agent_common.rs:41`)—— runtime 同时依赖 provider 和 tools,却让 provider 去桥接两者。

**修复**:`tool_schema.rs`/`system_prompt.rs` 移入 runtime(或新叶子 crate);归一化在 `Tool` trait 上加 `normalize_raw_arguments` 方法(`registry.rs:39` 已有同级的 `schema()`),provider 只调 trait 不认识工具名。完成后 provider 依赖从 4 降到 2。

### 2.4 server 层绕过 `surface` 门面 【High】

- `lib.rs:60-117` 手工策展的 `pub mod surface`(约 150 类型)旁边,`:142-144` 开着 `pub mod unstable_surface { pub use crate::runtime_surface::*; }` 全量 glob。
- 用量:`server/surface_adapter.rs` 94 处、`acp/supervisor.rs` 85 处走 `unstable_surface`;走正门 `crate::surface` 的只有 3 个文件。门面退化为纯装饰,`runtime_surface` 里任何 pub 项都事实上成了 server 的 API。
- 叠加 `runtime_surface/mod.rs:13-23` 的 11 行连续 `pub use xxx::*;`(共 38k 行代码互相无约束穿透),编译器无法报告跨模块引用,"文件拆了但耦合还在"。

**修复**:server 的 94 处引用逐一分类,契约类型补进 `surface` 白名单,内部细节走 `surface_adapter` 窄接口;`mod.rs` glob 改显式导出(不改逻辑,立刻暴露真实依赖图);收敛后删除 `unstable_surface`。

### 2.5 其余架构发现

- **【High,零成本】orca-tui 声明 `orca-provider` 依赖但全 crate 零引用**(`orca-tui/Cargo.toml:14`,`grep orca_provider` 无结果)。删一行,架构图不再说谎。
- **【Medium】TUI 持有 `McpRegistry` 双 owner**:`app.rs:258` TUI 构造 registry 再传给 runtime,20+ 处函数签名传参;而 elicitation 路由在 runtime 侧处理。应移入 RuntimeHost,TUI 经 surface 拿只读快照。skills 发现在 TUI 四处独立调用 `discover_from_env`,无缓存。
- **【Medium】`runtime_surface/commit.rs` 的授权判定散落成 15+ 个自由函数**(`:4699-6520` 的 `*_authorized` 系列),同一概念 15 份平行实现,无单一审计点。建议收敛为单入口 `authorize(request, state)`。
- **【Low】`protocol/events.rs:125-190` 对外契约事件的字段全是 `serde_json::Value`**(`docs/harness-contract.md` 定义为对外契约),`exit_code` 写成字符串不会被编译器发现。可渐进补类型。
- **澄清**:`protocol.rs`+`protocol/`、`server.rs`+`server/` 是 Rust 2018+ 标准模块布局,不是重复抽象,无需改。

### 2.6 架构上做对的

- orca-core/orca-platform/orca-approval/orca-file-search/orca-windows-* 共 5+ 个叶子 crate 分层干净;
- `runtime_turn_*` 六连文件(92-463 行)是设计过的 step pipeline,每文件一个 `RuntimeTurn*Step` 类型 + 动词方法,恰是 ThreadActor 该学的样子;`step_context.rs` 被 5 模块共享是正常依赖注入;
- `pub mod surface` 门面设计意图正确(acp 层证明可用),问题只在覆盖执行;
- 大文件多半是测试:`commit.rs` 生产 9.7k、`reducer.rs` 8.0k、`supervisor.rs` 生产仅 2.4k —— 真正的异常值只有 `runtime_host.rs` 的 35.7k;
- ADR 机制真实在运转,代码与 ADR-0005 方向一致,偏离在执行深度。

---

## 三、并发模型

### 3.1 执行模型(评审确认为刻意设计,自洽)

同步核心 + 极薄 async 外壳:调用方经 tokio mpsc(cap=16)→ 唯一 OS 线程 "orca-runtime-host"(`runtime_host.rs:3439`)内 `block_on` 多线程 tokio → 每会话一个 `ThreadActor` 单 task 串行事件循环(`:28894`)→ 每 turn `spawn_blocking`(`:31746`)把**整个 `ThreadActorState` move 进去**同步执行,结束 move 回来。

**以所有权换互斥**:state 在 actor task 与 turn blocking task 间 move 传递而非 `Arc<Mutex>` 共享,从根本消除锁竞争;配合 `catch_unwind`(仅 3 处,全在操作边界)把 panic 转 `GenerationTaskOutcome::Panicked` 且 state 原路返回 —— panic 也不丢状态。这是全库最漂亮的设计。

### 3.2 发现(除一节已列的 Critical/High 外)

- **【Medium】全局 `GOAL_RUNTIME_LEASES` Mutex 持锁期间做文件锁 + SQLite 写**(`goal_actor.rs:161-181`)——全库唯一违反"临界区不跨 IO"处;SQLite 卡 5 秒时所有会话的 goal 初始化一起排队。先锁外取资源再进锁插入。
- **【Medium】后台 workflow 取消无 deadline**(`workflow_execution.rs:626-636`:请求 stop 后 20ms 轮询 `is_finished`,无超时无强杀);脚本卡在检查点之外时取消永久转圈。
- **【Low】审批等待 25ms 轮询**(`runtime_host.rs:1438-1454`):语义正确但每个待审批 turn 占一个 blocking 线程 40Hz 空转,审批常持续数分钟。可改 Condvar 唤醒。
- **【Low】`runtime_tool_actor.rs:207` 的 `fallback_cancel`**:`Option<&CancelToken>` 让"忘传取消令牌"静默失去取消能力,建议改必传。

### 3.3 并发上做对的

锁纪律优秀(几乎无嵌套持锁,中毒锁统一 `into_inner()` 恢复);三种 channel 分工全部有理(tokio mpsc 做 actor 邮箱、std sync_channel 做同步侧等回执、crossbeam 仅用于 workflow 多消费者队列);线程回收纪律严明(shell reader 三条路径都 stop+join、actor 经 `ExitNotifier` 确定性 join、唯一 detach 线程有确定退出条件);**取消在代价最高的两条路径上真正干净**——shell 进程树(进程组 SIGTERM→SIGKILL,上限约 225ms,孙进程收得掉)与 provider HTTP 流(token 多点检查 + Drop 兜底,有测试断言连接关闭)。

存疑未决:Windows 下 `terminate_child_tree` 不调 `child.kill()` 只依赖 Job Object,若某些 spawn 分支未进 Job,孙进程可能逃逸(未读完全部 Windows 分支)。

---

## 四、TUI

### 4.1 规模的真相

app.rs 9,431 行中生产代码 **1,117 行**(88% 是测试);types.rs 3,487/8,657;ui.rs 4,351/9,284。"67k 行 TUI"实际约 30k 生产代码,测试密度在 TUI 项目里罕见,是优点。真正偏大的只有 ui.rs 与 types.rs,后者的问题是**事件归约器和状态定义同文件**(拆出 `app_state_reducer.rs` 可砍半),不是类型垃圾场。

### 4.2 发现

- **【High】resize/主题切换全量重排悬崖**:`transcript_view.rs:644-649` 宽度或主题一变,全部消息进 dirty 集合,渲染线程同步重换行 + 重高亮 + 重解析 diff。长会话拖窗口边缘肉眼可见卡顿。→ resize 去抖或只重建可视窗口。
- **【High】搜索每键全量重扫 + 流式时每帧重扫**:`transcript_search.rs:176/203` 每键 `invalidate_prepared()`;`ui.rs:814` 每帧无条件 `refresh_transcript_search()`,而流式重建会 bump `content_generation` → 搜索框开着 + 流式输出 = 每帧 O(全文) 重扫。→ 输入去抖 + 增量收窄。
- **【High】`push_message` 对每个 ToolCall 全表线性扫描**(`types.rs:1611`),整场会话累计 O(n²),agent 场景工具调用密集,长会话持续变慢。→ `HashMap<tool_id, index>`,十几行修掉。
- **【Medium】状态三层重复**:`surface_projection.rs:166-176` 同时持有影子字段和 `reducer_state`(内容重叠),`reduce_typed_batch` 并行跑两套且无一致性断言;goal 存三份、经有损映射手工同步。是最大的长期维护负债 —— 建议先补一致性断言测试锁住风险,再择期收敛。
- **【Medium】usage 用 `.max()` 防乱序**(`types.rs:2912-2917`)→ `/compact` 后 token 计数永不回落,用户以为压缩没生效。→ 按事件 ordinal 判序后直接赋值。
- **【Medium】runtime 事件映射 `_ => None` 兜底**(`runtime_event_projection.rs:268`),新事件静默不显示;与 `UserAction` 侧闭集校验的保护力度不对称。→ 穷举匹配。
- **【Medium】语法高亮无内容级缓存、diff 重建即重解析**(`syntax_highlight.rs:158`、`diff_highlight.rs:1392`)——稳态被 transcript 缓存挡住,但正是 resize 悬崖的成本来源,与上面第一条修一个即可显著缓解。

### 4.3 TUI 上做对的

transcript 真虚拟化(二分定位可见首行、只 materialize 可见区间、滚动 O(1),有 5000/40000 条消息的测试守着);流式渲染真增量(按换行缓冲、块冻结后永不重建、围栏代码块只高亮一次);缓存全部有界且键设计正确;协作式时间预算(diff 5ms deadline、高亮三道输入上限);输入路径干净(全 channel 有界、空闲零轮询、防饥饿有专门测试;空闲态延迟约 2-6ms、流式态 8-10ms);spinner 原地打补丁近零成本;边界有防漂移设计(cursor gap 检测、RAII 取消孤儿 operation)。

action 分发架构评审结论:**偏清晰,不是意外复杂度**。双轴拆分(UI mode × 事件类型),dispatcher 是独立线程上的路由器而非巨型 match;新增 `/new`/`/clear` 实测碰 11 个文件 +496 行,其中大半是测试与契约同步 —— 闭集契约的固有代价,换来漏改会被拦(前提是校验器接进 CI)。

---

## 五、性能(除 1.3 的 O(N²) 外)

**逐项排查后确认没有问题的**(避免后人重查):

- token 计数(tiktoken cl100k)只在每 turn 开始与压缩决策时全量跑(`runtime_turn_opening.rs:68`、`compaction.rs:241/298`),不在 delta 路径;
- 落盘是**消息粒度**不是 delta 粒度(`SessionRecord::Message`),每记录 open+flock+write+flush 在此粒度下可接受;durable 变体的"读全文件修复"只在末字节非换行时触发,正常路径 O(1);
- 每 batch 的 SHA-256 digest(`reducer.rs:525-538`)是对事件序列化的 O(delta) 操作,不是 O(累积);
- 工具输出上限 `MAX_TOOL_OUTPUT_BYTES = 8KB`(`orca-core/src/tool_types.rs:8`),大输出不会整段进内存穿越边界;
- 文件搜索是规范的 nucleo 增量实现(后台 walker 经 Injector 注入、匹配增量、截取 top 12 并注释了排序优化);mention catalog 刷新在后台线程且有 worker 回收;
- `reqwest` blocking feature 的使用者:provider HTTP(配合同步内核的有意选择)与后台更新检查,均合理。

---

## 六、行动计划

### 第一档:立即(小改动,大收益)

1. goal actor `request` 加超时 + supervisor 7 处包 `spawn_blocking`(§1.1、§1.2);
2. reducer delta 改追加语义(§1.3);
3. `push_message` ToolCall 查重加 HashMap(§4.2);
4. 删 orca-tui 的 orca-provider 依赖(§2.5);
5. usage 改按 ordinal 赋值(§4.2)。

### 第二档:v0.3.1 缺陷修复(下个补丁版本)

按 §1.6:picker fork 补历史快照(C1)、同步 manifest `closed_inventory` + sha256 trailer(C2)、实现 `rename_current_session` surface patch(H1)、picker UI 置灰当前会话破坏项(H2)、补渲染与 history_contract 测试(H3);顺手修 fork 默认标题与转录污染两个低成本 Medium。同时把契约校验器接进 CI(§1.5)。

### 第三档:计划性重构(按依存顺序)

1. 清理 254 处 `include_str!` 文本断言(§2.2,是后续一切的前置);
2. `runtime_surface/mod.rs` glob 改显式导出(§2.4,不改逻辑,先拿到真实依赖图);
3. ThreadActor 四刀拆分(§2.1);
4. provider→tools 倒置矫正(§2.3,可与 3 并行);
5. 收敛 `unstable_surface`、McpRegistry 移入 runtime、授权判定单入口(§2.4、§2.5);
6. TUI 三层状态收敛(先补一致性断言,§4.2)。

---

*审查方法说明:架构/并发/TUI/发布改动四个维度由并行评审完成并经主审抽查复核(picker fork 转录残留、manifest 漂移、alternate screen 迁移等关键结论均经独立验证);性能维度因评审资源中断由主审直接完成。*

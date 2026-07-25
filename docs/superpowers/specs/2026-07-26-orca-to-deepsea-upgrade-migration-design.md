# Orca to DeepSea Upgrade Migration Design

Date: 2026-07-26
Status: Approved direction, revised after code-grounded review
Selected approach: Orca installs and hands off to DeepSea; DeepSea owns migration

## Objective

Rename the public product from Orca to DeepSea Code without abandoning existing
users or silently mutating their local state.

An existing Orca installation must provide one guided upgrade flow that:

1. explains the rename;
2. installs the matching DeepSea distribution;
3. verifies the new executable;
4. hands migration authority to DeepSea;
5. inventories and copies compatible user and current-project state;
6. validates the migrated state;
7. activates `orca` as a compatibility command that launches DeepSea; and
8. launches DeepSea.

An independently installed DeepSea binary must discover the same legacy data
and offer the same migration before normal first-run setup.

The migration must be resumable, idempotent, inspectable, and reversible.

## Naming and Distribution Contract

| Surface | Legacy | New |
| --- | --- | --- |
| Product | Orca | DeepSea Code |
| Primary CLI | `orca` | `deepsea` |
| Compatibility CLI | n/a | `orca` forwards to `deepsea` after migration |
| npm package | `@blade-ai/orca` | `@blade-ai/deepsea` |
| User home | `~/.orca` / `ORCA_HOME` | `~/.deepsea` / `DEEPSEA_HOME` |
| Project directory | `.orca/` | `.deepsea/` |
| Repository | `echoVic/blade-deepseek` | unchanged |
| Legacy website | `orcaagent.dev` | retained as a transition and redirect surface |

Version continuity is explicit:

| Artifact | Version | Role |
| --- | --- | --- |
| `@blade-ai/orca` and Orca release | `0.2.53` | Last pure-Orca release |
| `@blade-ai/orca` | `0.3.0` | Full transition runtime and compatibility command |
| `@blade-ai/deepsea` | `0.3.0` | First DeepSea release |
| GitHub release | `v0.3.0` | Contains both Orca-transition and DeepSea assets |

Platform-specific npm packages follow the existing distribution model:

- `@blade-ai/deepsea-darwin-arm64`
- `@blade-ai/deepsea-darwin-x64`
- `@blade-ai/deepsea-linux-arm64`
- `@blade-ai/deepsea-linux-x64`

These are optional-dependency alias keys, matching the current Orca packaging
pattern. The published native artifacts remain prerelease variants of the main
package, for example
`@blade-ai/deepsea@0.3.0-darwin-arm64`; they are not separately published
package names.

The two binaries must have explicit compile-time identities. The current CLI
hard-codes `orca` in Clap metadata and diagnostics, so `deepsea` cannot be a
renamed copy or symlink of that executable. Shared runtime code receives a
typed product identity from separate Orca-transition and DeepSea entry points.
An internal machine-readable identity probe reports product, version,
migration-protocol version, and distribution kind for handoff verification.
It does not infer trusted identity from `argv[0]`.

## Ownership Boundary

The old Orca binary owns only:

- detecting that the available release is a product migration;
- explaining the migration;
- detecting the current installation method;
- installing the corresponding DeepSea package or binary;
- verifying that the installed DeepSea executable starts; and
- launching DeepSea with a short-lived migration handoff; and
- activating its own compatibility redirect only after DeepSea reports a
  validated migration.

The new DeepSea binary exclusively owns:

- inventorying legacy state;
- presenting the migration plan;
- resolving destination conflicts;
- copying and transforming data;
- validating migrated state;
- recording migration progress and results; and
- launching the normal DeepSea experience.

Orca must not write `~/.deepsea` or `.deepsea/`. DeepSea must not require Orca
to understand the new storage format.

## Required Product-Identity and Path Boundary

The current codebase hard-codes `orca`, `ORCA_HOME`, `~/.orca`, and `.orca/`
across the root CLI, core config, folder trust, runtime history, Goals, memory,
tasks, workflows, tools, skills, sandbox policy, and TUI. The TUI input-history
path currently bypasses `ORCA_HOME` and writes directly to
`~/.orca/history.jsonl`.

The migration cannot be implemented safely with a repository-wide string
replacement. Before either DeepSea binary or migration writer ships, shared
code must consume a typed product/path boundary that provides:

- product display name and CLI name;
- user-home environment variable and default home;
- project-directory name;
- diagnostics prefix;
- npm package and install command;
- update/migration state paths;
- history, archive, Goal, task, workflow, memory, skill, tool, trust, and input
  history paths; and
- compatibility project-source candidates.

Orca-transition identity resolves only Orca-owned writable paths. DeepSea
identity resolves only DeepSea-owned writable paths. Compatibility fallback
paths are explicitly read-only. Hook ABI names such as `ORCA_TOOL_NAME` remain
separate compatibility constants rather than being derived blindly from the
product display name.

Tests must inventory direct uses of legacy home/project literals and reject new
storage ownership outside this boundary. Internal Rust crate names may remain
`orca-*`; crate renaming is not required for correct user-visible migration.
Child processes and workers launched through the current executable must carry
the selected product identity explicitly, so a DeepSea parent cannot silently
spawn an Orca-identified child or return to Orca-owned paths.

## Legacy Command Compatibility

After a validated migration, the command:

```bash
orca
```

must open DeepSea. The compatibility applies to the full command surface, not
only an argument-free launch:

```bash
orca exec "fix the failing test"
orca history list
orca --mode=acp
```

Each invocation forwards to the equivalent `deepsea` invocation.

The compatibility command is a launcher, not a second product runtime. It must:

- preserve every argument byte and argument boundary;
- preserve stdin, stdout, stderr, terminal/PTY attachment, working directory,
  and environment except for documented legacy-to-new environment mapping;
- deliver interrupts and termination signals to DeepSea;
- return DeepSea's exit code;
- avoid printing a rename notice during normal interactive use;
- emit at most one concise deprecation notice in appropriate non-interactive
  contexts, controlled by a persisted user preference; and
- fail with an exact reinstall/repair command if DeepSea cannot be resolved.

The redirect is activated only after DeepSea returns a versioned,
handoff-nonce-bound migration-success result to the waiting Orca process. Orca
then records a compatibility receipt in its update state. Before that receipt
exists, `orca` continues to run Orca so a failed or cancelled migration cannot
strand the user.

For npm installations, `@blade-ai/orca@0.3.0` remains installed as the owner of
the `orca` executable. It is a complete transition runtime: before migration
or after rollback it can still run Orca; after a validated receipt it resolves
and launches the installed `@blade-ai/deepsea` package without depending on an
untrusted working-directory executable.

For direct installations, the `orca` path remains the full `v0.3.0` transition
binary. It forwards to the verified sibling DeepSea installation only while an
active compatibility receipt exists. It must not be replaced by a shell alias,
because aliases do not cover scripts, subprocesses, ACP clients, or other
non-interactive callers.

The compatibility launcher is supported for the migration compatibility
window and must not be removed by the guided migration. Removing it is a
separate advanced action that clearly states that the `orca` command will stop
working.

## Upgrade Entry

Existing `v0.2.53` clients cannot understand a new migration response. Their
current update flow first performs an ordinary upgrade to
`@blade-ai/orca@0.3.0`. On the next `orca` launch, the local transition runtime
shows the rename prompt without requiring another network update check:

```text
Orca is now DeepSea Code

• New command: deepsea
• New package: @blade-ai/deepsea
• Your configuration, credentials, sessions, goals, skills,
  tools, workflows, and current-project settings can be migrated.
• Orca data will be kept for rollback.

[Migrate to DeepSea] [Remind me later] [Stay on Orca]
```

Semantics:

- **Migrate to DeepSea** begins installation and handoff.
- **Remind me later** records a bounded reminder timestamp, initially seven
  days. It must not reuse the current `skip_until_version` update field,
  because no later Orca feature release is expected and that would effectively
  suppress migration forever.
- **Stay on Orca** records an explicit indefinite opt-out. The manual
  `orca migrate-to-deepsea` entry remains available; only a material migration
  or security requirement may override the opt-out.
- Server, ACP, JSONL, piped-stdin, worker, and other non-interactive invocations
  never open a migration prompt or add text to protocol stdout. Before
  migration they continue to execute Orca. A notice may be written to stderr
  only where stderr is not part of a documented machine protocol.
- `orca migrate-to-deepsea` provides the explicit migration entry for scripts,
  SSH sessions, and users who dismissed the startup prompt.

## Installation and Handoff

### npm-managed installation

Orca runs the platform-appropriate equivalent of:

```bash
npm install -g @blade-ai/deepsea
```

It does not uninstall `@blade-ai/orca`.

### Direct binary installation

Orca invokes the versioned, checksum-verified DeepSea installer using the same
destination directory as the running Orca executable when writable. The
current installer verifies a SHA-256 file from the same GitHub release; the
design must not claim cryptographic signing unless release signing or
attestation is implemented separately. If the directory is not writable, the
prompt explains the target and required user action rather than silently
changing installation location.

### Verification

After installation, Orca resolves the exact installed executable and runs a
machine-readable version probe. A successful probe must establish:

- the executable is DeepSea rather than Orca;
- its version supports the migration protocol; and
- the executable path is the one the handoff will launch.

Orca then creates a short-lived handoff file containing only:

- migration protocol version;
- source Orca version and install method;
- resolved legacy home path;
- current working directory;
- expected DeepSea executable path; and
- a random one-time nonce.

The handoff contains no API keys or configuration contents. It is created with
user-only permissions and deleted after consumption or expiry.

DeepSea is launched as:

```bash
deepsea migrate-from-orca --handoff <path>
```

Orca waits for the migration process. Only a protocol-level validated-success
result activates the legacy-command compatibility receipt. Cancellation,
partial success, a generic zero exit status without the success result, or any
validation error leaves Orca behavior unchanged.

## Direct DeepSea Installation Entry

Users may install `@blade-ai/deepsea` or a native DeepSea binary without first
accepting the Orca update prompt. The first interactive DeepSea launch must
therefore perform legacy discovery before creating new configuration,
credentials, history, or Goal state.

Automatic discovery runs only for an argument-free interactive TUI launch with
terminal stdin and stdout. It does not run before ACP, server, JSONL, worker,
history, trust, workflow, or `exec` commands. Those modes remain deterministic
and use `deepsea migrate-from-orca` as the explicit entry point.
In the interactive path, discovery is a preflight before effective config,
external tools, history, Goals, or the TUI are initialized; loading those
subsystems first would create destination conflicts merely by launching
DeepSea.

DeepSea checks legacy homes in this order:

1. an explicit legacy-home argument supplied to the migration command;
2. the current `ORCA_HOME`, when set; and
3. the default `~/.orca`.

Duplicate paths are canonicalized and inspected once. A directory counts as a
migration candidate only when it contains supported user data; an empty
directory or an update cache by itself does not trigger the prompt.

When a candidate exists, DeepSea shows:

```text
Existing Orca data found

Source: ~/.orca
• Configuration and credentials
• 126 sessions
• 3 active goals
• 8 skills
• 2 workflows
• Current project .orca settings

[Migrate from Orca] [Start Fresh] [Not Now]
```

Semantics:

- **Migrate from Orca** enters the same inventory, conflict, copy, validation,
  journal, and report flow used by the Orca handoff.
- **Start Fresh** records a decision for the fingerprinted legacy home and
  continues normal DeepSea setup. It never deletes or modifies Orca data.
- **Not Now** records the same bounded reminder timestamp used by the Orca
  prompt. It does not wait for a newer DeepSea release.
- `deepsea migrate-from-orca` always remains available to reopen discovery and
  migrate explicitly.

DeepSea records discovery decisions in its own update state, keyed by the
canonical legacy-home path and a non-secret content fingerprint. It must not
prompt on every launch when the user chose **Start Fresh**, when the candidate
has already migrated successfully, or when no supported Orca data exists. A
material change to the legacy source may be shown as a new optional migration,
but it must not block normal startup.

If `~/.deepsea` or `DEEPSEA_HOME` already contains user state, direct-install
migration enters the normal conflict-planning flow. It never treats the
DeepSea destination as empty merely because this is the first launch of the
current binary.

After validated direct-install migration, DeepSea checks whether an `orca`
command is installed:

- if no `orca` command exists, migration completes without creating an
  unsolicited alias;
- if a compatible transition Orca is found, DeepSea sends it a
  nonce-bound activation request so that Orca records its own redirect receipt;
- if an older Orca cannot activate the redirect, DeepSea offers to update the
  Orca compatibility package or exact direct launcher before retrying; and
- DeepSea never replaces an unrelated executable that happens to be named
  `orca`.

This preserves the ownership boundary: direct-install DeepSea discovers and
migrates data, while Orca still owns activation of its existing command path.

## Environment and Project Compatibility

DeepSea owns the new runtime namespace:

- `DEEPSEA_HOME` selects the DeepSea home;
- `~/.deepsea` is the default DeepSea home; and
- `.deepsea/` is the primary project directory.

`ORCA_HOME` is a legacy-source locator during discovery, not an alias for
`DEEPSEA_HOME`. A compatibility receipt records the exact DeepSea destination
home. When `orca` forwards to DeepSea, it supplies that destination as
`DEEPSEA_HOME` unless the user explicitly provided a different
`DEEPSEA_HOME`.

For other public runtime variables, DeepSea uses this precedence during the
compatibility window:

```text
DEEPSEA_* > ORCA_* > DEEPSEEK_* > configuration/default
```

Legacy hook and external-tool environment variables beginning with `ORCA_`
are an existing integration ABI. DeepSea must continue emitting them during
the compatibility window and may additionally emit documented `DEEPSEA_`
aliases. Migration must not rewrite arbitrary hook, tool, skill, workflow, or
shell content.

Project configuration uses deterministic fallback rather than eager mutation:

1. `.deepsea/` wins when present;
2. when `.deepsea/` is absent, DeepSea may read a trusted `.orca/` project
   directory in compatibility mode;
3. `.deepsea/` and `.orca/` are never implicitly merged; and
4. compatibility reads never write into `.orca/`.

This fallback applies to `deepsea` and forwarded `orca` invocations, including
non-interactive `exec`, server, and ACP use. It prevents scripts in projects
that have not yet accepted project migration from losing config, rules,
skills, or workflows.

The fallback does not weaken folder trust. DeepSea evaluates its own migrated
trust store first. While migration is pending, it may consult the legacy Orca
trust decision read-only for the same canonical project path; an absent,
invalid, or changed decision requires the normal trust flow. Merely finding a
`.orca/` directory never grants trust.

## Migration Inventory

DeepSea inventories the resolved Orca home. It must respect an explicit
`ORCA_HOME`; it must not assume `~/.orca` when the old installation used a
custom home.

The initial migration covers:

| Legacy source | DeepSea destination | Treatment |
| --- | --- | --- |
| `config.toml` | `config.toml` | Parse and copy semantically; transform only explicitly versioned schema fields |
| `auth.json` | `auth.json` | Copy without logging contents; preserve restrictive permissions |
| `sessions/` | `sessions/` | Copy JSONL/Zstd history and validate indexes |
| `archive/` | `archive/` | Copy archived JSONL/Zstd history and preserve archive status |
| `history.jsonl` | `history.jsonl` | Copy TUI input history; also inspect default `~/.orca/history.jsonl` because current code does not honor `ORCA_HOME` for this file |
| `goals.sqlite3` | `goals.sqlite3` | Create a consistent SQLite backup, then validate schema and active state |
| `goals_1.json` | legacy import input | Import only when the SQLite store has not already recorded legacy migration |
| `task-sessions/` | `task-sessions/` | Copy and validate metadata |
| `skills/` | `skills/` | Copy |
| `tools/` | `tools/` | Copy byte-for-byte and validate descriptors |
| `workflows/` | `workflows/` | Copy byte-for-byte and validate discoverability |
| `memory/` | `memory/` | Copy user and project memory byte-for-byte |
| `AGENTS.md` | `AGENTS.md` | Copy user instructions byte-for-byte |
| `folder_trust.toml` | `folder_trust.toml` | Parse and preserve effective trust decisions |
| supported user rules | corresponding DeepSea paths | Copy byte-for-byte |
| `summary_cache/` | not migrated | Rebuildable cache |
| `goals.runtime.lock` and SQLite transient files | not migrated | Runtime coordination state, not durable user data |
| legacy goal backups and update cache | not active migration input | Leave in Orca and list in the report |

Unknown files are listed in the report and left in the Orca home. They are not
silently copied.

## Current-Project Migration

The wizard inspects only the current working directory for `.orca/`.

It may offer to copy:

- `.orca/config.toml`;
- `.orca/skills/`;
- `.orca/workflows/`;
- `.orca/rules*`;
- `.orca/workflow-sessions/`; and
- legacy `.orca/task-sessions/`.

Project `.orca/tools/` is not a supported input because the current runtime
intentionally loads external tools only from the user home to avoid
repository-controlled tool execution.

It must not scan the entire filesystem or mutate projects recovered from
session history. Other projects are reported as candidates and are migrated
only when the user later opens them and confirms the project migration.

Project migration must respect repository state:

- if `.deepsea/` does not exist, stage the copied directory atomically;
- if `.deepsea/` exists, show conflicts before writing;
- active project workflow sessions are not copied while their source state is
  changing; they are deferred until stable or explicitly skipped;
- do not stage, commit, or modify Git history; and
- clearly report that new `.deepsea/` files are ordinary working-tree changes.

## Confirmation and Conflict Policy

Before writing, DeepSea displays counts, byte sizes, destinations, warnings,
and conflicts. The default action is migration with rollback preserved.

For each destination conflict the supported decisions are:

- keep the existing DeepSea item;
- replace it with the Orca item;
- keep both when the item type has a safe deterministic alternate name; or
- skip it.

No global "replace everything" default is allowed when credentials, config,
goals, trust, or permission state conflicts.

## Source Consistency and Running Processes

Migration may start while another Orca process is writing sessions, Goals,
tasks, or project workflows. A recursive filesystem copy is therefore not a
valid snapshot by itself.

DeepSea must:

- acquire its own exclusive destination migration lock;
- detect the Orca Goal runtime lock and other known live writers;
- use SQLite's backup API or an equivalent consistent read transaction for
  `goals.sqlite3`, never copy a live database plus ad hoc WAL files;
- copy append-only session and task files only after recording source
  fingerprints, then recheck those fingerprints before commit;
- retry a bounded number of times when a source changes;
- defer still-active items with a clear "close other Orca processes and
  resume" instruction; and
- never mark a partial or unstable snapshot as migrated.

The migration wizard itself runs before the new DeepSea TUI/runtime starts, so
it does not create competing DeepSea writers. The Orca handoff process waits
without starting its normal runtime.

A deferred item keeps the migration incomplete and leaves `orca` running Orca.
The user may explicitly exclude a non-critical item from a revised plan, in
which case the final report names the omission. Credentials, effective config,
the Goal store, compatibility receipt, and any item required by an active Goal
cannot be silently or implicitly excluded. Redirect activation occurs only
after every item in the confirmed plan is committed and validated.

## Transaction and Recovery

Migration is copy-based. Orca user content and the project `.orca/` directory
are never deleted or modified. After validated success, the waiting Orca
process may update only its existing update-state file with the compatibility
receipt; this receipt is not user content and contains no migrated data.

DeepSea writes a migration journal under its home. Each item progresses through
explicit states:

```text
discovered -> planned -> copied -> validated -> committed
```

Writes use a staging directory on the same filesystem as the destination.
After validation, files or directories are atomically renamed into place.

Restart behavior:

- committed items are not recopied;
- staged but uncommitted items are revalidated or discarded safely;
- changed source files are detected by size, modification time, and checksum
  before reuse;
- the user can resume, restart the migration plan, or exit without losing the
  original Orca data.

Running the migration again after success produces an inventory and a
no-op/changed-items plan rather than duplicating sessions or goals.

## Rollback

Reversibility requires an executable path, not only retained files.

The transition runtime reserves:

```bash
orca migrate rollback
```

This command is handled by Orca before forwarding. It disables the
compatibility receipt and returns future `orca` invocations to the full
`v0.3.0` Orca runtime. It does not delete DeepSea or copy DeepSea state back
into Orca.

The rollback prompt must warn that sessions, Goals, configuration changes, and
other writes created after migration live only under the DeepSea home.
Re-entering migration later performs a new incremental inventory rather than
assuming the first snapshot is current.

## Validation

Migration success requires more than successful file copies. DeepSea validates:

- `config.toml` parses under the new schema;
- authentication data is readable without exposing the credential;
- all migrated session transcripts can be indexed;
- active goals reference available sessions and retain status, elapsed time,
  token usage, budget, objective, and timestamps;
- task and workflow session metadata can be loaded;
- skills, tools, workflows, and rules can be discovered;
- trust and permission stores parse with the same effective decisions; and
- the current project's new configuration resolves from `.deepsea/`.

Validation failures are itemized. A partial migration is never reported as
complete.

## Completion and Cleanup

On success, DeepSea displays:

```text
Migration complete

✓ Configuration
✓ Credentials
✓ 126 sessions
✓ 3 active goals
✓ 8 skills
✓ 2 workflows
✓ Current project settings

Run: deepsea

The orca command now opens DeepSea.
Orca user content was not modified; only its transition receipt changed.

[Launch DeepSea] [View migration report]
```

The guided migration does not uninstall `@blade-ai/orca` or remove the direct
`orca` launcher, because that would violate the command-compatibility
contract.

Removing legacy components is a separate, explicit advanced action:

- npm installations may remove `@blade-ai/orca` only after warning that the
  `orca` command will no longer work;
- direct installations may remove only the exact compatibility launcher after
  the same warning;
- neither path deletes `~/.orca` or project `.orca/` data; and
- data deletion, if ever supported, requires a separate command and explicit
  target confirmation.

## Failure Behavior

- **Network or package failure:** keep Orca running and provide the exact retry
  command.
- **DeepSea probe failure:** do not launch migration or uninstall Orca.
- **Compatibility activation failure:** report migration success separately,
  keep the existing Orca command functional, and provide an idempotent
  `deepsea migrate repair-alias` command.
- **Direct-install discovery failure:** continue normal DeepSea startup only
  after reporting the inaccessible candidate; never interpret a permission
  error as an empty Orca home.
- **DeepSea missing after migration:** the `orca` launcher fails safely with
  the exact `@blade-ai/deepsea` reinstall command; it never falls back to an
  unrelated executable from the current directory.
- **Source changes during migration:** retry stable items, defer live items,
  and keep Orca active; never commit a mixed-time snapshot as complete.
- **Permission failure:** identify the exact destination and leave both
  installations unchanged.
- **Existing DeepSea state:** enter conflict planning; never overwrite.
- **Migration validation failure:** retain the journal and staging data needed
  for resume, keep Orca data untouched, and do not claim success.
- **Offline environment:** provide a version-matched manual download and a
  `deepsea migrate-from-orca` command that can run after installation.
- **Non-interactive environment:** never prompt or mutate; return structured
  instructions.

## Security and Privacy

- Handoff and journal files use user-only permissions.
- Secrets are never embedded in the handoff, report, logs, telemetry, or error
  messages.
- Symlinks in copied trees are inventoried and shown; migration must not follow
  a symlink outside the approved source root.
- Destination paths are canonicalized and confined to the approved DeepSea
  home or current-project `.deepsea/`.
- No network upload of configuration, history, prompts, goals, or credentials
  occurs.
- Migration telemetry, if introduced, is opt-in and limited to anonymous
  success/failure counters without paths or item contents.

## Release Strategy

The current tag workflow publishes one Orca binary/package family and creates
the GitHub release before npm publication. It cannot satisfy this design
unchanged.

The `v0.3.0` release pipeline must:

1. reserve and verify control of the `@blade-ai/deepsea` package name before
   advertising the migration;
2. build both `orca` and `deepsea` binaries from the same source revision;
3. package both native asset families into the same GitHub release;
4. stage both npm package families;
5. publish DeepSea platform packages, then `@blade-ai/deepsea@0.3.0`, using
   non-default candidate tags during verification;
6. verify a real global DeepSea install and `deepsea --version`;
7. publish Orca transition platform packages, then
   `@blade-ai/orca@0.3.0`, also under non-default candidate tags;
8. verify a real `v0.2.53 -> v0.3.0` npm upgrade, migration handoff, forwarding,
   and rollback;
9. move the DeepSea and Orca main-package `latest` dist-tags to `0.3.0` only
   after both package families and the cross-package handoff are verified; and
10. publish release notes that identify `v0.2.53` as the last pure-Orca
    release.

The existing installer remains hosted at `orcaagent.dev/install.sh` for Orca
transition upgrades and gains a distinct DeepSea installation mode/asset
name. It must never replace the `orca` path with the `deepsea` binary.

After release:

- keep `@blade-ai/orca@0.3.0` available as the full transition runtime;
- keep ordinary Orca execution available before migration and after rollback,
  failed migration, or cancelled migration;
- stop feature releases under Orca, allowing only transition/security fixes;
- do not publish a launcher-only Orca version that would remove rollback for
  users who have not migrated; and
- keep `orcaagent.dev` as a migration explanation and eventual permanent
  redirect surface.

## Verification Matrix

Automated coverage must include:

- npm-managed and direct-binary handoff;
- npm-managed and direct-binary DeepSea installation without an Orca handoff;
- first interactive launch with and without supported Orca data;
- direct installation before DeepSea has created any destination state;
- direct installation with existing non-empty DeepSea state;
- Start Fresh, Not Now, later explicit migration, and source-change
  rediscovery;
- canonical duplicate legacy paths and inaccessible legacy homes;
- compatible, outdated, missing, and unrelated `orca` executables after direct
  installation;
- macOS arm64/x64 and Linux arm64/x64 package resolution;
- custom `ORCA_HOME`;
- empty legacy home;
- complete home with every supported item type;
- `goals.sqlite3` with WAL activity and consistent-backup validation;
- archived sessions, input history, memory, trust, and user instructions;
- concurrent Orca session, Goal, task, and project-workflow writers;
- existing non-conflicting and conflicting DeepSea homes;
- interrupted migration at every journal state;
- repeated migration after success;
- malformed config, auth metadata, sessions, goals, and workflow state;
- symlink escape attempts;
- current project with clean, dirty, and pre-existing `.deepsea/` state;
- non-interactive invocation;
- install, probe, and permission failures;
- `orca` forwarding with no arguments and every supported command mode;
- argument, stdio, PTY, working-directory, signal, and exit-code preservation;
- compatibility activation only after protocol-level migration success;
- missing/tampered DeepSea executable and alias repair;
- custom `ORCA_HOME` and `DEEPSEA_HOME` compatibility routing;
- environment precedence and legacy hook ABI preservation;
- DeepSea child processes and workers preserving DeepSea identity and paths;
- discovery preflight creating no DeepSea state before the user's decision;
- trusted `.orca/` project fallback in interactive, exec, server, and ACP
  modes;
- untrusted or stale `.orca/` fallback requiring the normal trust flow;
- `.deepsea/` precedence without implicit merge;
- deferred live items blocking receipt activation until resumed or explicitly
  excluded from a valid revised plan;
- `orca migrate rollback` and incremental remigration after rollback;
- explicit compatibility-launcher removal and warning; and
- published-artifact smoke tests that prove the `v0.2.53 -> v0.3.0` update,
  direct DeepSea installation, `deepsea --version`, history resume, Goal
  restoration, `orca` forwarding, and Orca rollback all work.

## Non-Goals

- Renaming the GitHub repository.
- Deleting legacy Orca user or project data.
- Scanning and mutating every repository referenced by history.
- Silently merging conflicting credentials, permissions, trust, or goals.
- Maintaining permanent dual writes between `.orca` and `.deepsea`.
- Making Orca understand or write the DeepSea storage format.
- Renaming internal Rust crates as part of the user-data migration.

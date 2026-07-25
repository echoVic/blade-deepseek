# Orca to DeepSea Upgrade Migration Design

Date: 2026-07-26
Status: Approved direction
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

Platform-specific npm packages follow the existing distribution model:

- `@blade-ai/deepsea-darwin-arm64`
- `@blade-ai/deepsea-darwin-x64`
- `@blade-ai/deepsea-linux-arm64`
- `@blade-ai/deepsea-linux-x64`

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

For npm installations, `@blade-ai/orca` remains installed as the owner of the
`orca` executable and transitions to a lightweight compatibility launcher.
It resolves the installed `@blade-ai/deepsea` launcher without depending on an
untrusted working-directory executable.

For direct installations, the exact old `orca` path becomes a small launcher
for the verified sibling DeepSea installation. It must not be a shell alias,
because aliases do not cover scripts, subprocesses, ACP clients, or other
non-interactive callers.

The compatibility launcher is supported for the migration compatibility
window and must not be removed by the guided migration. Removing it is a
separate advanced action that clearly states that the `orca` command will stop
working.

## Upgrade Entry

The existing update checker gains a distinct migration release response rather
than representing the rename as an ordinary patch update. The prompt is:

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
- **Remind me later** dismisses the prompt only for the current release.
- **Stay on Orca** records an explicit opt-out for the migration release.
  A materially newer migration or security release may prompt again.
- Non-interactive invocations never open an interactive migration. They print
  one concise notice to stderr and provide an explicit migration command.

## Installation and Handoff

### npm-managed installation

Orca runs the platform-appropriate equivalent of:

```bash
npm install -g @blade-ai/deepsea
```

It does not uninstall `@blade-ai/orca`.

### Direct binary installation

Orca invokes the signed/versioned DeepSea installer using the same destination
directory as the running Orca executable when writable. If the directory is
not writable, the prompt explains the target and required user action rather
than silently changing installation location.

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
- **Not Now** postpones the decision for the current DeepSea release.
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

## Migration Inventory

DeepSea inventories the resolved Orca home. It must respect an explicit
`ORCA_HOME`; it must not assume `~/.orca` when the old installation used a
custom home.

The initial migration covers:

| Legacy source | DeepSea destination | Treatment |
| --- | --- | --- |
| `config.toml` | `config.toml` | Parse, transform renamed keys, then serialize |
| `auth.json` | `auth.json` | Copy without logging contents; preserve restrictive permissions |
| `sessions/` | `sessions/` | Copy; preserve bytes unless a versioned index migration is required |
| `goals_1.json` | `goals_1.json` | Parse and validate references to migrated sessions |
| `task-sessions/` | `task-sessions/` | Copy and validate metadata |
| `workflow-sessions/` | `workflow-sessions/` | Copy and validate metadata |
| `skills/` | `skills/` | Copy |
| `tools/` | `tools/` | Copy and rewrite only documented Orca paths or executable names |
| `workflows/` | `workflows/` | Copy and rewrite only documented Orca paths or executable names |
| rules files/directories | corresponding DeepSea paths | Copy |
| trust and permission stores | corresponding DeepSea paths | Parse and validate; preserve decisions |
| update cache | not migrated | DeepSea starts with its own update state |

Unknown files are listed in the report and left in the Orca home. They are not
silently copied.

## Current-Project Migration

The wizard inspects only the current working directory for `.orca/`.

It may offer to copy:

- `.orca/config.toml`;
- `.orca/skills/`;
- `.orca/workflows/`;
- `.orca/tools/`; and
- `.orca/rules*`.

It must not scan the entire filesystem or mutate projects recovered from
session history. Other projects are reported as candidates and are migrated
only when the user later opens them and confirms the project migration.

Project migration must respect repository state:

- if `.deepsea/` does not exist, stage the copied directory atomically;
- if `.deepsea/` exists, show conflicts before writing;
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
Orca user data was not modified.

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

1. Publish and verify all DeepSea platform packages and the main npm package.
2. Publish a final Orca transition release containing the migration-aware
   update prompt and installer handoff.
3. After validated migration, retain `@blade-ai/orca` as the compatibility
   launcher package for the `orca` command.
4. Keep ordinary Orca execution available before migration and after failed or
   cancelled migration.
5. Stop feature releases under Orca after the transition release; allow
   security fixes when required.
6. Mark `@blade-ai/orca` as a compatibility package only after the DeepSea
   install, redirect, and rollback paths have been verified against published
   artifacts.
7. Keep `orcaagent.dev` as a migration explanation and eventual permanent
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
- explicit compatibility-launcher removal and warning; and
- published-package smoke tests that prove `deepsea --version`, history resume,
  Goal restoration, `orca` forwarding, and Orca rollback all work with real
  artifacts.

## Non-Goals

- Renaming the GitHub repository.
- Deleting legacy Orca user or project data.
- Scanning and mutating every repository referenced by history.
- Silently merging conflicting credentials, permissions, trust, or goals.
- Maintaining permanent dual writes between `.orca` and `.deepsea`.
- Making Orca understand or write the DeepSea storage format.

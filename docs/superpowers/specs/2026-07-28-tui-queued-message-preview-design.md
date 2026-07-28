# TUI Queued Message Preview Design

## Goal

Make follow-up input visible and editable while an Orca TUI turn is running:

- the running composer accepts ordinary message input;
- Enter queues a follow-up instead of interrupting the current turn;
- up to three rows of queue preview appear immediately above the composer;
- `Alt+Up` removes the most recently queued message and restores it to the
  composer for editing;
- queued follow-ups run in FIFO order, one turn at a time;
- dispatcher control actions keep their existing bypass behavior.

This is the queued-message-preview item from the P2 TUI roadmap. It does not
include cwd/git status, Vim counts or registers, configurable keybindings,
`/doctor`, FPS telemetry, or onboarding work.

## Current Constraints

Orca currently has two different queue-like mechanisms:

1. `TuiActionDispatcher` owns a bounded command mailbox plus backlog.
   Interaction responses, interrupt, and background-current-turn control bypass
   that backlog.
2. `AppState::pending_workflow_notifications` and the controller thread's
   `pending_actions` hold internal workflow notifications.

Neither is a suitable source of truth for editable user follow-ups:

- an item accepted by the dispatcher may already have moved from backlog to the
  controller mailbox, so the TUI cannot reliably retract it;
- the dispatcher queue includes non-user actions that must never appear as
  queued chat messages;
- workflow notifications are internal model input and must stay distinct from
  user-authored follow-ups;
- the TUI currently renders a composer while `Running`, but ordinary text keys
  are routed only in `Idle` and `WaitingUserInput`.

The feature therefore needs a small user-input queue owned by `AppState`. A
queued user message is not sent to the dispatcher until Orca is ready to start
that exact next turn.

## Reference Behavior

The design follows current Codex TUI behavior where it fits Orca:

- queued user messages live in TUI state;
- the preview is distinct from approvals and other pending interactions;
- `Alt+Up` pops the newest queued user message back into the composer;
- turn completion sends at most one FIFO follow-up;
- large-paste restoration keeps placeholder payloads with the queued message.

Orca intentionally remains smaller:

- there is no steer-to-active-turn mode in this task;
- queued slash-looking text is a normal user follow-up, not a deferred local
  slash command;
- no terminal-specific alternative to `Alt+Up` is added;
- no queue persistence across process restart or session switch is added.

## Ownership Model

### `QueuedUserMessage`

Create a focused queue model in
`crates/orca-tui/src/queued_input.rs`.

Each queued item owns the complete state required both to submit and to restore:

- `visible_text`: the trimmed composer text shown in transcript and preview;
- `submission_text`: the trimmed text after large-paste placeholders expand;
- `submission_bindings`: mention bindings reconciled against
  `submission_text`;
- `composer_bindings`: mention bindings reconciled against `visible_text`;
- `pending_pastes`: placeholder-to-payload pairs needed by `Alt+Up`.

The queue never stores rendered `Line` values, cache entries, or transcript
coordinates.

The model exposes:

```rust
pub(crate) struct QueuedUserMessage;

impl QueuedUserMessage {
    pub(crate) fn from_composer(
        visible_text: String,
        pending_pastes: Vec<(String, String)>,
        mention_bindings: MentionBindings,
    ) -> Option<Self>;

    pub(crate) fn visible_text(&self) -> &str;
    pub(crate) fn preview_text(&self) -> String;
    pub(crate) fn submission(&self) -> (&str, &MentionBindings);
    pub(crate) fn into_composer_state(self) -> QueuedComposerState;
}

pub(crate) struct QueuedComposerState {
    pub(crate) visible_text: String,
    pub(crate) mention_bindings: MentionBindings,
    pub(crate) pending_pastes: Vec<(String, String)>,
}
```

Construction is pure and returns `None` for whitespace-only input. Mention
bindings are reconciled first against visible text, then against expanded and
trimmed submission text, matching the existing idle-submit pipeline.

### `AppState`

`AppState` owns:

```rust
pub(crate) queued_user_messages: VecDeque<QueuedUserMessage>,
pub(crate) queued_submission_in_flight: Option<QueuedUserMessage>,
pub(crate) queued_follow_up_autosend: bool,
```

The queue capacity is exactly `USER_ACTION_CAPACITY` (64). This prevents the new
pre-dispatch queue from growing beyond the existing action-mailbox contract.

`AppState` provides reducer-style operations:

```rust
pub(crate) fn enqueue_user_message(
    &mut self,
    message: QueuedUserMessage,
) -> Result<(), QueuedUserMessage>;

pub(crate) fn pop_latest_queued_message(&mut self) -> Option<QueuedUserMessage>;

pub(crate) fn begin_next_queued_message(
    &mut self,
) -> Option<UserAction>;

pub(crate) fn finish_queued_submission_start(&mut self);

pub(crate) fn reject_queued_submission(
    &mut self,
) -> Option<QueuedComposerState>;
```

`begin_next_queued_message`:

1. requires `Idle`;
2. requires `queued_follow_up_autosend`;
3. pops exactly one item from the FIFO front;
4. records input history at actual dispatch time;
5. appends the visible `ChatMessage::User`;
6. enters `Running` and scrolls to bottom;
7. stores a clone in `queued_submission_in_flight`;
8. returns `UserAction::SubmitWithMentions`.

It does not send the action itself. The event/action layer remains responsible
for side effects.

`TurnStarted` clears `queued_submission_in_flight`. Before that event, a
`SubmissionRejected` restores the exact visible composer state, pending-paste
payloads, and mention bindings instead of restoring expanded text.

`replace_messages`, `clear_messages`, and session-picker transitions clear all
queued-user state. Queue content never crosses a conversation boundary.

## Submission Semantics

### Idle

Idle submission keeps its existing behavior:

- local slash commands execute immediately;
- normal prompts are recorded and sent immediately;
- user transcript content appears immediately;
- large paste and mention behavior is unchanged.

If queued follow-ups remain after an explicit interrupt, a new manually
submitted idle prompt starts immediately. The queued FIFO remains behind that
new turn and autosend resumes for later boundaries.

### Running

The running composer supports:

- normal character editing;
- Vim insert/normal editing through the existing `VimState`;
- `Shift+Enter`, `Alt+Enter`, and `Ctrl+J` newline insertion;
- paste, including large-paste placeholders;
- file/skill/plugin/MCP mention completion;
- Enter to enqueue the current composer.

Running submission does not:

- append a user transcript message yet;
- record prompt history yet;
- send a `UserAction` yet;
- execute local slash commands.

A `/compact`-looking value entered during a turn is queued as literal user
input. Local slash commands remain an idle-only operation.

After successful enqueue:

- composer text, mention bindings, and pending-paste payloads reset;
- Vim returns to the same post-submit mode used by idle submission;
- the current turn remains `Running`;
- auto-follow remains unchanged;
- the preview updates immediately.

If the 64-item queue is full:

- the input remains in the composer;
- no message or history entry is added;
- the TUI emits a bounded visible error/notice;
- no dispatcher action is sent.

### Waiting for Approval or Input

Approval, permission, MCP elicitation, and runtime user-input responses are not
queued follow-up messages.

- `WaitingApproval` continues to hide the composer and queue preview.
- `WaitingUserInput` continues to treat Enter as the response to the pending
  interaction.
- Existing queued follow-ups remain stored but cannot be edited while an
  approval/modal interaction owns input.
- Workflow notifications remain in their existing internal queue and never
  appear in the user-message preview.

### Compacting

Compacting keeps the current restricted running shortcut set. It does not accept
new queued user input in this task.

## Turn-Boundary Dispatch

On every terminal `SessionCompleted` status (`success`, `failed`,
`verification_failed`, and other terminal statuses):

1. apply the existing transcript finalization;
2. drain internal workflow notifications into their existing queue;
3. if user-follow-up autosend is enabled, submit exactly one FIFO user
   follow-up;
4. otherwise, if no user follow-up started, submit one workflow notification
   using the existing policy;
5. leave all remaining user follow-ups visible for later turns.

User follow-ups have priority over internal workflow notifications because the
user explicitly queued them as the next conversational turns. Workflow
notifications remain queued and run after user follow-ups drain.

Only one user follow-up is sent per boundary. The dispatcher and runtime still
enforce actual admission and operation serialization.

### Explicit Interrupt

When the user interrupts the active turn with `Esc` or `Ctrl+G`:

- existing interrupt behavior is preserved;
- `queued_follow_up_autosend` becomes `false`;
- queued messages remain in the preview;
- the resulting terminal event does not automatically send the next follow-up.

This avoids surprising execution after an explicit stop. The user may:

- press `Alt+Up` to restore the newest queued item;
- submit a new idle prompt;
- enqueue/send again during the next running turn.

Starting any new foreground user turn restores
`queued_follow_up_autosend = true`.

### Background Current Turn

`Ctrl+B` keeps its control-path priority. After requesting background:

- the foreground status becomes idle as today;
- if queued follow-ups exist, exactly one is immediately promoted to the next
  foreground turn;
- the dispatcher sees `BackgroundCurrentTurn` before the promoted submit
  because both are sent in that order and background is a bypass control;
- remaining queued messages continue FIFO behind the new foreground turn.

## `Alt+Up` Restore

Register `Alt+Up` in both Idle and Running shortcut contexts.

The action is enabled only when:

- the queue is non-empty;
- the transcript search field is closed;
- no approval, setup, session picker, slash popup, mention popup, or shortcuts
  overlay owns input;
- the conversation panel is active.

The action:

1. pops from the queue back;
2. replaces the current composer contents;
3. restores original pending-paste payloads;
4. restores mention bindings against visible composer text;
5. resets history navigation;
6. places the cursor at the end;
7. applies the existing post-submit/post-rejection Vim reset convention
   (`Normal` when Vim mode is enabled, `Insert` otherwise);
8. redraws the preview.

Replacing the current composer is deliberate and matches the explicit “edit
last queued message” action. It does not silently merge two drafts.

`Alt+Up` does nothing when no queued message exists. Plain Up retains its
existing context behavior: history in Idle and transcript scroll in Running.

## Queue Preview

### Placement

Add one bounded layout region between the activity row and the transcript
search/composer rows:

```text
transcript
plan/activity
queued follow-up preview (0..3 rows)
search row (optional)
composer
status
```

The preview appears only when:

- the conversation panel is active;
- at least one queued user message exists;
- status is `Idle` or `Running`;
- no approval or full-screen overlay owns the frame.

It remains visible after an explicit interrupt so users can see what was held.

### Three-Row Contract

The region uses at most three physical rows total, not three rows per message.

- Row 1: `Queued N · Alt+Up edit latest`
- Row 2: FIFO head preview
- Row 3:
  - the second item when exactly two are queued;
  - `… K more · latest: <preview>` when more than two are queued.

For one queued item, only rows 1 and 2 are used.

Each message preview:

- collapses CR/LF and repeated whitespace to single spaces;
- uses the visible composer form, so large pastes remain bounded placeholders;
- is truncated by terminal display width without splitting UTF-8 or wide
  graphemes;
- uses muted/italic styling while preserving no secret expansion;
- renders safely when width is zero or very narrow.

The preview does not mutate `TranscriptRenderCache`, transcript search matches,
selection coordinates, or scroll offsets.

### Compact Frames

Queue preview height is computed before `main_layout` and capped at three.
Fixed composer/status/search rows keep their existing priority. On extremely
short frames, ratatui may reduce the transcript to zero rows, but queue preview
must never overlap the composer, status, popup, or hardware cursor.

Slash and mention popup geometry continues to use the composer input rect, so
the preview is outside popup hit testing.

## Input Priority

The event order remains:

1. focus/paste/resize/mouse preprocessing;
2. global key preflight (`Ctrl+C`, search, global scroll/clear);
3. modal status routing;
4. conversation-status input.

Within `Running`:

1. open transcript search owns all search editing/navigation;
2. mention popup owns selection;
3. `Alt+Up` restores a queued item;
4. background/interrupt/scroll shortcuts retain priority;
5. newline and queue-submit shortcuts apply;
6. remaining keys edit the composer.

Consequences:

- `Ctrl+C` still exits/cancels at global priority;
- open transcript search swallows `Alt+Up`;
- `Ctrl+G` still navigates search while search is open and interrupts otherwise;
- plain Up still scrolls during Running;
- Enter selects an open mention candidate before it queues the message.

## Error and Recovery Behavior

### Pre-start Submission Rejection

When a promoted queued message is rejected before `TurnStarted`:

- remove the optimistic user transcript row;
- restore its original composer text, pending-paste payloads, and mention
  bindings;
- leave the remaining FIFO untouched;
- set status to `Idle`;
- disable automatic queue draining until the user submits again.

For ordinary non-queued submission rejection, preserve the existing expanded
prompt restoration behavior.

### Runtime Failure After Start

After `TurnStarted`, the message is no longer retractable. A later failed or
verification-failed terminal is a completed turn boundary and may advance the
next queued follow-up unless the user explicitly interrupted.

### Channel Admission Failure

If `try_send` reports a full or disconnected action channel:

- restore the in-flight queued item to the front of the queue;
- remove the optimistic transcript row;
- return to `Idle`;
- show an error;
- do not drop mention or paste state.

Tests cover both full and disconnected channels. The event loop never blocks
waiting for action-channel capacity while promoting a queued follow-up.

## Files

### Create `crates/orca-tui/src/queued_input.rs`

Own:

- `QueuedUserMessage`;
- `QueuedComposerState`;
- construction, preview normalization, and pure queue-item tests.

### Create `crates/orca-tui/src/queued_input_actions.rs`

Own:

- enqueue from composer;
- restore latest into composer;
- promote one FIFO item to a `UserAction`;
- running composer key handling;
- runtime-boundary dispatch helpers.

### Modify `crates/orca-tui/src/lib.rs`

Register the two focused modules.

### Modify `crates/orca-tui/src/types.rs`

Add queue state and reducer methods. Clear queue state on conversation
replacement/reset, track in-flight acceptance, and integrate `TurnStarted`.

### Modify `crates/orca-tui/src/status_key_actions.rs`

Route Running composer input through the focused handler after modal/Vim search
ownership.

### Modify `crates/orca-tui/src/running_actions.rs`

Suspend autosend on interrupt and promote a queued message after backgrounding.

### Modify `crates/orca-tui/src/shortcuts.rs`

Register:

- Running Enter for queue submit;
- Running newline bindings;
- Idle and Running `Alt+Up` for edit-latest;
- backed shortcut hints.

Do not add configurable bindings in this task.

### Modify `crates/orca-tui/src/input_event_actions.rs`

Allow paste into the Running composer. Keep approval/setup/search paste
precedence unchanged.

### Modify `crates/orca-tui/src/mention_search_manager.rs`

Enable mention lookup in the Running conversation composer while keeping modal
and slash-menu exclusions.

### Modify `crates/orca-tui/src/runtime_event_actions.rs`

Dispatch one queued user follow-up at terminal boundaries before internal
workflow notification submission. Restore exact composer state for a rejected
promoted queue item.

### Modify `crates/orca-tui/src/ui.rs`

Add bounded preview height/rendering and integrate the region into
`main_layout`. Preserve hardware-cursor and popup geometry.

### Modify `crates/orca-tui/src/composer_input_actions.rs`

Keep slash-menu projection idle-only while allowing the shared editor path to
run in Running.

### Modify `crates/orca-tui/src/display_text.rs`

Upgrade `truncate_to_display_width` to iterate extended grapheme clusters rather
than scalar characters. Preserve its current ASCII/CJK behavior and prove that
combining sequences, emoji modifiers, ZWJ families, and keycaps stay intact.

### Modify `crates/orca-tui/src/idle_submit_actions.rs`

Resume queue autosend when a new user foreground turn is submitted. Do not
otherwise change idle submission.

## Test Matrix

### Queue Model

- whitespace-only input is rejected;
- visible text, expanded paste content, visible bindings, submission bindings,
  and paste payloads round-trip;
- FIFO promotion and LIFO restore are independent;
- capacity 64 accepts exactly 64 and rejects the 65th without mutation;
- preview normalization is Unicode-safe and never expands large paste content.

### Running Input

- ordinary text edits the composer during Running;
- Enter queues and clears composer without sending an action;
- newline bindings insert newlines instead of queueing;
- plain Up keeps scrolling;
- mention selection precedes queue submit;
- slash-looking text queues as literal user input;
- paste uses the same large-paste placeholder contract;
- Vim insert and normal editing remain valid.

### Restore

- `Alt+Up` restores the newest queued item and keeps earlier FIFO entries;
- pending paste and mention bindings restore exactly;
- cursor lands at the end;
- existing draft replacement is explicit;
- search/modal/overlay ownership prevents restore;
- no-op on an empty queue.

### Dispatch

- `SessionCompleted` sends one FIFO item and leaves later items visible;
- all terminal statuses use the same boundary;
- user queue precedes workflow notifications;
- `TurnStarted` clears in-flight restore state;
- rejection before start restores exact composer state;
- failure after start does not restore;
- full and disconnected action channels restore the item to queue front;
- explicit interrupt suppresses autosend;
- new foreground submit resumes autosend;
- background-current sends background control before one queued submit.

### Rendering

- one item uses two rows;
- two items use three rows;
- three or more use exactly three rows with head/latest/overflow metadata;
- Unicode, CJK, combining marks, and emoji truncate without corruption;
- large paste previews show placeholders only;
- narrow and zero-width areas do not panic;
- preview never overlaps composer, search row, status, mention/slash popup, or
  hardware cursor;
- preview is absent in approval, setup, session picker, workflow, and agent
  panels.

### Regression

- transcript search keyboard/frame tests remain unchanged;
- composer IME cursor tests remain unchanged;
- compact popup tests remain unchanged;
- selection and transcript cache tests remain unchanged;
- workflow notification queue tests remain unchanged;
- dispatcher control-bypass and overflow tests remain unchanged;
- full `orca-tui` and workspace gates pass.

## Performance

- enqueue, FIFO promotion, and LIFO restore are O(1);
- preview rendering examines only the head, tail, and queue length;
- no frame scans all queued messages;
- no queued text enters `TranscriptRenderCache` before actual dispatch;
- no per-frame syntax parse, mention expansion, or large-paste expansion occurs.

A test with 64 long queued messages proves preview construction touches at most
two queue items.

## Delivery Gates

The implementation is complete only when:

1. strict RED/GREEN evidence exists for every behavior above;
2. focused queue/input/layout tests pass;
3. `cargo test -p orca-tui -- --test-threads=1` passes;
4. `cargo test --workspace --all-targets -- --test-threads=1` passes, with the
   existing proven-flaky exception process only if needed;
5. `cargo check -p orca-tui` passes;
6. `cargo fmt --all -- --check` passes;
7. the full feature range passes `git diff --check`;
8. independent spec and code-quality reviews report no Critical or Important
   findings;
9. every commit has exactly one final
   `Co-authored-by: TRAE CLI <noreply@bytedance.com>` trailer;
10. changed-file and symbol audits show no cwd/git status, Vim enhancement,
    configurable keybinding, `/doctor`, FPS, or onboarding leakage;
11. `feature/tui-syntax-highlighting` is pushed;
12. local and remote branch SHAs match.

mod agent_runner;
mod agent_subagent_execution;
mod agent_tool_execution;
mod agent_workflow_execution;
pub mod app;
mod approval_actions;
mod approval_dialog_actions;
mod background_approval;
mod background_tasks;
pub mod bridge;
pub mod commands;
mod composer_input_actions;
mod composer_textarea;
pub mod diff;
mod global_actions;
mod idle_navigation_actions;
mod idle_submit_actions;
mod mention_menu_actions;
mod running_actions;
mod runtime_event_projection;
mod runtime_interaction_adapter;
pub mod shortcuts;
mod slash_command_actions;
mod slash_menu_actions;
mod submitted_turn;
pub mod theme;
pub mod types;
pub mod ui;
pub mod vim;
mod workflow_notifications;
mod workflow_panel_actions;

pub use app::run_tui;

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_event_projection_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let projection =
            std::fs::read_to_string(format!("{manifest_dir}/src/runtime_event_projection.rs"))
                .expect("runtime event projection module should exist");
        assert!(
            projection.contains("pub(crate) fn tui_event_from_runtime_event"),
            "runtime_event_projection should export the TUI runtime event adapter"
        );

        let bridge = std::fs::read_to_string(format!("{manifest_dir}/src/bridge.rs"))
            .expect("bridge source should be readable");
        assert!(
            !bridge.contains("fn tui_event_from_runtime_event"),
            "bridge should call the shared TUI runtime event adapter instead of owning it"
        );
    }

    #[test]
    fn runtime_interaction_adapters_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let adapter =
            std::fs::read_to_string(format!("{manifest_dir}/src/runtime_interaction_adapter.rs"))
                .expect("runtime interaction adapter module should exist");
        assert!(
            adapter.contains("pub(crate) struct TuiApprovalHandler"),
            "runtime_interaction_adapter should own the TUI approval handler"
        );
        assert!(
            adapter.contains("pub(crate) struct TuiUserInputHandler"),
            "runtime_interaction_adapter should own the TUI user-input handler"
        );
        assert!(
            adapter.contains("pub(crate) fn resolve_tui_tool_approval"),
            "runtime_interaction_adapter should own the TUI tool approval gate"
        );
        assert!(
            adapter.contains("RuntimePendingInteractionRecord"),
            "runtime_interaction_adapter should project runtime-owned pending interaction records"
        );
        assert!(
            adapter.contains("fn approval_event_from_pending_interaction"),
            "runtime_interaction_adapter should map runtime pending approval records into TUI events"
        );
        assert!(
            adapter.contains("fn user_input_event_from_pending_interaction"),
            "runtime_interaction_adapter should map runtime pending user-input records into TUI events"
        );

        let bridge = std::fs::read_to_string(format!("{manifest_dir}/src/bridge.rs"))
            .expect("bridge source should be readable");
        assert!(
            !bridge.contains("struct TuiApprovalHandler"),
            "bridge should use the TUI approval adapter instead of owning it"
        );
        assert!(
            !bridge.contains("struct TuiUserInputHandler"),
            "bridge should use the TUI user-input adapter instead of owning it"
        );
        assert!(
            !bridge.contains("approval_request_for_invocation"),
            "bridge should delegate TUI approval request construction to the interaction adapter"
        );
        assert!(
            !bridge.contains("resolve_interactive_tool_approval"),
            "bridge should delegate interactive approval waits to the interaction adapter"
        );
    }

    #[test]
    fn tui_agent_runner_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner module should exist");
        assert!(
            runner.contains("pub fn run_agent_for_tui"),
            "agent_runner should own the TUI agent turn loop entrypoint"
        );

        let bridge = std::fs::read_to_string(format!("{manifest_dir}/src/bridge.rs"))
            .expect("bridge source should be readable");
        assert!(
            !bridge.contains("pub fn run_agent_for_tui"),
            "bridge should not own the TUI agent turn loop"
        );
    }

    #[test]
    fn tui_submitted_turn_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let submitted_turn =
            std::fs::read_to_string(format!("{manifest_dir}/src/submitted_turn.rs"))
                .expect("submitted_turn module should exist");
        assert!(
            submitted_turn.contains("pub(crate) struct SubmittedTurn"),
            "submitted_turn should own the submitted-turn boundary"
        );
        assert!(
            submitted_turn.contains("enum SubmittedTurnKind"),
            "submitted_turn should keep prompt/source state inside SubmittedTurnKind"
        );
        assert!(
            submitted_turn.contains("struct SubmittedTurnPresentation"),
            "submitted_turn should own TUI submitted-turn presentation metadata"
        );
        assert!(
            !submitted_turn.contains("pub(crate) struct SubmittedTurnPresentation"),
            "SubmittedTurnPresentation should stay private behind SubmittedTurn accessors"
        );
        assert!(
            submitted_turn.contains("pub(crate) fn task_label(&self) -> Option<&str>")
                && submitted_turn.contains("pub(crate) fn is_backtrack_target(&self) -> bool"),
            "submitted_turn should expose presentation policy through SubmittedTurn methods"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nstruct SubmittedTurn {"),
            "app should use the submitted_turn module instead of defining the boundary inline"
        );
        assert!(
            !app.contains("\nenum SubmittedTurnKind {"),
            "app should not own submitted-turn prompt/source variants"
        );
    }

    #[test]
    fn tui_background_approval_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let background_approval =
            std::fs::read_to_string(format!("{manifest_dir}/src/background_approval.rs"))
                .expect("background_approval module should exist");
        assert!(
            background_approval
                .contains("pub(crate) fn submit_background_approval_response_for_tui"),
            "background_approval should own TUI background approval response submission"
        );
        assert!(
            background_approval.contains("TuiBackgroundTurnContinuationRequest"),
            "background_approval should return the typed background continuation request"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn submit_background_approval_response_for_tui("),
            "app should use the background_approval module instead of defining approval submission inline"
        );
    }

    #[test]
    fn tui_background_task_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let background_tasks =
            std::fs::read_to_string(format!("{manifest_dir}/src/background_tasks.rs"))
                .expect("background_tasks module should exist");
        assert!(
            background_tasks.contains("pub(crate) fn stop_task_for_tui"),
            "background_tasks should own TUI task stop execution"
        );
        assert!(
            background_tasks.contains("pub(crate) fn foreground_task_for_tui"),
            "background_tasks should own TUI task foreground execution"
        );
        assert!(
            background_tasks
                .contains("pub(crate) fn notify_recovered_background_approvals_for_tui"),
            "background_tasks should own recovered background approval notifications"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn stop_task_for_tui("),
            "app should use the background_tasks module instead of defining stop execution inline"
        );
        assert!(
            !app.contains("\nfn foreground_task_for_tui("),
            "app should use the background_tasks module instead of defining foreground execution inline"
        );
        assert!(
            !app.contains("\nfn notify_recovered_background_approvals_for_tui("),
            "app should use the background_tasks module instead of defining recovery notifications inline"
        );
    }

    #[test]
    fn tui_workflow_panel_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workflow_panel_actions =
            std::fs::read_to_string(format!("{manifest_dir}/src/workflow_panel_actions.rs"))
                .expect("workflow_panel_actions module should exist");
        assert!(
            workflow_panel_actions.contains("pub(crate) fn handle_workflows_panel_key"),
            "workflow_panel_actions should own workflow panel key handling"
        );
        assert!(
            workflow_panel_actions.contains("fn selected_stoppable_task("),
            "workflow_panel_actions should keep stop eligibility local to panel actions"
        );
        assert!(
            workflow_panel_actions.contains("fn selected_foregroundable_task("),
            "workflow_panel_actions should keep foreground eligibility local to panel actions"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn handle_workflows_panel_key("),
            "app should use the workflow_panel_actions module instead of defining panel key handling inline"
        );
        assert!(
            !app.contains("\nfn selected_stoppable_task("),
            "app should not own workflow task stop eligibility"
        );
        assert!(
            !app.contains("\nfn selected_foregroundable_task("),
            "app should not own workflow task foreground eligibility"
        );
    }

    #[test]
    fn tui_running_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let running_actions =
            std::fs::read_to_string(format!("{manifest_dir}/src/running_actions.rs"))
                .expect("running_actions module should exist");
        assert!(
            running_actions.contains("pub(crate) fn handle_running_shortcut"),
            "running_actions should own running-state shortcut execution"
        );
        assert!(
            running_actions.contains("RunningShortcut::BackgroundCurrentTurn")
                && running_actions.contains("RunningShortcut::Interrupt"),
            "running_actions should dispatch background and interrupt actions"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn handle_running_shortcut("),
            "app should use the running_actions module instead of defining running shortcut execution inline"
        );
    }

    #[test]
    fn tui_global_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let global_actions =
            std::fs::read_to_string(format!("{manifest_dir}/src/global_actions.rs"))
                .expect("global_actions module should exist");
        assert!(
            global_actions.contains("pub(crate) fn handle_global_shortcut"),
            "global_actions should own global shortcut handling"
        );
        assert!(
            global_actions.contains("GlobalShortcutFlow::Exit(130)"),
            "global_actions should own double-Ctrl-C exit flow"
        );
        assert!(
            global_actions.contains("UserAction::Interrupt"),
            "global_actions should own running interrupt dispatch"
        );
        assert!(
            global_actions.contains("clear_terminal"),
            "global_actions should own clear-screen terminal callback"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("GlobalShortcut::Cancel =>"),
            "app should use global_actions instead of matching cancel inline"
        );
        assert!(
            !app.contains("GlobalShortcut::ClearScreen =>"),
            "app should use global_actions instead of matching clear-screen inline"
        );
        assert!(
            !app.contains("state.toggle_shortcuts();"),
            "app should use global_actions instead of toggling shortcuts inline"
        );
    }

    #[test]
    fn tui_composer_textarea_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let composer_textarea =
            std::fs::read_to_string(format!("{manifest_dir}/src/composer_textarea.rs"))
                .expect("composer_textarea module should exist");
        assert!(
            composer_textarea.contains("pub(crate) fn make_textarea"),
            "composer_textarea should own normal composer construction"
        );
        assert!(
            composer_textarea.contains("pub(crate) fn make_textarea_with_text"),
            "composer_textarea should own prefilled composer construction"
        );
        assert!(
            composer_textarea.contains("pub(crate) fn textarea_text"),
            "composer_textarea should own composer text extraction"
        );
        assert!(
            composer_textarea.contains("pub(crate) fn insert_pasted_text"),
            "composer_textarea should own paste insertion behavior"
        );
        assert!(
            composer_textarea.contains("pub(crate) fn make_setup_textarea"),
            "composer_textarea should own setup composer construction"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn make_textarea<"),
            "app should use the composer_textarea module instead of defining normal composer construction inline"
        );
        assert!(
            !app.contains("\nfn make_textarea_with_text<"),
            "app should use the composer_textarea module instead of defining prefilled composer construction inline"
        );
        assert!(
            !app.contains("\nfn textarea_text("),
            "app should use the composer_textarea module instead of defining text extraction inline"
        );
        assert!(
            !app.contains("\nfn insert_pasted_text("),
            "app should use the composer_textarea module instead of defining paste behavior inline"
        );
        assert!(
            !app.contains("\nfn make_setup_textarea<"),
            "app should use the composer_textarea module instead of defining setup composer construction inline"
        );
    }

    #[test]
    fn tui_composer_input_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let composer_input_actions =
            std::fs::read_to_string(format!("{manifest_dir}/src/composer_input_actions.rs"))
                .expect("composer_input_actions module should exist");
        assert!(
            composer_input_actions.contains("pub(crate) fn refresh_input_menus"),
            "composer_input_actions should own slash/mention refresh after composer edits"
        );
        assert!(
            composer_input_actions.contains("pub(crate) fn insert_composer_newline"),
            "composer_input_actions should own composer newline handling"
        );
        assert!(
            composer_input_actions.contains("pub(crate) fn recall_previous_history"),
            "composer_input_actions should own previous-history recall"
        );
        assert!(
            composer_input_actions.contains("pub(crate) fn recall_next_history"),
            "composer_input_actions should own next-history recall"
        );
        assert!(
            composer_input_actions.contains("pub(crate) fn apply_composer_key_input"),
            "composer_input_actions should own plain composer key input and file mention completion"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("mentions::complete_file_mention"),
            "app should use the composer_input_actions module instead of completing file mentions inline"
        );
        assert!(
            !app.contains(".history_previous("),
            "app should use the composer_input_actions module instead of recalling previous history inline"
        );
        assert!(
            !app.contains(".history_next("),
            "app should use the composer_input_actions module instead of recalling next history inline"
        );
    }

    #[test]
    fn tui_idle_submit_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let idle_submit_actions =
            std::fs::read_to_string(format!("{manifest_dir}/src/idle_submit_actions.rs"))
                .expect("idle_submit_actions module should exist");
        assert!(
            idle_submit_actions.contains("pub(crate) fn handle_idle_submit"),
            "idle_submit_actions should own idle Enter submit handling"
        );
        assert!(
            idle_submit_actions.contains("fn reset_composer_after_submit"),
            "idle_submit_actions should own composer reset after submit"
        );
        assert!(
            idle_submit_actions.contains("UserAction::RespondToUserInput"),
            "idle_submit_actions should own user-input answer submission"
        );
        assert!(
            idle_submit_actions.contains("UserAction::Submit"),
            "idle_submit_actions should own normal prompt submission"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("state.record_prompt(text.clone())"),
            "app should use idle_submit_actions instead of recording submitted prompts inline"
        );
        assert!(
            !app.contains("answer: text"),
            "app should use idle_submit_actions instead of sending user-input answers inline"
        );
        assert!(
            !app.contains("UserAction::Submit(text)"),
            "app should use idle_submit_actions instead of sending normal prompt submissions inline"
        );
    }

    #[test]
    fn tui_idle_navigation_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let idle_navigation_actions =
            std::fs::read_to_string(format!("{manifest_dir}/src/idle_navigation_actions.rs"))
                .expect("idle_navigation_actions module should exist");
        assert!(
            idle_navigation_actions.contains("pub(crate) fn handle_idle_navigation_shortcut"),
            "idle_navigation_actions should own idle navigation shortcut handling"
        );
        assert!(
            idle_navigation_actions.contains("UserAction::Backtrack"),
            "idle_navigation_actions should own backtrack shortcut dispatch"
        );
        assert!(
            idle_navigation_actions.contains("toggle_latest_tool_output"),
            "idle_navigation_actions should own tool output expansion"
        );
        assert!(
            idle_navigation_actions.contains("visible_height.saturating_sub(2)"),
            "idle_navigation_actions should own page scrolling size"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("state.scroll_up(page)"),
            "app should use idle_navigation_actions instead of paging inline"
        );
        assert!(
            !app.contains("action_tx.send(UserAction::Backtrack)"),
            "app should use idle_navigation_actions instead of dispatching backtrack inline"
        );
        assert!(
            !app.contains("toggle_latest_tool_output"),
            "app should use idle_navigation_actions instead of toggling tool output inline"
        );
    }

    #[test]
    fn tui_mention_menu_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let mention_menu_actions =
            std::fs::read_to_string(format!("{manifest_dir}/src/mention_menu_actions.rs"))
                .expect("mention_menu_actions module should exist");
        assert!(
            mention_menu_actions.contains("pub(crate) fn update_mention_candidates"),
            "mention_menu_actions should own mention candidate refresh"
        );
        assert!(
            mention_menu_actions.contains("pub(crate) fn handle_mention_menu_key"),
            "mention_menu_actions should own mention menu key handling"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn update_mention_candidates("),
            "app should use the mention_menu_actions module instead of defining mention refresh inline"
        );
        assert!(
            !app.contains("\nfn handle_mention_menu_key("),
            "app should use the mention_menu_actions module instead of defining mention key handling inline"
        );
    }

    #[test]
    fn tui_slash_command_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let slash_command_actions =
            std::fs::read_to_string(format!("{manifest_dir}/src/slash_command_actions.rs"))
                .expect("slash_command_actions module should exist");
        assert!(
            slash_command_actions.contains("pub(crate) fn handle_slash_command"),
            "slash_command_actions should own slash command execution"
        );
        assert!(
            slash_command_actions.contains("pub(crate) enum SlashOutcome"),
            "slash_command_actions should own the slash execution outcome"
        );
        assert!(
            slash_command_actions.contains("pub(crate) fn parse_approval_mode"),
            "slash_command_actions should own slash approval mode parsing"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn handle_slash_command("),
            "app should use the slash_command_actions module instead of defining slash execution inline"
        );
        assert!(
            !app.contains("\nenum SlashOutcome"),
            "app should use the slash_command_actions module instead of defining slash outcomes inline"
        );
        assert!(
            !app.contains("\nfn parse_approval_mode("),
            "app should use the slash_command_actions module instead of defining mode parsing inline"
        );
    }

    #[test]
    fn tui_slash_menu_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let slash_menu_actions =
            std::fs::read_to_string(format!("{manifest_dir}/src/slash_menu_actions.rs"))
                .expect("slash_menu_actions module should exist");
        assert!(
            slash_menu_actions.contains("pub(crate) fn update_slash_menu"),
            "slash_menu_actions should own slash menu candidate refresh"
        );
        assert!(
            slash_menu_actions.contains("pub(crate) fn handle_slash_menu_key"),
            "slash_menu_actions should own slash menu key handling"
        );
        assert!(
            slash_menu_actions.contains("fn select_slash_menu_command"),
            "slash_menu_actions should own selected slash menu command dispatch"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn update_slash_menu("),
            "app should use the slash_menu_actions module instead of defining slash refresh inline"
        );
        assert!(
            !app.contains("\nfn handle_slash_menu_key("),
            "app should use the slash_menu_actions module instead of defining slash key handling inline"
        );
        assert!(
            !app.contains("\nfn select_slash_menu_command("),
            "app should use the slash_menu_actions module instead of defining slash selection inline"
        );
    }

    #[test]
    fn tui_approval_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let approval_actions =
            std::fs::read_to_string(format!("{manifest_dir}/src/approval_actions.rs"))
                .expect("approval_actions module should exist");
        assert!(
            approval_actions.contains("pub(crate) fn resolve_approval_option"),
            "approval_actions should own TUI approval option resolution"
        );
        assert!(
            approval_actions.contains("fn resolve_approval("),
            "approval_actions should keep the raw approve/deny action dispatch private"
        );
        assert!(
            approval_actions.contains("ApprovalOption::AlwaysTool")
                && approval_actions.contains("ApprovalOption::AlwaysTarget"),
            "approval_actions should own approval allowlist updates for persistent choices"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn resolve_approval_option("),
            "app should use the approval_actions module instead of defining approval option resolution inline"
        );
        assert!(
            !app.contains("\nfn resolve_approval("),
            "app should use the approval_actions module instead of defining approval action dispatch inline"
        );
    }

    #[test]
    fn tui_approval_dialog_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let approval_dialog_actions =
            std::fs::read_to_string(format!("{manifest_dir}/src/approval_dialog_actions.rs"))
                .expect("approval_dialog_actions module should exist");
        assert!(
            approval_dialog_actions.contains("pub(crate) fn handle_approval_dialog_key"),
            "approval_dialog_actions should own approval dialog key handling"
        );
        assert!(
            approval_dialog_actions.contains("option_for_key"),
            "approval_dialog_actions should own direct numeric and legacy option keys"
        );
        assert!(
            approval_dialog_actions.contains("ApprovalShortcut::ToggleSelection"),
            "approval_dialog_actions should own dialog selection movement"
        );
        assert!(
            approval_dialog_actions.contains("resolve_approval_option"),
            "approval_dialog_actions should resolve selected approval options"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("option_for_key(c)"),
            "app should use approval_dialog_actions instead of resolving direct approval keys inline"
        );
        assert!(
            !app.contains("ApprovalShortcut::ToggleSelection"),
            "app should use approval_dialog_actions instead of moving dialog selection inline"
        );
        assert!(
            !app.contains("ApprovalShortcut::Confirm"),
            "app should use approval_dialog_actions instead of confirming dialog selection inline"
        );
    }

    #[test]
    fn tui_workflow_notifications_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workflow_notifications =
            std::fs::read_to_string(format!("{manifest_dir}/src/workflow_notifications.rs"))
                .expect("workflow_notifications module should exist");
        assert!(
            workflow_notifications.contains("pub(crate) fn submit_pending_workflow_notification"),
            "workflow_notifications should own pending notification submission"
        );
        assert!(
            workflow_notifications.contains("pub(crate) fn queue_workflow_terminal_notification"),
            "workflow_notifications should own terminal notification queueing"
        );
        assert!(
            workflow_notifications
                .contains("pub(crate) fn remove_pending_workflow_notification_by_id"),
            "workflow_notifications should own pending notification removal by id"
        );
        assert!(
            workflow_notifications.contains("pub(crate) fn drain_pending_workflow_notifications"),
            "workflow_notifications should own cross-thread notification draining"
        );
        assert!(
            workflow_notifications.contains("pub(crate) fn is_workflow_notification_turn_boundary"),
            "workflow_notifications should own workflow notification turn-boundary detection"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn submit_pending_workflow_notification("),
            "app should use the workflow_notifications module instead of defining pending notification submission inline"
        );
        assert!(
            !app.contains("\nfn queue_workflow_terminal_notification("),
            "app should use the workflow_notifications module instead of defining terminal notification queueing inline"
        );
        assert!(
            !app.contains("\nfn remove_pending_workflow_notification_by_id("),
            "app should use the workflow_notifications module instead of defining notification removal inline"
        );
        assert!(
            !app.contains("\nfn drain_pending_workflow_notifications("),
            "app should use the workflow_notifications module instead of defining notification draining inline"
        );
    }

    #[test]
    fn approved_background_continuation_is_owned_by_runtime() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner source should be readable");
        assert!(
            runner.contains("take_approved_background_turn_continuation"),
            "agent_runner should ask runtime for approved background turn continuations"
        );
        assert!(
            !runner.contains(".take_approved_pending_provider_response("),
            "agent_runner should not directly consume pending provider responses from TaskRegistry"
        );
        assert!(
            !runner.contains("fn provider_response_first_tool_call_id"),
            "agent_runner should not derive preapproved tool call ids; runtime owns that boundary"
        );
        assert!(
            runner.contains("into_runtime_turn_continuation"),
            "agent_runner should convert approved background continuations into runtime turn continuations"
        );
        assert!(
            runner.contains("with_continuation"),
            "agent_runner should resume approved background turns through a runtime ThreadTurnRequest continuation"
        );
        assert!(
            !runner.contains("execute_preapproved_tool_for_tui"),
            "approved background continuation should not use a renderer-owned preapproved tool loop"
        );
    }

    #[test]
    fn tui_main_session_task_status_uses_runtime_task_status_event() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner source should be readable");

        assert!(
            runner.contains("fn send_task_status_updated_for_tui"),
            "agent_runner should expose a single-task runtime status event helper"
        );
        assert!(
            runner.contains("events.task_status_updated(task)"),
            "TUI task status helper must emit task.status.updated runtime events"
        );
        assert!(
            runner.contains("send_task_status_updated_for_tui(event_tx, events, &task);"),
            "main session task start should announce the concrete task status event"
        );
        assert!(
            runner.contains(
                "send_task_status_updated_for_tui(event_tx, events, &backgrounded_task);"
            ),
            "backgrounding a main session should announce the concrete task status event"
        );
        assert!(
            runner.contains("send_task_status_updated_for_tui(event_tx, events, &finished_task);"),
            "main session completion should announce the concrete task status event"
        );
        assert!(
            runner.contains(
                "send_task_status_updated_for_tui(&event_tx, &mut events, &updated_task);"
            ),
            "background provider completion should announce the concrete task status event"
        );
        assert!(
            runner.contains(
                "send_task_status_updated_for_tui(event_tx, &mut runtime_events, &continued_task);"
            ),
            "approved background turn continuation should announce the concrete task status event"
        );
    }

    #[test]
    fn tui_agent_tool_execution_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let execution =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_tool_execution.rs"))
                .expect("TUI agent tool execution module should exist");
        assert!(
            execution.contains("pub(crate) fn execute_tool_for_tui"),
            "agent_tool_execution should own the TUI tool execution entrypoint"
        );

        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner source should be readable");
        assert!(
            !runner.contains("fn execute_tool_for_tui"),
            "agent_runner should not own TUI tool execution helpers"
        );
    }

    #[test]
    fn tui_goal_updates_use_runtime_thread_extension_guard() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let bridge = std::fs::read_to_string(format!("{manifest_dir}/src/bridge.rs"))
            .expect("bridge source should be readable");
        assert!(
            bridge.contains("thread_extensions"),
            "TUI session should expose RuntimeThread thread extension state"
        );

        let execution =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_tool_execution.rs"))
                .expect("TUI agent tool execution source should be readable");
        assert!(
            execution.contains("validate_goal_terminal_update_against_extensions"),
            "TUI goal update handler must guard terminal updates with live runtime extension state"
        );

        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner source should be readable");
        assert!(
            runner.contains("record_tui_goal_tool_finish"),
            "TUI agent runner must record completed tools into live runtime thread extension state"
        );
        assert!(
            runner.contains("RuntimeTurnReducer"),
            "TUI completed-tool recording should route through the runtime turn reducer"
        );
        assert!(
            !runner.contains("goals::record_goal_tool_finish"),
            "TUI should not write goal progress directly; runtime turn reducer owns tool finish state"
        );
    }

    #[test]
    fn tui_agent_workflow_execution_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workflow =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_workflow_execution.rs"))
                .expect("TUI agent workflow execution module should exist");
        assert!(
            workflow.contains("pub(crate) fn execute_workflow_for_tui"),
            "agent_workflow_execution should own the TUI workflow execution entrypoint"
        );

        let execution =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_tool_execution.rs"))
                .expect("TUI agent tool execution source should be readable");
        assert!(
            !execution.contains("fn execute_workflow_for_tui"),
            "agent_tool_execution should not own TUI workflow helpers"
        );
    }

    #[test]
    fn tui_workflow_task_status_uses_runtime_task_status_event() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workflow =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_workflow_execution.rs"))
                .expect("TUI agent workflow execution module should exist");

        assert!(
            workflow.contains("fn send_workflow_task_status_for_tui"),
            "workflow execution should centralize single-task status announcements"
        );
        assert!(
            workflow.contains("send_task_status_updated_for_tui"),
            "workflow task status should announce concrete task status runtime events"
        );
        assert!(
            workflow.contains("task_summary_for_tui"),
            "workflow task status should load the concrete task summary before notifying"
        );
        assert!(
            workflow.contains("send_task_status_updated_for_tui(event_tx, events, &task);"),
            "workflow task status helper must emit task.status.updated runtime events"
        );
        assert!(
            workflow.contains("send_workflow_tasks_updated_for_tui"),
            "workflow progress refreshes should keep the aggregate workflow task-list event"
        );
    }

    #[test]
    fn tui_agent_subagent_execution_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("pub(crate) fn execute_subagent_batch_for_tui"),
            "agent_subagent_execution should own the TUI subagent batch entrypoint"
        );
        assert!(
            subagent.contains("pub(crate) fn execute_subagent_for_tui"),
            "agent_subagent_execution should own the TUI subagent execution entrypoint"
        );
        assert!(
            subagent.contains("pub(crate) fn execute_subagent_status_for_tui"),
            "agent_subagent_execution should own the TUI subagent status entrypoint"
        );

        let execution =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_tool_execution.rs"))
                .expect("TUI agent tool execution source should be readable");
        assert!(
            !execution.contains("fn execute_subagent_for_tui"),
            "agent_tool_execution should not own TUI subagent helpers"
        );
        assert!(
            !execution.contains("fn execute_subagent_batch_for_tui"),
            "agent_tool_execution should not own TUI subagent batch helpers"
        );
    }

    #[test]
    fn tui_subagent_task_status_uses_runtime_task_status_event() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");

        assert!(
            subagent.contains("send_task_status_updated_for_tui"),
            "subagent task status should announce concrete task status runtime events"
        );
        assert!(
            subagent.contains("task_summary_for_tui"),
            "subagent task status should load the concrete task summary before notifying"
        );
        assert!(
            !subagent.contains("send_workflow_tasks_updated_for_tui"),
            "subagent task status should not borrow the workflow task-list event"
        );
    }

    #[test]
    fn tui_subagent_results_use_runtime_child_agent_result() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner source should be readable");
        assert!(
            !runner.contains("struct TuiAgentResult"),
            "agent_runner should not own the child-agent result type"
        );

        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("ChildAgentResult"),
            "agent_subagent_execution should use the runtime child-agent result type"
        );
    }

    #[test]
    fn tui_subagent_child_runs_delegate_model_override_to_runtime() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("run_child_agent_prompt_with_tool_executor"),
            "agent_subagent_execution should delegate child-agent model/cost setup to runtime"
        );
        assert!(
            !subagent.contains("run_child_agent_with_executor"),
            "agent_subagent_execution should not call the low-level child-agent executor wrapper"
        );
        assert!(
            !subagent.contains("with_subagent_override"),
            "agent_subagent_execution should not own child-agent model override logic"
        );
    }

    #[test]
    fn tui_subagent_child_request_construction_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("run_child_agent_prompt_with_tool_executor"),
            "agent_subagent_execution should delegate child request construction to runtime"
        );
        assert!(
            !subagent.contains("ChildAgentRequest::new"),
            "agent_subagent_execution should not construct child requests directly"
        );
    }

    #[test]
    fn tui_subagent_child_loop_setup_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("run_child_agent_prompt_with_tool_executor"),
            "agent_subagent_execution should delegate child loop orchestration to runtime"
        );
        assert!(
            !subagent.contains("prepare_child_agent_loop"),
            "agent_subagent_execution should not prepare child loop setup directly"
        );
        assert!(
            !subagent.contains("ProviderConfig"),
            "agent_subagent_execution should not construct child provider config"
        );
        assert!(
            !subagent.contains("Conversation::new"),
            "agent_subagent_execution should not bootstrap child conversation"
        );
        assert!(
            !subagent.contains("build_agent_system_prompt"),
            "agent_subagent_execution should not own child system prompt construction"
        );
    }

    #[test]
    fn tui_subagent_child_model_routing_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("route_child_agent_model"),
            "agent_subagent_execution should not route child models directly"
        );
        assert!(
            !subagent.contains("ModelRouteContext"),
            "agent_subagent_execution should not construct child model route context"
        );
        assert!(
            !subagent.contains("set_model"),
            "agent_subagent_execution should not update child cost model directly"
        );
    }

    #[test]
    fn tui_subagent_child_compaction_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("compact_child_agent_conversation_if_needed"),
            "agent_subagent_execution should not trigger child compaction directly"
        );
        assert!(
            !subagent.contains("needs_compaction_wire"),
            "agent_subagent_execution should not decide child compaction thresholds"
        );
        assert!(
            !subagent.contains("HookEvent::OnBudgetWarning"),
            "agent_subagent_execution should not run child budget-warning hooks directly"
        );
    }

    #[test]
    fn tui_subagent_child_provider_error_handling_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("handle_child_agent_provider_error"),
            "agent_subagent_execution should not handle child provider errors directly"
        );
        assert!(
            !subagent.contains("is_prompt_too_long_error"),
            "agent_subagent_execution should not classify prompt-too-long provider errors"
        );
        assert!(
            !subagent.contains("orca_provider::context::compact("),
            "agent_subagent_execution should not compact child conversations directly"
        );
    }

    #[test]
    fn tui_subagent_child_provider_turn_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("run_child_agent_provider_turn"),
            "agent_subagent_execution should not run child provider turns directly"
        );
        assert!(
            !subagent.contains("call_streaming"),
            "agent_subagent_execution should not call providers directly"
        );
        assert!(
            !subagent.contains("HookEvent::PreModelCall")
                && !subagent.contains("HookEvent::PostModelCall"),
            "agent_subagent_execution should not run child model hooks directly"
        );
        assert!(
            !subagent.contains("conversation_with_hook_context"),
            "agent_subagent_execution should not build child model hook conversations"
        );
    }

    #[test]
    fn tui_subagent_child_provider_response_folding_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("fold_child_agent_provider_response"),
            "agent_subagent_execution should not fold child provider responses directly"
        );
        assert!(
            !subagent.contains("add_usage"),
            "agent_subagent_execution should not update child provider usage directly"
        );
        assert!(
            !subagent.contains("tool_calls.is_empty"),
            "agent_subagent_execution should not decide terminal provider response state"
        );
        assert!(
            !subagent.contains("add_assistant"),
            "agent_subagent_execution should not record child assistant responses directly"
        );
    }

    #[test]
    fn tui_subagent_child_tool_result_folding_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("fold_child_agent_tool_result"),
            "agent_subagent_execution should not fold child tool results directly"
        );
        assert!(
            !subagent.contains("child_cost_tracker.merge"),
            "agent_subagent_execution should not merge child tool costs directly"
        );
        assert!(
            !subagent.contains("format_tool_result_for_model"),
            "agent_subagent_execution should not format child tool results for model context"
        );
        assert!(
            !subagent.contains("add_tool_result"),
            "agent_subagent_execution should not record child tool results directly"
        );
    }

    #[test]
    fn tui_subagent_child_turn_budget_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("advance_child_agent_turn"),
            "agent_subagent_execution should not advance child turns directly"
        );
        assert!(
            !subagent.contains("DEFAULT_MAX_TURNS"),
            "agent_subagent_execution should not own child max-turn limits"
        );
        assert!(
            !subagent.contains("turn += 1"),
            "agent_subagent_execution should not advance child turn counters directly"
        );
        assert!(
            !subagent.contains("RunStatus::BudgetExhausted"),
            "agent_subagent_execution should not construct child budget-exhausted results"
        );
    }

    #[test]
    fn tui_subagent_child_loop_state_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("let mut turn"),
            "agent_subagent_execution should not own child turn state"
        );
        assert!(
            !subagent.contains("reactive_compacted"),
            "agent_subagent_execution should not own child reactive compaction state"
        );
    }

    #[test]
    fn tui_subagent_child_tool_executor_uses_runtime_tool_context() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("tool_context.policy"),
            "agent_subagent_execution should consume a narrow runtime tool context"
        );
        assert!(
            subagent.contains("tool_context.mcp_registry"),
            "agent_subagent_execution should consume runtime MCP context through tool context"
        );
        assert!(
            !subagent.contains("setup.policy") && !subagent.contains("setup.mcp_registry"),
            "agent_subagent_execution should not depend on child loop setup internals"
        );
    }

    #[test]
    fn tui_subagent_child_tool_request_extraction_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("child_agent_tool_requests"),
            "agent_subagent_execution should not extract child provider tool calls directly"
        );
        assert!(
            !subagent.contains("ProviderStep"),
            "agent_subagent_execution should not inspect provider steps directly"
        );
        assert!(
            !subagent.contains("response.steps"),
            "agent_subagent_execution should not iterate provider response steps directly"
        );
    }
}

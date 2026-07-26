use std::collections::BTreeSet;

const MANIFEST: &str = include_str!(
    "../../../docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json"
);
const TYPES: &str = include_str!("types.rs");
const APP: &str = include_str!("app.rs");
const SURFACE_PROJECTION: &str = include_str!("surface_projection.rs");
const SUBMITTED_TURN: &str = include_str!("submitted_turn.rs");
const MENTION_SEARCH_MANAGER: &str = include_str!("mention_search_manager.rs");
const BACKGROUND_TASKS: &str = include_str!("background_tasks.rs");
const BACKGROUND_APPROVAL: &str = include_str!("background_approval.rs");
const SLASH_COMMAND_ACTIONS: &str = include_str!("slash_command_actions.rs");
const SLASH_MENU_ACTIONS: &str = include_str!("slash_menu_actions.rs");
const SESSION_PICKER_ACTIONS: &str = include_str!("session_picker_actions.rs");
const SETUP_ACTIONS: &str = include_str!("setup_actions.rs");

const CURRENT_ACTIONS: [(&str, &str); 21] = [
    ("Submit", "runtime_mutation"),
    ("SubmitWithMentions", "runtime_mutation"),
    ("SubmitWorkflowNotification", "runtime_mutation"),
    ("RunWorkflow", "workflow_mutation"),
    ("SetModel", "settings_mutation"),
    ("Remember", "host_store_and_thread_mutation"),
    ("Compact", "runtime_mutation"),
    ("GoalShow", "authoritative_read"),
    ("GoalSet", "goal_and_operation_mutation"),
    ("GoalEdit", "goal_mutation"),
    ("GoalClear", "goal_mutation"),
    ("GoalPause", "goal_and_operation_mutation"),
    ("GoalResume", "goal_session_operation_mutation"),
    ("ResolveBackgroundApproval", "interaction_mutation"),
    ("StopTask", "task_mutation"),
    ("ForegroundTask", "task_ownership_mutation"),
    ("RespondToInteraction", "interaction_mutation"),
    ("Backtrack", "history_mutation"),
    ("BackgroundCurrentTurn", "operation_ownership_mutation"),
    ("Interrupt", "operation_mutation"),
    ("Cancel", "host_lifecycle_mutation"),
];

const FUTURE_ACTIONS: [&str; 2] = ["ResumeOperation", "CancelOperation"];

const TUI_ENTRYPOINTS: [&str; 38] = [
    "slash.model_write",
    "slash.model_read",
    "slash.mode_plan_and_backtab",
    "slash.config_show",
    "slash.cost",
    "slash.goal",
    "slash.workflow_run",
    "slash.workflow_and_agent_panels",
    "slash.skills_list",
    "slash.dynamic_skill",
    "slash.remember",
    "slash.compact",
    "slash.history",
    "slash.trust_show",
    "slash.trust_mutation",
    "slash_menu.discovery",
    "dispatcher.route_action",
    "operation_controller.controls",
    "provider_suspension.poll",
    "run_hosted_operation",
    "hosted_event_observer",
    "approval_handler",
    "permission_handler",
    "user_input_handler",
    "mcp_elicitation_handler",
    "approval_always",
    "background_approval_reconstruction",
    "workflow_result_autosubmit",
    "background_task_callbacks",
    "recovered_background_scan",
    "startup_session_mcp",
    "session_picker_transition",
    "goal_callbacks",
    "mention_catalog_expansion",
    "setup_api_key",
    "app_state_update",
    "input_history",
    "terminal_clipboard_notifications",
];

fn rust_char_literal_len(source: &str) -> Option<usize> {
    let mut chars = source.char_indices();
    if chars.next()?.1 != '\'' {
        return None;
    }
    let (_, character) = chars.next()?;
    if character == '\\' {
        let (_, escape) = chars.next()?;
        match escape {
            '0' | 'n' | 'r' | 't' | '\\' | '\'' | '"' => {}
            'x' => {
                for _ in 0..2 {
                    if !chars.next()?.1.is_ascii_hexdigit() {
                        return None;
                    }
                }
            }
            'u' => {
                if chars.next()?.1 != '{' {
                    return None;
                }
                let mut digits = 0;
                loop {
                    match chars.next()?.1 {
                        '}' if (1..=6).contains(&digits) => break,
                        '_' => {}
                        digit if digit.is_ascii_hexdigit() => digits += 1,
                        _ => return None,
                    }
                }
            }
            _ => return None,
        }
    } else if character == '\'' || character.is_control() {
        return None;
    }
    let (closing_index, closing) = chars.next()?;
    (closing == '\'').then_some(closing_index + closing.len_utf8())
}

fn enum_variants(source: &str, declaration: &str) -> Vec<String> {
    let uncommented = strip_rust_comments(source);
    let body = uncommented
        .split(declaration)
        .nth(1)
        .unwrap_or_else(|| panic!("missing {declaration}"));
    let mut variants = Vec::new();
    let mut chunk = String::new();
    let mut brace_depth = 0_i32;
    let mut paren_depth = 0_i32;
    let mut bracket_depth = 0_i32;
    let mut quote = None;
    let mut escaped = false;

    let mut index = 0;
    while index < body.len() {
        let ch = body[index..].chars().next().expect("character at boundary");
        let ch_len = ch.len_utf8();
        if let Some(delimiter) = quote {
            chunk.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == delimiter {
                quote = None;
            }
            index += ch_len;
            continue;
        }
        if ch == '"' {
            quote = Some(ch);
            chunk.push(ch);
            index += ch_len;
            continue;
        }
        if ch == '\'' {
            if let Some(len) = rust_char_literal_len(&body[index..]) {
                chunk.push_str(&body[index..index + len]);
                index += len;
                continue;
            }
        }
        match ch {
            '{' => brace_depth += 1,
            '}' if brace_depth == 0 && paren_depth == 0 && bracket_depth == 0 => {
                push_variant(&mut variants, &mut chunk);
                break;
            }
            '}' => brace_depth -= 1,
            '(' => paren_depth += 1,
            ')' => paren_depth -= 1,
            '[' => bracket_depth += 1,
            ']' => bracket_depth -= 1,
            ',' if brace_depth == 0 && paren_depth == 0 && bracket_depth == 0 => {
                push_variant(&mut variants, &mut chunk);
                index += ch_len;
                continue;
            }
            _ => {}
        }
        chunk.push(ch);
        index += ch_len;
    }
    variants
}

fn strip_rust_comments(source: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        Line,
        Block(usize),
        String,
    }

    let mut output = String::new();
    let mut state = State::Code;
    let mut escaped = false;
    let mut index = 0;
    while index < source.len() {
        let ch = source[index..]
            .chars()
            .next()
            .expect("character at boundary");
        let ch_len = ch.len_utf8();
        let next = source[index + ch_len..].chars().next();
        match state {
            State::Code if ch == '/' && next == Some('/') => {
                index += 2;
                state = State::Line;
                continue;
            }
            State::Code if ch == '/' && next == Some('*') => {
                index += 2;
                state = State::Block(1);
                continue;
            }
            State::Code => {
                if ch == '\'' {
                    if let Some(len) = rust_char_literal_len(&source[index..]) {
                        output.push_str(&source[index..index + len]);
                        index += len;
                        continue;
                    }
                }
                output.push(ch);
                if ch == '"' {
                    escaped = false;
                    state = State::String;
                }
            }
            State::Line if ch == '\n' => {
                output.push(ch);
                state = State::Code;
            }
            State::Line => {}
            State::Block(depth) if ch == '/' && next == Some('*') => {
                index += 2;
                state = State::Block(depth + 1);
                continue;
            }
            State::Block(depth) if ch == '*' && next == Some('/') => {
                index += 2;
                state = if depth == 1 {
                    State::Code
                } else {
                    State::Block(depth - 1)
                };
                continue;
            }
            State::Block(depth) => {
                if ch == '\n' {
                    output.push(ch);
                }
                state = State::Block(depth);
            }
            State::String => {
                output.push(ch);
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    state = State::Code;
                }
            }
        }
        index += ch_len;
    }
    output
}

fn push_variant(variants: &mut Vec<String>, chunk: &mut String) {
    let mut remaining = chunk.trim();
    while remaining.starts_with("#[") {
        let mut depth = 0_i32;
        let mut end = None;
        let mut quote = None;
        let mut escaped = false;
        let mut index = 1;
        while index < remaining.len() {
            let ch = remaining[index..]
                .chars()
                .next()
                .expect("character at boundary");
            let ch_len = ch.len_utf8();
            if let Some(delimiter) = quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == delimiter {
                    quote = None;
                }
                index += ch_len;
                continue;
            }
            match ch {
                '"' => quote = Some(ch),
                '\'' => {
                    if let Some(len) = rust_char_literal_len(&remaining[index..]) {
                        index += len;
                        continue;
                    }
                }
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
            index += ch_len;
        }
        let end = end.expect("terminated Rust enum attribute");
        remaining = remaining[end..].trim_start();
    }
    let name: String = remaining
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect();
    if !name.is_empty() {
        variants.push(name);
    }
    chunk.clear();
}

#[test]
fn enum_inventory_parser_handles_rust_syntax_and_final_variant_without_comma() {
    let source = r#"
pub enum Fixture {
    #[serde(rename = "tuple,renamed")]
    Tuple(String, Vec<u8>), // line comment, with comma
    /*
    FakeVariant,
    */
    /// doc comment, with comma
    Struct { value: Option<(u8, u8)> },
    #[cfg_attr(feature = "nested", serde(rename = "right]bracket"))]
    #[serde(rename = "right]bracket")]
    RightBracket,
    Final
}
"#;

    assert_eq!(
        enum_variants(source, "pub enum Fixture {"),
        ["Tuple", "Struct", "RightBracket", "Final"]
    );
    assert_enum_inventory_parser_distinguishes_lifetimes_from_character_literals();
}

fn assert_enum_inventory_parser_distinguishes_lifetimes_from_character_literals() {
    let source = r##"
pub enum LifetimeFixture<'a> {
    Existing,
    #[doc = "borrowed, static"]
    Hidden(&'static str),
    Named { value: &'a str },
    Character(char),
    Plain = 'x' as isize,
    Newline = '\n' as isize,
    Backslash = '\\' as isize,
    Quote = '\'' as isize,
    Unicode = '界' as isize,
}
"##;

    assert_eq!(
        enum_variants(source, "pub enum LifetimeFixture<'a> {"),
        [
            "Existing",
            "Hidden",
            "Named",
            "Character",
            "Plain",
            "Newline",
            "Backslash",
            "Quote",
            "Unicode",
        ]
    );
}

#[test]
fn user_actions_are_exactly_classified_with_required_recovery_variants() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("manifest JSON");
    let rows = manifest["tui_actions"].as_array().expect("tui_actions");
    let current_rows: Vec<(&str, &str)> = rows
        .iter()
        .filter(|row| row[1] == "current")
        .map(|row| {
            (
                row[0].as_str().expect("action id"),
                row[3].as_str().expect("action classification"),
            )
        })
        .collect();
    let current_variants = enum_variants(TYPES, "pub enum UserAction {");

    assert_eq!(current_rows, CURRENT_ACTIONS);
    let expected = CURRENT_ACTIONS
        .map(|(name, _)| name.to_string())
        .into_iter()
        .chain(FUTURE_ACTIONS.map(str::to_string))
        .collect::<Vec<_>>();
    assert_eq!(
        current_variants, expected,
        "UserAction drift requires an inventory review"
    );
}

#[test]
fn future_recovery_actions_are_separate_and_exact() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("manifest JSON");
    let rows = manifest["tui_actions"].as_array().expect("tui_actions");
    let additions: Vec<&str> = rows
        .iter()
        .filter(|row| row[1] == "required_addition")
        .map(|row| row[0].as_str().expect("future action id"))
        .collect();

    assert_eq!(additions, FUTURE_ACTIONS);
    assert_eq!(
        manifest["closed_inventory"]["required_tui_user_action_additions"]
            .as_array()
            .expect("required additions")
            .iter()
            .map(|value| value.as_str().expect("required addition"))
            .collect::<Vec<_>>(),
        FUTURE_ACTIONS
    );
}

#[test]
fn mutation_capable_entrypoints_have_a_closed_baseline_route() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("manifest JSON");
    let rows = manifest["tui_entrypoints"]
        .as_array()
        .expect("tui_entrypoints");
    let ids: Vec<&str> = rows
        .iter()
        .map(|row| row[0].as_str().expect("entrypoint id"))
        .collect();

    assert_eq!(ids, TUI_ENTRYPOINTS);
    assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
    for row in rows {
        let classification = row[2].as_str().expect("entrypoint classification");
        let mutation_capable = classification.contains("mutation")
            || classification.contains("authority")
            || classification.contains("router")
            || classification.contains("transition")
            || classification.contains("runtime_effect");
        if mutation_capable {
            assert!(!row[4].as_str().expect("target route").is_empty());
            assert!(!row[6].as_str().expect("result consumer").is_empty());
            assert!(!row[7].as_str().expect("Phase 3 disposition").is_empty());
        }
    }
}

#[test]
fn typed_thread_actions_enter_through_the_tui_surface_action_facade() {
    let action_source = include_str!("surface_actions.rs");

    for method in [
        "run_turn",
        "resume_operation",
        "cancel_operation",
        "update_settings",
        "read_snapshot",
        "add_pinned_context",
        "expand_mentions",
        "discover_mention_catalog",
        "backtrack_last_user",
        "goal",
        "stop_task",
        "foreground_task",
        "resolve_background_approval",
        "launch_workflow",
    ] {
        assert!(
            action_source.contains(&format!("fn {method}")),
            "surface action facade is missing {method}"
        );
    }
    assert!(
        APP.contains("TuiSurfaceActions::new"),
        "app must construct the closed TUI action facade"
    );
    assert!(
        APP.contains("Ok(UserAction::ResumeOperation { operation_id })")
            && APP.contains("Ok(UserAction::CancelOperation { operation_id })"),
        "TUI recovery actions must route through the controller loop"
    );
    assert!(
        !SUBMITTED_TURN.contains("RuntimeSurfaceThreadHandle"),
        "submitted turns must not call the runtime thread facade directly"
    );
    assert!(
        !MENTION_SEARCH_MANAGER.contains("RuntimeSurfaceThreadHandle"),
        "mention discovery must use the TUI action facade"
    );
    assert!(
        !BACKGROUND_TASKS.contains("TaskRegistry") && !BACKGROUND_APPROVAL.contains("TaskRegistry"),
        "background controls must not retain the runtime task registry"
    );
    assert!(
        TYPES.contains("Remember {") && !SLASH_COMMAND_ACTIONS.contains("orca_runtime::memory::"),
        "memory scope and persistence must cross the TUI action facade"
    );
    assert!(
        !SLASH_COMMAND_ACTIONS.contains("history::list_sessions")
            && !SLASH_MENU_ACTIONS.contains("history::list_sessions")
            && !SESSION_PICKER_ACTIONS.contains("history::load_session"),
        "session history reads must cross the runtime surface history boundary"
    );
    assert!(
        !APP.contains("GoalRuntimeHandle::open_default"),
        "saved Goal actor ownership must remain behind the runtime surface host"
    );
    assert!(
        !action_source.contains(".goal()"),
        "the TUI facade must receive Goal values, not a callable Goal actor handle"
    );
    assert!(
        action_source.contains("crate::surface_client::launch_workflow")
            && !action_source.contains("self.thread\n            .launch_workflow"),
        "saved workflow launch must use the typed runtime surface client"
    );
    assert!(
        action_source.contains("crate::surface_client::stop_task")
            && !action_source.contains("self.thread.stop_task"),
        "workflow task stop must cancel its runtime-owned typed operation"
    );
    assert!(
        !APP.contains("HostedWorkflowRequest")
            && SURFACE_PROJECTION.contains("SurfaceEvent::Task")
            && SURFACE_PROJECTION.contains("SurfaceEvent::Workflow"),
        "TUI workflow task and lifecycle updates must come from typed surface batches"
    );
    assert!(
        !SLASH_COMMAND_ACTIONS.contains("folder_trust::")
            && !SETUP_ACTIONS.contains("orca_core::config::file"),
        "host-scoped trust and credentials must mutate through the TUI surface facade"
    );
}

#[test]
fn typed_history_resume_projects_only_the_durable_surface_snapshot() {
    let start = APP
        .find("fn emit_typed_history_snapshot(")
        .expect("typed history emitter");
    let end = APP[start..]
        .find("\nfn typed_history_startup_eligible(")
        .map(|offset| start + offset)
        .expect("typed history emitter boundary");
    let emitter = &APP[start..end];

    assert!(
        emitter.contains("history_messages_from_surface_snapshot(&snapshot)"),
        "resume must rebuild the transcript from the runtime-owned typed snapshot"
    );
    assert!(
        !emitter.contains("read_history"),
        "resume must not fall back to the legacy conversation history projection"
    );
    assert!(
        !include_str!("surface_actions.rs").contains("fn read_history"),
        "the TUI facade must not retain a second history truth beside SurfaceSnapshot.items"
    );
}

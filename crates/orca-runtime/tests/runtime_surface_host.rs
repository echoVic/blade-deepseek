use orca_core::approval_types::ActionKind;
use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::model::ModelSelection;
use orca_core::subagent_config::SubagentConfig;
use orca_core::task_types::{PendingToolCallSummary, TaskStatus};
use orca_runtime::runtime_host::RuntimeHost;
use orca_runtime::surface::{
    AttachResult, CompactionState, DisplayText, FreshAttachRequest, MutationReply, NonEmptyText,
    OperationTerminal, PinnedContextAction, PinnedContextRevision, PinnedContextSourceRevision,
    PinnedUserRevision, Sha256Digest, SurfaceAttachmentRole, SurfaceCapability,
    SurfaceCatalogEntryId, SurfaceEvent, SurfaceInteractionKind, SurfacePinnedContextEntry,
    SurfacePinnedContextKind, SurfaceRequestId, SurfaceSubscriptionItem,
    WaitOperationTerminalResult,
};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use tempfile::tempdir;

#[test]
fn closed_host_facade_starts_a_typed_thread_surface() {
    let cwd = tempdir().expect("temp cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let surface_host = host.surface_handle();
    let thread = surface_host
        .start_thread(test_config(cwd.path().to_path_buf()), "facade")
        .expect("typed thread");

    assert!(!thread.thread_id().is_empty());
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::<SurfaceInteractionKind>::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("unexpected attachment result"),
    };
    let thread_id = orca_runtime::surface::SurfaceThreadId::try_from_bytes(
        *uuid::Uuid::parse_str(thread.thread_id())
            .expect("thread id is UUID")
            .as_bytes(),
    )
    .expect("surface thread id");
    assert_eq!(attachment.baseline.snapshot.thread.thread_id, thread_id);

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn thread_facade_issues_a_distinct_acp_surface_authority() {
    let cwd = tempdir().expect("temp cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(test_config(cwd.path().to_path_buf()), "ACP facade")
        .expect("typed thread");
    let surface = thread.acp_surface().expect("ACP surface");
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Acp,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("unexpected ACP attachment result"),
    };
    assert_eq!(
        attachment.capabilities.grant.role,
        SurfaceAttachmentRole::Acp
    );
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn tui_surface_can_commit_and_publish_pinned_context() {
    let cwd = tempdir().expect("temp cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(test_config(cwd.path().to_path_buf()), "pinned context")
        .expect("typed thread");
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ManagePinnedContext,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("unexpected attachment result"),
    };
    let entry = SurfacePinnedContextEntry {
        id: SurfaceCatalogEntryId::try_new("user-note-1").unwrap(),
        kind: SurfacePinnedContextKind::User,
        label: NonEmptyText::try_new("remembered note").unwrap(),
        content: DisplayText::new("remember this"),
        content_digest: Sha256Digest::new([7; 32]),
        source_revision: PinnedContextSourceRevision::User(PinnedUserRevision::try_new(1).unwrap()),
    };
    let result = attachment.client.pinned_context_mutation(
        SurfaceRequestId::new(),
        PinnedContextAction::Add {
            expected_revision: PinnedContextRevision::try_new(1).unwrap(),
            entry: entry.clone(),
            memory_receipt: None,
        },
    );
    let output = match result.expect("pinned context command") {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("unexpected pinned context result"),
    };
    assert_eq!(
        output.snapshot.revision,
        PinnedContextRevision::try_new(2).unwrap()
    );
    assert_eq!(output.snapshot.entries, vec![entry]);
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn tui_surface_manual_compaction_is_durable_before_terminal() {
    let cwd = tempdir().expect("temp cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(
            test_config(cwd.path().to_path_buf()),
            "typed manual compaction",
        )
        .expect("typed thread");
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("unexpected attachment result"),
    };
    let expected_context_revision = attachment.baseline.snapshot.context.revision;
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .expect("surface subscription");
    let output = match attachment
        .client
        .manual_compact(SurfaceRequestId::new(), expected_context_revision)
        .expect("manual compact command")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("manual compaction must commit"),
    };
    let terminal = attachment
        .client
        .wait_operation_terminal(SurfaceRequestId::new(), output.operation_id.clone())
        .expect("terminal wait");
    assert!(matches!(
        terminal,
        WaitOperationTerminalResult::Terminal { value }
            if matches!(value.terminal, OperationTerminal::Succeeded { .. })
    ));

    let mut saw_running = false;
    let mut saw_completed = false;
    let mut saw_terminal = false;
    while let Some(item) = subscription.try_recv() {
        if let SurfaceSubscriptionItem::Batch { batch } = item {
            for envelope in batch.events.as_slice() {
                match &envelope.event {
                    SurfaceEvent::Context(context) => match &context.compaction {
                        CompactionState::Running { operation_id, .. }
                            if operation_id == &output.operation_id =>
                        {
                            saw_running = true;
                            assert!(!saw_completed);
                            assert!(!saw_terminal);
                        }
                        CompactionState::Completed { operation_id, .. }
                            if operation_id == &output.operation_id =>
                        {
                            saw_completed = true;
                            assert!(saw_running);
                            assert!(!saw_terminal);
                        }
                        _ => {}
                    },
                    SurfaceEvent::Operation(orca_runtime::surface::OperationPatch::Terminal {
                        record,
                    }) if record.operation_id == output.operation_id => {
                        saw_terminal = true;
                        assert!(saw_completed);
                    }
                    _ => {}
                }
            }
        }
    }
    assert!(saw_running);
    assert!(saw_completed);
    assert!(saw_terminal);

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn typed_thread_expands_mentions_with_runtime_owned_registry() {
    let cwd = tempdir().expect("temp cwd");
    fs::write(cwd.path().join("context.txt"), "runtime-owned context").expect("context file");
    let root = cwd.path().canonicalize().expect("canonical cwd");
    let input = "read @context.txt";
    let bindings = orca_runtime::mentions::MentionBindings::from_bindings(
        input,
        vec![orca_runtime::mentions::MentionBinding {
            start: 5,
            end: input.len(),
            visible: "@context.txt".to_string(),
            target: orca_runtime::mentions::MentionTarget::File {
                root: root.clone(),
                path: "context.txt".to_string(),
                kind: orca_runtime::mentions::MentionFileKind::File,
            },
        }],
    );
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(test_config(root.clone()), "mention expansion")
        .expect("typed thread");

    let expanded = thread
        .expand_mentions(input, &bindings, &root, std::slice::from_ref(&root))
        .expect("runtime mention expansion");

    assert!(expanded.contains("runtime-owned context"));
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn typed_thread_discovers_mention_catalog_with_runtime_owned_registry() {
    let cwd = tempdir().expect("temp cwd");
    let manifest_dir = cwd.path().join(".orca/plugins/github/.codex-plugin");
    fs::create_dir_all(&manifest_dir).expect("plugin directory");
    fs::write(
        manifest_dir.join("plugin.json"),
        r#"{"name":"github","description":"GitHub workflows","interface":{"displayName":"GitHub"}}"#,
    )
    .expect("plugin manifest");
    let root = cwd.path().canonicalize().expect("canonical cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(test_config(root.clone()), "mention catalog")
        .expect("typed thread");

    let catalog = thread.discover_mention_catalog(std::slice::from_ref(&root));

    assert!(
        catalog
            .candidates()
            .iter()
            .any(|candidate| candidate.display == "GitHub")
    );
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn closed_thread_facade_owns_task_control_and_background_approval() {
    let cwd = tempdir().expect("temp cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let runtime_thread = host
        .handle()
        .start_thread(test_config(cwd.path().to_path_buf()), "task facade")
        .expect("runtime thread");
    let registry = runtime_thread.task_registry();
    let foreground = registry.create_main_session("background turn".to_string());
    registry.mark_running(&foreground.id).expect("running task");
    registry
        .mark_backgrounded(&foreground.id)
        .expect("backgrounded task");
    let approval = registry.create_main_session("approval turn".to_string());
    registry
        .approval_required_for_pending_tool(
            &approval.id,
            "approval required".to_string(),
            Some(PendingToolCallSummary {
                id: "approval-request".to_string(),
                name: "shell".to_string(),
                action: ActionKind::Shell,
                target: None,
                arguments: "{}".to_string(),
            }),
        )
        .expect("approval task");
    let surface = runtime_thread.typed_surface();

    let foregrounded = surface
        .foreground_task(&foreground.id)
        .expect("foreground through facade");
    assert!(
        foregrounded
            .iter()
            .any(|task| task.id == foreground.id && !task.is_backgrounded)
    );

    let (task_id, denied) = surface
        .resolve_background_approval("approval-request", false)
        .expect("deny approval through facade");
    assert_eq!(task_id, approval.id);
    assert!(
        denied
            .iter()
            .any(|task| task.id == approval.id && task.status == TaskStatus::Stopped)
    );

    let stopped = surface
        .stop_task(&foreground.id)
        .expect("stop through facade");
    assert!(
        stopped
            .iter()
            .any(|task| task.id == foreground.id && task.status == TaskStatus::Stopping)
    );

    host.shutdown().expect("shutdown runtime host");
}

fn test_config(cwd: std::path::PathBuf) -> RunConfig {
    RunConfig {
        app_version: "test".to_string(),
        prompt: String::new(),
        cwd: Some(cwd),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::Suggest,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::parse(None).expect("default model"),
        model_runtime: ModelRuntimeConfig::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        api_key: None,
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode: HistoryMode::Record,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: HashMap::new(),
        runtime_workspace_roots: None,
        permission_rules: Default::default(),
        additional_working_directories: Vec::new(),
        max_budget_usd: None,
        subagents: SubagentConfig::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: ThemeName::default(),
        vim_mode: false,
        update_check: false,
        desktop_notifications: false,
        auto_memory: false,
    }
}

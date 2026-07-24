use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::model::ModelSelection;
use orca_core::subagent_config::SubagentConfig;
use orca_runtime::runtime_host::RuntimeHost;
use orca_runtime::surface::{
    AttachResult, FreshAttachRequest, SurfaceAttachmentRole, SurfaceCapability,
    SurfaceInteractionKind, SurfaceRequestId,
};
use std::collections::{BTreeSet, HashMap};
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

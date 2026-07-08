pub mod agent_child;
pub mod agent_common;
pub mod agent_loop;
pub mod approval_resolution;
pub mod background_turn;
mod child_agent_entrypoints;
mod child_agent_loop_runner;
mod child_agent_loop_setup;
mod child_agent_provider_turn;
mod child_agent_response_folding;
#[cfg(test)]
mod child_agent_tests;
mod child_agent_types;
pub mod compaction;
pub mod controller;
pub mod cost;
pub mod extension;
pub mod goals;
pub mod history;
pub mod hooks;
pub mod instructions;
pub mod lifecycle;
pub mod memory;
pub mod mentions;
pub mod network_proxy;
pub mod notify;
pub mod protocol;
pub mod provider_turn;
pub(crate) mod runtime_approval;
pub(crate) mod runtime_bash;
pub mod runtime_capability;
mod runtime_conversation_bootstrap;
pub mod runtime_directive;
pub(crate) mod runtime_event_projector;
mod runtime_lifecycle;
mod runtime_model_route;
mod runtime_normal_tool;
pub mod runtime_pending_interaction;
pub(crate) mod runtime_permission;
pub(crate) mod runtime_readonly_tool_turn;
pub(crate) mod runtime_special;
pub mod runtime_state;
mod runtime_steer;
mod runtime_tool_actor;
mod runtime_turn_iteration;
mod runtime_turn_kernel;
mod runtime_turn_loop;
mod runtime_turn_opening;
mod runtime_turn_setup;
mod runtime_turn_start;
pub(crate) mod runtime_user_input;
pub mod sandbox_denial;
pub mod schema_validation;
pub mod server;
pub mod server_runtime;
pub mod session;
pub mod shell_session;
mod step_context;
pub mod subagent;
pub mod subagent_async_worker;
pub mod subagent_execution;
pub mod tasks;
pub mod thread;
pub mod thread_store;
pub mod tool_execution;
pub mod tool_invocation;
pub(crate) mod tool_item_projection;
mod tool_router;
pub mod tool_turn;
pub mod update_check;
pub mod workflow;
pub mod workflow_execution;
pub mod worktree;

#[cfg(test)]
mod tests {
    use crate::extension::{
        ExtensionData, ExtensionRegistryBuilder, ToolCallOutcome, ToolFinishInput,
        ToolLifecycleContributor,
    };
    use crate::goals::GoalToolProgressState;
    use crate::lifecycle::{
        RuntimePermissionRequest, RuntimePermissionRequestHandler, RuntimePermissionResponse,
        TurnPermissionOverlay,
    };
    use crate::protocol::{
        PermissionGrantScope, PermissionResponseDecision, RequestFileSystemPermissions,
        RequestNetworkPermissions, RequestPermissionProfile,
    };
    use crate::runtime_capability::{RuntimeCapabilityPatch, RuntimeCapabilitySnapshot};
    use crate::runtime_directive::{RuntimeDirective, RuntimeDirectiveState};
    use crate::runtime_state::{RuntimeToolFinish, RuntimeTurnReducer};
    use crate::thread_store::{SessionStore, ThreadStore};
    use orca_core::config::PermissionProfileNetworkAccess;
    use std::collections::HashMap;
    use std::io;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[test]
    fn thread_store_module_exports_session_store_boundary() {
        fn assert_thread_store<T: ThreadStore>(store: &T) {
            let _ = store;
        }

        assert_thread_store(&SessionStore::new());
    }

    #[test]
    fn extension_data_stores_typed_values_per_scope() {
        #[derive(Debug, Eq, PartialEq)]
        struct Marker(&'static str);

        let data = ExtensionData::new("thread-a");
        assert_eq!(data.level_id(), "thread-a");
        assert!(data.get::<Marker>().is_none());

        assert!(data.insert(Marker("seed")).is_none());
        assert_eq!(data.get::<Marker>().as_deref(), Some(&Marker("seed")));

        let existing = data.get_or_init(|| Marker("ignored"));
        assert_eq!(existing.as_ref(), &Marker("seed"));
    }

    #[test]
    fn extension_registry_runs_tool_lifecycle_contributors_in_order() {
        #[derive(Default)]
        struct RecordingContributor {
            label: &'static str,
            calls: Arc<Mutex<Vec<String>>>,
        }

        impl ToolLifecycleContributor for RecordingContributor {
            fn on_tool_finish(&self, input: ToolFinishInput<'_>) {
                self.calls.lock().unwrap().push(format!(
                    "{}:{}:{}:{}",
                    self.label,
                    input.thread_store.level_id(),
                    input.turn_store.level_id(),
                    input.tool_name
                ));
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut builder = ExtensionRegistryBuilder::new();
        builder.tool_lifecycle_contributor(Arc::new(RecordingContributor {
            label: "first",
            calls: Arc::clone(&calls),
        }));
        builder.tool_lifecycle_contributor(Arc::new(RecordingContributor {
            label: "second",
            calls: Arc::clone(&calls),
        }));

        let registry = builder.build();
        let thread_store = ExtensionData::new("thread-a");
        let turn_store = ExtensionData::new("turn-1");

        registry.on_tool_finish(ToolFinishInput {
            thread_store: &thread_store,
            turn_store: &turn_store,
            tool_name: "bash",
            call_id: "call-1",
            outcome: ToolCallOutcome::Completed,
        });

        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["first:thread-a:turn-1:bash", "second:thread-a:turn-1:bash"]
        );
    }

    #[test]
    fn runtime_turn_reducer_records_tool_finish_goal_progress() {
        let thread_store = ExtensionData::new("thread-a");
        let turn_store = ExtensionData::new("turn-1");
        let reducer = RuntimeTurnReducer::new(&thread_store, &turn_store);

        reducer.record_tool_finish(RuntimeToolFinish {
            tool_name: "bash",
            call_id: "call-1",
            outcome: ToolCallOutcome::Completed,
        });

        let progress = thread_store
            .get::<GoalToolProgressState>()
            .expect("tool finish should update goal progress through reducer");
        assert_eq!(progress.completed_tool_attempts(), 1);
        assert_eq!(progress.last_turn_id().as_deref(), Some("turn-1"));
        assert_eq!(progress.last_call_id().as_deref(), Some("call-1"));
    }

    #[test]
    fn runtime_turn_reducer_applies_runtime_directives_in_order() {
        let thread_store = ExtensionData::new("thread-a");
        let turn_store = ExtensionData::new("turn-1");
        let reducer = RuntimeTurnReducer::new(&thread_store, &turn_store);
        let mut directives = RuntimeDirectiveState::default();

        reducer.apply_directive(
            &mut directives,
            RuntimeDirective::SwitchModel {
                model: orca_core::model::FLASH_MODEL.to_string(),
                reason: "skill requested cheaper execution".to_string(),
            },
        );
        reducer.apply_directive(
            &mut directives,
            RuntimeDirective::ReplaceAllowedTools {
                tool_names: vec!["read_file".to_string(), "grep".to_string()],
                reason: "skill narrowed tool surface".to_string(),
            },
        );
        reducer.apply_directive(
            &mut directives,
            RuntimeDirective::InjectSystemMessage {
                message: "Prefer focused repository evidence.".to_string(),
                reason: "skill added runtime instruction".to_string(),
            },
        );

        assert_eq!(
            directives.model_override(),
            Some(orca_core::model::FLASH_MODEL)
        );
        assert_eq!(
            directives.allowed_tools(),
            Some(&["read_file".to_string(), "grep".to_string()][..])
        );
        assert_eq!(
            directives.pending_system_messages(),
            &["Prefer focused repository evidence.".to_string()]
        );
        assert_eq!(
            directives.transition_reasons(),
            &[
                "switch_model: skill requested cheaper execution".to_string(),
                "replace_allowed_tools: skill narrowed tool surface".to_string(),
                "inject_system_message: skill added runtime instruction".to_string(),
            ]
        );
    }

    #[test]
    fn runtime_capability_patch_updates_named_snapshot() {
        let mut snapshot = RuntimeCapabilitySnapshot::default();

        snapshot.apply_patch(RuntimeCapabilityPatch::SwitchModel {
            model: orca_core::model::FLASH_MODEL.to_string(),
            reason: "skill requested cheaper execution".to_string(),
        });
        snapshot.apply_patch(RuntimeCapabilityPatch::ReplaceAllowedTools {
            tool_names: vec!["read_file".to_string(), "grep".to_string()],
            reason: "skill narrowed tool surface".to_string(),
        });
        snapshot.apply_patch(RuntimeCapabilityPatch::InjectSystemMessage {
            message: "Prefer focused repository evidence.".to_string(),
            reason: "skill added runtime instruction".to_string(),
        });

        assert_eq!(
            snapshot.model_override(),
            Some(orca_core::model::FLASH_MODEL)
        );
        assert_eq!(
            snapshot.allowed_tools(),
            Some(&["read_file".to_string(), "grep".to_string()][..])
        );
        assert_eq!(
            snapshot.pending_system_messages(),
            &["Prefer focused repository evidence.".to_string()]
        );
        assert_eq!(
            snapshot.transition_reasons(),
            &[
                "switch_model: skill requested cheaper execution".to_string(),
                "replace_allowed_tools: skill narrowed tool surface".to_string(),
                "inject_system_message: skill added runtime instruction".to_string(),
            ]
        );
    }

    #[test]
    fn runtime_turn_reducer_applies_capability_patches_to_snapshot() {
        let thread_store = ExtensionData::new("thread-a");
        let turn_store = ExtensionData::new("turn-1");
        let reducer = RuntimeTurnReducer::new(&thread_store, &turn_store);
        let mut snapshot = RuntimeCapabilitySnapshot::default();

        reducer.apply_capability_patch(
            &mut snapshot,
            RuntimeCapabilityPatch::SwitchModel {
                model: orca_core::model::FLASH_MODEL.to_string(),
                reason: "runtime chose flash".to_string(),
            },
        );

        assert_eq!(
            snapshot.model_override(),
            Some(orca_core::model::FLASH_MODEL)
        );
        assert_eq!(
            snapshot.transition_reasons(),
            &["switch_model: runtime chose flash".to_string()]
        );
    }

    #[test]
    fn runtime_directive_state_exposes_capability_snapshot_contract() {
        let mut directives = RuntimeDirectiveState::default();

        directives.apply_patch(RuntimeCapabilityPatch::SwitchModel {
            model: orca_core::model::FLASH_MODEL.to_string(),
            reason: "skill requested cheaper execution".to_string(),
        });
        directives.apply_patch(RuntimeCapabilityPatch::InjectSystemMessage {
            message: "Prefer focused repository evidence.".to_string(),
            reason: "skill added runtime instruction".to_string(),
        });

        let capabilities = directives.capabilities();
        assert_eq!(
            capabilities.model_override(),
            Some(orca_core::model::FLASH_MODEL)
        );
        assert_eq!(
            capabilities.pending_system_messages(),
            &["Prefer focused repository evidence.".to_string()]
        );
        assert_eq!(
            capabilities.transition_reasons(),
            &[
                "switch_model: skill requested cheaper execution".to_string(),
                "inject_system_message: skill added runtime instruction".to_string(),
            ]
        );
    }

    #[test]
    fn runtime_turn_reducer_requests_and_merges_permission_overlay() {
        struct AllowWithStrictReview;

        impl RuntimePermissionRequestHandler for AllowWithStrictReview {
            fn request_permissions(
                &self,
                request: &RuntimePermissionRequest,
            ) -> io::Result<RuntimePermissionResponse> {
                Ok(RuntimePermissionResponse {
                    decision: PermissionResponseDecision::Allow,
                    scope: PermissionGrantScope::Turn,
                    permissions: request.permissions.clone(),
                    strict_auto_review: true,
                })
            }
        }

        let thread_store = ExtensionData::new("thread-a");
        let turn_store = ExtensionData::new("turn-1");
        let reducer = RuntimeTurnReducer::new(&thread_store, &turn_store);
        let mut overlay = TurnPermissionOverlay::default();
        let write_root = PathBuf::from("/tmp/orca-write-root");
        let mut domains = HashMap::new();
        domains.insert(
            "api.deepseek.com".to_string(),
            PermissionProfileNetworkAccess::Allow,
        );

        let response = reducer
            .request_permission(
                &mut overlay,
                &AllowWithStrictReview,
                RuntimePermissionRequest {
                    id: "permission-1".to_string(),
                    reason: Some("bash needs a write root and network access".to_string()),
                    permissions: RequestPermissionProfile {
                        file_system: Some(RequestFileSystemPermissions {
                            read: None,
                            write: Some(vec![write_root.clone()]),
                            entries: None,
                        }),
                        network: Some(RequestNetworkPermissions {
                            enabled: None,
                            domains,
                        }),
                        shell: None,
                    },
                },
            )
            .expect("permission reducer should delegate to handler");

        assert_eq!(response.decision, PermissionResponseDecision::Allow);
        assert_eq!(overlay.additional_working_directories(), &[write_root]);
        assert_eq!(
            overlay.network_domain_permissions().get("api.deepseek.com"),
            Some(&PermissionProfileNetworkAccess::Allow)
        );
        assert!(overlay.strict_auto_review());
    }

    #[test]
    fn runtime_permission_overlay_mutations_route_through_runtime_reducer() {
        let runtime_state_source = include_str!("runtime_state.rs");
        let extension_source = include_str!("extension.rs");
        let runtime_special_source = include_str!("runtime_special.rs");
        let runtime_bash_source = include_str!("runtime_bash.rs");
        let runtime_normal_tool_source = include_str!("runtime_normal_tool.rs");
        let tool_router_source = include_str!("tool_router.rs");

        assert!(
            extension_source.contains("struct RuntimeExtensionStores"),
            "extension module must own the grouped thread/turn extension store refs"
        );
        assert!(
            runtime_state_source.contains("struct PermissionRuntimeState"),
            "runtime_state must own the permission reducer branch"
        );
        assert!(
            !runtime_state_source.contains("pub fn permission()"),
            "permission reduction must be owned by RuntimeTurnReducer instances"
        );
        assert!(
            runtime_state_source.contains("pub fn request_permission("),
            "RuntimeTurnReducer must expose permission request reduction"
        );
        assert!(
            runtime_state_source.contains("pub fn merge_permission_overlay("),
            "RuntimeTurnReducer must expose permission overlay merge reduction"
        );

        for (module_name, source) in [
            ("runtime_special", runtime_special_source),
            ("runtime_bash", runtime_bash_source),
            ("tool_router", tool_router_source),
        ] {
            assert!(
                source.contains("RuntimeTurnReducer::from_extension_stores("),
                "{module_name} must create a RuntimeTurnReducer from grouped extension stores"
            );
            assert!(
                !source.contains("RuntimeTurnReducer::permission()"),
                "{module_name} must not bypass RuntimeTurnReducer instance state for permission overlay mutation"
            );
            assert!(
                !source.contains(".request_and_merge("),
                "{module_name} must not request and merge permission overlay directly"
            );
        }

        for (module_name, source) in [
            ("runtime_bash", runtime_bash_source),
            ("runtime_normal_tool", runtime_normal_tool_source),
            ("tool_router", tool_router_source),
        ] {
            assert!(
                source.contains("extension_stores"),
                "{module_name} must pass grouped extension stores through permission-sensitive tool contexts"
            );
            assert!(
                !source.contains("pub(crate) thread_extensions:"),
                "{module_name} must not expose thread extension refs as a parallel context field"
            );
            assert!(
                !source.contains("pub(crate) turn_extensions:"),
                "{module_name} must not expose turn extension refs as a parallel context field"
            );
        }

        assert!(
            !tool_router_source.contains("permission_overlay.merge("),
            "tool_router must not merge permission overlay directly"
        );
    }

    #[test]
    fn tool_execution_context_groups_extension_store_refs() {
        let tool_execution_source = include_str!("tool_execution.rs");

        assert!(
            tool_execution_source.contains("extension_stores: Option<RuntimeExtensionStores"),
            "ToolExecutionContext must carry grouped runtime extension stores"
        );
        assert!(
            tool_execution_source.contains("with_extensions(\n        mut self,\n        extension_registry: &'a ExtensionRegistry,\n        extension_stores: RuntimeExtensionStores<'a>,"),
            "ToolExecutionContext::with_extensions must accept grouped stores"
        );
        assert!(
            !tool_execution_source.contains("thread_extensions: Option<&'a ExtensionData>"),
            "ToolExecutionContext must not expose thread extension refs as a parallel field"
        );
        assert!(
            !tool_execution_source.contains("turn_extensions: Option<&'a ExtensionData>"),
            "ToolExecutionContext must not expose turn extension refs as a parallel field"
        );
        assert!(
            !tool_execution_source.contains("match (thread_extensions, turn_extensions)"),
            "ToolExecutionActor must not reconstruct grouped stores from parallel refs"
        );
        assert!(
            tool_execution_source.contains("extension_stores.thread_store()"),
            "tool lifecycle notifications must read the thread store from grouped stores"
        );
        assert!(
            tool_execution_source.contains("extension_stores.turn_store()"),
            "tool lifecycle notifications must read the turn store from grouped stores"
        );
    }

    #[test]
    fn step_and_normal_tool_turn_contexts_group_runtime_extensions() {
        let step_context_source = include_str!("step_context.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        assert!(
            step_context_source.contains("extensions: Option<RuntimeExtensionContext"),
            "RuntimeStepContext must carry runtime extensions as one grouped context"
        );
        assert!(
            !step_context_source.contains("RuntimeExtensionStores"),
            "RuntimeStepContext must not own extension-store binding; RuntimeTurnKernel binds step extensions"
        );
        assert!(
            !step_context_source.contains("thread_extensions: Option<&'a ExtensionData>"),
            "RuntimeStepContext must not expose thread extension refs as a parallel field"
        );
        assert!(
            !step_context_source.contains("turn_extensions: Option<&'a ExtensionData>"),
            "RuntimeStepContext must not expose turn extension refs as a parallel field"
        );

        assert!(
            tool_turn_source.contains("extensions: Option<RuntimeExtensionContext"),
            "RuntimeNormalToolTurnContext must carry runtime extensions as one grouped context"
        );
        assert!(
            !tool_turn_source.contains("thread_extensions: Option<&'a ExtensionData>"),
            "RuntimeNormalToolTurnContext must not expose thread extension refs as a parallel field"
        );
        assert!(
            !tool_turn_source.contains("turn_extensions: Option<&'a ExtensionData>"),
            "RuntimeNormalToolTurnContext must not expose turn extension refs as a parallel field"
        );
        assert!(
            !tool_turn_source.contains("(extension_registry, thread_extensions, turn_extensions)"),
            "normal tool turns must not reconstruct grouped stores from three parallel refs"
        );
    }

    #[test]
    fn runtime_step_context_exposes_request_snapshot_contract() {
        let step_context_source = include_str!("step_context.rs");
        let provider_turn_source = include_str!("provider_turn.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        assert!(
            step_context_source.contains("pub(crate) struct RuntimeStepSnapshot"),
            "RuntimeStepSnapshot must be the named request-scoped runtime snapshot"
        );
        assert!(
            step_context_source.contains("snapshot: RuntimeStepSnapshot<'a>"),
            "RuntimeStepContext must carry request-scoped inputs through one snapshot field"
        );
        let runtime_step_context_struct = step_context_source
            .split("pub(crate) struct RuntimeStepContext<'a> {")
            .nth(1)
            .expect("RuntimeStepContext struct body")
            .split("}")
            .next()
            .expect("RuntimeStepContext struct end");
        for field_name in [
            "config",
            "cwd",
            "tool_policy",
            "subagent_depth",
            "emit_deltas",
            "policy",
            "instructions",
            "memory",
            "mcp_registry",
            "hooks",
            "cancel",
            "task_registry",
            "workflow_ipc",
            "permission_handler",
        ] {
            assert!(
                !runtime_step_context_struct.contains(&format!("pub(crate) {field_name}:")),
                "RuntimeStepContext must not expose request-scoped field {field_name} outside RuntimeStepSnapshot"
            );
        }
        assert!(
            provider_turn_source.contains("step_context.snapshot()"),
            "provider_turn must read request-scoped inputs through RuntimeStepSnapshot"
        );
        assert!(
            tool_turn_source.contains("step_context.into_parts()"),
            "tool_turn dispatch must split RuntimeStepContext into its snapshot plus extension binding"
        );
    }

    #[test]
    fn runtime_step_snapshot_groups_runtime_capabilities_contract() {
        let step_context_source = include_str!("step_context.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        assert!(
            step_context_source.contains("pub(crate) struct RuntimeStepCapabilitySnapshot"),
            "RuntimeStepCapabilitySnapshot must name the request-scoped runtime capability bundle"
        );
        assert!(
            step_context_source.contains("capabilities: RuntimeStepCapabilitySnapshot<'a>"),
            "RuntimeStepSnapshot must carry runtime capability refs through one named field"
        );
        assert!(
            step_context_source.contains("turn_context: RuntimeTurnContext<'a>"),
            "RuntimeStepSnapshot must carry immutable turn inputs through RuntimeTurnContext"
        );
        assert!(
            step_context_source.contains("pub(crate) fn capabilities(&self)"),
            "RuntimeStepSnapshot must expose runtime capability refs through an accessor"
        );

        let runtime_step_snapshot_struct = step_context_source
            .split("pub(crate) struct RuntimeStepSnapshot<'a> {")
            .nth(1)
            .expect("RuntimeStepSnapshot struct body")
            .split("}")
            .next()
            .expect("RuntimeStepSnapshot struct end");
        for field_name in [
            "instructions",
            "memory",
            "mcp_registry",
            "hooks",
            "cancel",
            "task_registry",
            "workflow_ipc",
            "permission_handler",
            "user_input_handler",
            "cwd",
            "subagent_depth",
            "emit_deltas",
        ] {
            assert!(
                !runtime_step_snapshot_struct.contains(&format!("pub(crate) {field_name}:")),
                "RuntimeStepSnapshot must not expose grouped field {field_name} outside its named context"
            );
        }

        assert!(
            tool_turn_source.contains("let capabilities = step_snapshot.capabilities();"),
            "tool_turn dispatch must route runtime capability refs through RuntimeStepCapabilitySnapshot"
        );
        assert!(
            !tool_turn_source.contains("let instructions = step_snapshot.instructions;"),
            "tool_turn dispatch must not peel capability refs directly off RuntimeStepSnapshot"
        );
    }

    #[test]
    fn runtime_provider_cycle_reuses_step_capability_snapshot_contract() {
        let provider_turn_source = include_str!("provider_turn.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");

        assert!(
            provider_turn_source.contains("RuntimeStepCapabilitySnapshot"),
            "provider cycle must reuse the step capability snapshot type instead of repeating capability refs"
        );
        assert!(
            provider_turn_source.contains("capabilities: RuntimeStepCapabilitySnapshot<'a>"),
            "RuntimeProviderCycleInput must carry request capability refs through one named field"
        );
        assert!(
            provider_turn_source.contains("turn_context: RuntimeTurnContext<'a>"),
            "RuntimeProviderCycleInput must carry immutable turn refs through RuntimeTurnContext"
        );

        let provider_cycle_input_struct = provider_turn_source
            .split("pub(crate) struct RuntimeProviderCycleInput")
            .nth(1)
            .expect("RuntimeProviderCycleInput struct body")
            .split("pub(crate) struct RuntimeProviderTurnInput")
            .next()
            .expect("RuntimeProviderCycleInput struct end");
        for field_name in [
            "instructions",
            "memory",
            "mcp_registry",
            "hooks",
            "cancel",
            "task_registry",
            "workflow_ipc",
            "permission_handler",
            "user_input_handler",
            "cwd",
            "emit_deltas",
            "steer_handle",
        ] {
            assert!(
                !provider_cycle_input_struct.contains(&format!("pub(crate) {field_name}:")),
                "RuntimeProviderCycleInput must not expose grouped field {field_name} outside its named context"
            );
        }

        assert!(
            provider_turn_source.contains("RuntimeStepContext::from_snapshot"),
            "provider cycle should pass the grouped provider-cycle snapshot into RuntimeStepContext without expanding capability refs"
        );
        assert!(
            !provider_turn_source.contains("input.instructions"),
            "provider cycle must not expand capability refs when creating RuntimeStepContext"
        );
        assert!(
            runtime_turn_iteration_source
                .contains("capabilities: RuntimeStepCapabilitySnapshot::new("),
            "runtime_turn_iteration must assemble provider-cycle capability refs through RuntimeStepCapabilitySnapshot"
        );
    }

    #[test]
    fn runtime_turn_interaction_state_groups_turn_scoped_interaction_handlers() {
        let lifecycle_source = include_str!("lifecycle.rs");
        let agent_loop_source = include_str!("agent_loop.rs");
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");

        assert!(
            lifecycle_source.contains("pub(crate) struct RuntimeTurnInteractionState"),
            "RuntimeTurnInteractionState must be the named home for turn-scoped interaction handlers"
        );
        assert!(
            lifecycle_source.contains("turn_interactions: RuntimeTurnInteractionState<'a>"),
            "RuntimeTurnDeps must carry turn-scoped interaction handlers through one grouped field"
        );
        assert!(
            lifecycle_source
                .contains("user_input_handler: Option<&'a dyn RuntimeUserInputHandler>"),
            "RuntimeTurnInteractionState must carry request_user_input handlers with other turn-scoped interactions"
        );
        let agent_loop_context_struct = lifecycle_source
            .split("pub(crate) struct AgentLoopContext<'a> {")
            .nth(1)
            .expect("AgentLoopContext struct body")
            .split("}")
            .next()
            .expect("AgentLoopContext struct end");
        assert!(
            !agent_loop_context_struct.contains("permission_handler:"),
            "AgentLoopContext must not expose permission_handler as a parallel top-level field"
        );
        assert!(
            !agent_loop_context_struct
                .contains("turn_interactions: RuntimeTurnInteractionState<'a>"),
            "AgentLoopContext must not carry turn interaction handlers outside RuntimeTurnDeps"
        );
        let turn_deps_struct = lifecycle_source
            .split("pub(crate) struct RuntimeTurnDeps<'a> {")
            .nth(1)
            .expect("RuntimeTurnDeps struct body")
            .split("}")
            .next()
            .expect("RuntimeTurnDeps struct end");
        assert!(
            turn_deps_struct.contains("turn_interactions: RuntimeTurnInteractionState<'a>"),
            "RuntimeTurnDeps must own turn interaction handlers with other injected turn services"
        );
        assert!(
            agent_loop_source.contains("turn_deps"),
            "agent_loop must route turn-scoped interaction handlers through RuntimeTurnDeps"
        );
        assert!(
            runtime_turn_loop_source.contains("deps: RuntimeTurnDeps"),
            "runtime_turn_loop must pass RuntimeTurnDeps through turn-loop inputs"
        );
        assert!(
            runtime_turn_iteration_source.contains("deps.turn_interactions"),
            "runtime_turn_iteration must read interaction handlers through RuntimeTurnDeps"
        );
    }

    #[test]
    fn sampling_request_state_owns_tool_permission_overlay() {
        let step_context_source = include_str!("step_context.rs");
        let tool_turn_source = include_str!("tool_turn.rs");
        let tool_turn_runtime_source = tool_turn_source
            .split("mod tests")
            .next()
            .expect("tool turn runtime source");

        assert!(
            step_context_source.contains("struct RuntimeSamplingRequestState"),
            "RuntimeStepContext must have a sampling-request state object"
        );
        assert!(
            step_context_source.contains("permission_overlay: TurnPermissionOverlay"),
            "RuntimeSamplingRequestState must own the turn permission overlay"
        );
        assert!(
            step_context_source.contains("fn permission_overlay_mut(&mut self)"),
            "RuntimeSamplingRequestState must expose mutable permission overlay access"
        );
        assert!(
            tool_turn_source.contains("sampling_state: &'a mut RuntimeSamplingRequestState"),
            "RuntimeToolTurnsContext must receive lifecycle-owned sampling state"
        );
        assert!(
            tool_turn_source
                .contains(".with_permission_overlay(sampling_state.permission_overlay_mut())"),
            "normal tool turns must use the sampling state's permission overlay"
        );
        assert!(
            !tool_turn_runtime_source.contains("TurnPermissionOverlay::default()"),
            "tool_turn must not allocate a local permission overlay for production execution"
        );
    }

    #[test]
    fn sampling_request_state_owns_tool_dispatch_cursor() {
        let step_context_source = include_str!("step_context.rs");
        let tool_turn_source = include_str!("tool_turn.rs");
        let tool_turn_runtime_source = tool_turn_source
            .split("mod tests")
            .next()
            .expect("tool turn runtime source");

        for marker in [
            "tool_cursor_index: usize",
            "fn current_tool_request<'a>(",
            "fn tool_cursor_position(&self)",
            "fn advance_tool_cursor_one(&mut self, tool_request_count: usize)",
            "fn advance_tool_cursor_to(",
            "next_index: usize",
            "tool_request_count: usize",
        ] {
            assert!(
                step_context_source.contains(marker),
                "RuntimeSamplingRequestState must own tool dispatch cursor detail {marker}"
            );
        }
        assert!(
            tool_turn_runtime_source.contains(".current_tool_request(tool_requests)"),
            "tool_turn must read the current tool request through sampling state"
        );
        assert!(
            tool_turn_runtime_source.contains(".tool_dispatch_window(tool_requests"),
            "tool_turn must read dispatch windows through sampling state"
        );
        assert!(
            tool_turn_runtime_source.contains(".advance_tool_cursor_to_window_end("),
            "tool_turn must advance batch cursor windows through sampling state"
        );
        assert!(
            tool_turn_runtime_source.contains(".advance_tool_cursor_one("),
            "tool_turn must advance single-tool cursor position through sampling state"
        );
        assert!(
            !tool_turn_runtime_source.contains("ToolRequestCursor"),
            "tool_turn production code must not own a separate tool request cursor"
        );
    }

    #[test]
    fn sampling_request_state_owns_tool_dispatch_windows() {
        let step_context_source = include_str!("step_context.rs");
        let tool_turn_source = include_str!("tool_turn.rs");
        let tool_turn_runtime_source = tool_turn_source
            .split("mod tests")
            .next()
            .expect("tool turn runtime source");

        for marker in [
            "struct RuntimeToolDispatchWindow<'a>",
            "fn tool_requests(&self) -> &'a [ToolRequest]",
            "fn end_index(&self) -> usize",
            "fn tool_dispatch_window<'a, F>(",
            "collect_end: F",
            "FnOnce(&[ToolRequest], usize) -> usize",
            "fn advance_tool_cursor_to_window_end(",
            "window: &RuntimeToolDispatchWindow<'_>",
        ] {
            assert!(
                step_context_source.contains(marker),
                "RuntimeSamplingRequestState must own dispatch-window detail {marker}"
            );
        }
        assert!(
            tool_turn_runtime_source.contains(".tool_dispatch_window(tool_requests"),
            "tool_turn must request batch windows through sampling state"
        );
        assert!(
            tool_turn_runtime_source.contains(".advance_tool_cursor_to_window_end("),
            "tool_turn must advance batch windows through sampling state"
        );
        assert!(
            !tool_turn_runtime_source.contains(".tool_cursor_position()"),
            "tool_turn production code must not read raw cursor positions"
        );
        assert!(
            !tool_turn_runtime_source.contains("&tool_requests[cursor_position..batch_end]"),
            "tool_turn production code must not slice batch windows directly"
        );
    }

    #[test]
    fn sampling_request_state_owns_normal_tool_result_recording() {
        let step_context_source = include_str!("step_context.rs");
        let tool_turn_source = include_str!("tool_turn.rs");
        let tool_turn_runtime_source = tool_turn_source
            .split("mod tests")
            .next()
            .expect("tool turn runtime source");

        for marker in [
            "fn record_normal_tool_result(",
            "record_plan_state_for_agent(",
            "record_tool_result_for_agent(",
            "status == RunStatus::ApprovalRequired",
            "tool_request.name == ToolName::Subagent",
        ] {
            assert!(
                step_context_source.contains(marker),
                "RuntimeSamplingRequestState must own normal tool result recording detail {marker}"
            );
        }
        assert!(
            tool_turn_runtime_source.contains(".record_normal_tool_result("),
            "tool_turn must record normal tool results through sampling request state"
        );
        assert!(
            !tool_turn_runtime_source.contains("pub(crate) fn record_normal_tool_result"),
            "tool_turn production code must not own normal tool result recording"
        );
    }

    #[test]
    fn runtime_turn_kernel_owns_sampling_request_state_and_reducer() {
        let kernel_source = include_str!("runtime_turn_kernel.rs");
        let provider_turn_source = include_str!("provider_turn.rs");
        let provider_turn_runtime_source = provider_turn_source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("provider turn runtime source");

        assert!(
            kernel_source.contains("pub(crate) struct RuntimeTurnKernel"),
            "RuntimeTurnKernel must be the named per-turn runtime kernel"
        );
        assert!(
            kernel_source.contains("sampling_state: RuntimeSamplingRequestState"),
            "RuntimeTurnKernel must own sampling-request state"
        );
        assert!(
            kernel_source.contains("reducer: RuntimeTurnReducer"),
            "RuntimeTurnKernel must own the runtime turn reducer"
        );
        assert!(
            !kernel_source.contains("pub(crate) fn sampling_state_mut("),
            "RuntimeTurnKernel must not expose mutable sampling state outside response input assembly"
        );
        assert!(
            kernel_source.contains("pub(crate) fn reducer("),
            "RuntimeTurnKernel must expose reducer access for turn transitions"
        );
        assert!(
            provider_turn_runtime_source.contains("RuntimeTurnKernel::from_extension_stores("),
            "provider_turn must construct sampling request state through RuntimeTurnKernel"
        );
        assert!(
            provider_turn_runtime_source.contains("kernel.provider_response_input("),
            "provider_turn must pass kernel-owned sampling state through response input assembly"
        );
        assert!(
            !provider_turn_runtime_source.contains("RuntimeSamplingRequestState::new()"),
            "provider_turn must not allocate sampling request state outside RuntimeTurnKernel"
        );
    }

    #[test]
    fn runtime_turn_kernel_binds_provider_step_context_extensions() {
        let kernel_source = include_str!("runtime_turn_kernel.rs");
        let provider_turn_source = include_str!("provider_turn.rs");
        let provider_turn_runtime_source = provider_turn_source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("provider turn runtime source");

        assert!(
            kernel_source.contains("extension_stores: RuntimeExtensionStores"),
            "RuntimeTurnKernel must retain the extension stores used by its reducer"
        );
        assert!(
            kernel_source.contains("#[cfg(test)]\n    pub(crate) fn bind_step_context("),
            "RuntimeTurnKernel must keep the standalone step-context binding helper test-only"
        );
        assert!(
            kernel_source.contains("RuntimeExtensionContext::new("),
            "RuntimeTurnKernel must own step-context extension binding"
        );
        assert!(
            provider_turn_runtime_source.contains("kernel.provider_response_input("),
            "provider_turn must bind RuntimeStepContext extensions through RuntimeTurnKernel response input assembly"
        );
        assert!(
            !provider_turn_runtime_source.contains(
                ".with_extensions(input.extensions.registry(), input.extensions.stores())"
            ),
            "provider_turn must not wire step-context extension stores outside RuntimeTurnKernel"
        );
    }

    #[test]
    fn runtime_turn_kernel_assembles_provider_response_input() {
        let kernel_source = include_str!("runtime_turn_kernel.rs");
        let provider_turn_source = include_str!("provider_turn.rs");
        let provider_turn_runtime_source = provider_turn_source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("provider turn runtime source");

        assert!(
            kernel_source.contains("pub(crate) fn provider_response_input<"),
            "RuntimeTurnKernel must expose a provider-response input assembly helper"
        );
        assert!(
            kernel_source.contains("RuntimeProviderResponseInput {"),
            "RuntimeTurnKernel must assemble RuntimeProviderResponseInput"
        );
        assert!(
            provider_turn_runtime_source.contains("kernel.provider_response_input("),
            "provider_turn must ask RuntimeTurnKernel to assemble provider-response input"
        );
        assert!(
            !provider_turn_runtime_source.contains("step_context: kernel.bind_step_context("),
            "provider_turn must not bind response step context as a separate field"
        );
        assert!(
            !provider_turn_runtime_source.contains("sampling_state: kernel.sampling_state_mut()"),
            "provider_turn must not expose kernel-owned sampling state as a separate field"
        );
    }

    #[test]
    fn runtime_provider_response_input_groups_io_refs_contract() {
        let kernel_source = include_str!("runtime_turn_kernel.rs");
        let provider_turn_source = include_str!("provider_turn.rs");
        let provider_turn_runtime_source = provider_turn_source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("provider turn runtime source");

        assert!(
            provider_turn_source.contains("pub(crate) struct RuntimeProviderResponseIo"),
            "provider response I/O refs must live behind a named grouped context"
        );
        assert!(
            provider_turn_source.contains("io: RuntimeProviderResponseIo<'a, W>"),
            "RuntimeProviderResponseInput must carry provider-response I/O refs through one named field"
        );

        let provider_response_input_struct = provider_turn_source
            .split("pub(crate) struct RuntimeProviderResponseInput")
            .nth(1)
            .expect("RuntimeProviderResponseInput struct body")
            .split("pub(crate) struct RuntimeProviderResponseIo")
            .next()
            .expect("RuntimeProviderResponseInput struct end");
        for field_name in [
            "events",
            "sink",
            "conversation",
            "history_writer",
            "cost_tracker",
            "background_workflows",
        ] {
            assert!(
                !provider_response_input_struct.contains(&format!("pub(crate) {field_name}:")),
                "RuntimeProviderResponseInput must not expose provider-response I/O field {field_name} outside RuntimeProviderResponseIo"
            );
        }

        assert!(
            kernel_source.contains("io: RuntimeProviderResponseIo {"),
            "RuntimeTurnKernel must assemble provider-response I/O refs through RuntimeProviderResponseIo"
        );
        assert!(
            provider_turn_source.contains("let RuntimeProviderResponseIo {"),
            "provider response handling should destructure the grouped I/O context at the execution boundary"
        );
        assert!(
            provider_turn_source.contains("pub(crate) struct RuntimeProviderResponseExecutors"),
            "provider response child executors must live behind a named grouped context"
        );
        assert!(
            provider_turn_runtime_source.contains(
                "pub(crate) fn handle<W: io::Write>(\n        &mut self,\n        response: ProviderResponse,\n        input: RuntimeProviderResponseInput<'_, W>,\n        executors: RuntimeProviderResponseExecutors<W>,"
            ),
            "RuntimeProviderResponseStep::handle must take RuntimeProviderResponseInput instead of a flat provider-response parameter list"
        );
        assert!(
            !provider_turn_runtime_source
                .contains("response,\n            input.step_context,\n            events,"),
            "provider-cycle handling must not flatten RuntimeProviderResponseInput before calling RuntimeProviderResponseStep"
        );
    }

    #[test]
    fn runtime_provider_turn_input_groups_io_refs_contract() {
        let provider_turn_source = include_str!("provider_turn.rs");
        let provider_turn_runtime_source = provider_turn_source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("provider turn runtime source");

        assert!(
            provider_turn_source.contains("pub(crate) struct RuntimeProviderTurnInput"),
            "provider turn execution must receive one named input object"
        );
        assert!(
            provider_turn_source.contains("pub(crate) struct RuntimeProviderTurnIo"),
            "provider turn I/O refs must live behind a named grouped context"
        );
        assert!(
            provider_turn_source.contains("io: RuntimeProviderTurnIo<'a, W>"),
            "RuntimeProviderTurnInput must carry provider-turn I/O refs through one named field"
        );
        assert!(
            provider_turn_source.contains("turn_context: RuntimeTurnContext<'a>"),
            "RuntimeProviderTurnInput must carry immutable turn refs through RuntimeTurnContext"
        );
        assert!(
            provider_turn_runtime_source.contains(
                "pub(crate) fn run<W: io::Write>(\n        &mut self,\n        input: RuntimeProviderTurnInput<'_, '_, W>,"
            ),
            "RuntimeProviderTurnStep::run must take RuntimeProviderTurnInput instead of a flat provider-call parameter list"
        );

        let provider_turn_input_struct = provider_turn_source
            .split("pub(crate) struct RuntimeProviderTurnInput")
            .nth(1)
            .expect("RuntimeProviderTurnInput struct body")
            .split("pub(crate) struct RuntimeProviderTurnIo")
            .next()
            .expect("RuntimeProviderTurnInput struct end");
        for field_name in [
            "events",
            "sink",
            "conversation",
            "history_writer",
            "cost_tracker",
            "cwd",
            "emit_deltas",
            "steer_handle",
        ] {
            assert!(
                !provider_turn_input_struct.contains(&format!("pub(crate) {field_name}:")),
                "RuntimeProviderTurnInput must not expose grouped field {field_name} outside its named context"
            );
        }

        assert!(
            provider_turn_runtime_source.contains("let RuntimeProviderTurnIo {"),
            "provider turn execution should destructure the grouped I/O context at the execution boundary"
        );
    }

    #[test]
    fn runtime_turn_kernel_assembles_turn_loop_state() {
        let kernel_source = include_str!("runtime_turn_kernel.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let lifecycle_runtime_source = lifecycle_source
            .split("\n#[cfg(test)]")
            .next()
            .expect("lifecycle runtime source");
        let into_loop_state_source = lifecycle_runtime_source
            .split("pub(crate) fn into_loop_state")
            .nth(1)
            .and_then(|source| source.split("\n    #[cfg(test)]").next())
            .expect("RuntimeTurnState::into_loop_state source");

        assert!(
            kernel_source.contains("pub(crate) fn turn_loop_state")
                && kernel_source.contains("turn_loop_state<'loop_state>(\n        &self,"),
            "RuntimeTurnKernel must expose turn-loop state assembly as an instance helper"
        );
        assert!(
            kernel_source.contains("RuntimeTurnLoopState {"),
            "RuntimeTurnKernel must assemble RuntimeTurnLoopState"
        );
        assert!(
            lifecycle_runtime_source.contains("let kernel = RuntimeTurnKernel::new("),
            "RuntimeTurnState must create a RuntimeTurnKernel for turn-loop state assembly"
        );
        assert!(
            lifecycle_runtime_source.contains("kernel.turn_loop_state("),
            "RuntimeTurnState must ask the RuntimeTurnKernel instance to assemble turn-loop state"
        );
        assert!(
            !into_loop_state_source.contains("RuntimeTurnKernel::turn_loop_state("),
            "RuntimeTurnState must not use a static turn-loop state assembly helper"
        );
        assert!(
            !into_loop_state_source.contains("RuntimeTurnLoopState {"),
            "RuntimeTurnState must not assemble loop state by expanding kernel-owned runtime parts"
        );
        assert!(
            !into_loop_state_source.contains("RuntimeTurnExtensionState {"),
            "RuntimeTurnState must not assemble extension state outside RuntimeTurnKernel"
        );
    }

    #[test]
    fn turn_loop_iteration_and_provider_contexts_group_runtime_extensions() {
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");
        let provider_turn_source = include_str!("provider_turn.rs");

        assert!(
            runtime_turn_loop_source.contains("loop_state: RuntimeTurnLoopState"),
            "runtime_turn_loop must carry lifecycle-owned loop state"
        );
        assert!(
            lifecycle_source.contains("extensions: runtime.extensions.extension_context()"),
            "lifecycle must derive grouped extension context from loop runtime state"
        );

        assert!(
            lifecycle_source.contains("extensions: RuntimeExtensionContext"),
            "lifecycle iteration state must carry runtime extensions as one grouped context"
        );
        assert!(
            runtime_turn_iteration_source.contains("loop_state: RuntimeTurnLoopIterationState"),
            "runtime_turn_iteration must receive grouped lifecycle iteration state"
        );
        assert!(
            runtime_turn_iteration_source.contains("extensions: input.loop_state.extensions"),
            "runtime_turn_iteration must route grouped extensions from lifecycle iteration state"
        );
        assert!(
            provider_turn_source.contains("extensions: RuntimeExtensionContext"),
            "provider_turn must carry runtime extensions as one grouped context"
        );
        for (module_name, source) in [
            ("runtime_turn_iteration", runtime_turn_iteration_source),
            ("provider_turn", provider_turn_source),
        ] {
            assert!(
                !source.contains("thread_extensions: &'a ExtensionData"),
                "{module_name} must not expose thread extension refs as a parallel input field"
            );
            assert!(
                !source.contains("turn_extensions: &'a ExtensionData"),
                "{module_name} must not expose turn extension refs as a parallel input field"
            );
        }

        assert!(
            !runtime_turn_loop_source.contains("extension_registry: self.extension_registry"),
            "runtime_turn_loop must pass grouped runtime extensions into each iteration"
        );
        assert!(
            !runtime_turn_iteration_source.contains("extension_registry: input.extension_registry"),
            "runtime_turn_iteration must pass grouped runtime extensions into provider turns"
        );
        assert!(
            !provider_turn_source.contains(
                "RuntimeExtensionStores::new(input.thread_extensions, input.turn_extensions)"
            ),
            "provider_turn must not reconstruct grouped stores from parallel refs"
        );
    }

    #[test]
    fn runtime_turn_state_exposes_grouped_runtime_extension_context() {
        let lifecycle_source = include_str!("lifecycle.rs");
        let agent_loop_source = include_str!("agent_loop.rs");
        let agent_loop_runtime_source = agent_loop_source
            .split("#[cfg(test)]")
            .next()
            .expect("agent loop runtime source");

        assert!(
            lifecycle_source
                .contains("pub(crate) fn extension_context(&self) -> RuntimeExtensionContext"),
            "RuntimeTurnState must expose the grouped runtime extension context it owns"
        );
        assert!(
            lifecycle_source.contains("RuntimeExtensionContext::new(")
                && lifecycle_source.contains("RuntimeExtensionStores::new("),
            "RuntimeTurnState::extension_context must compose registry and scoped stores"
        );
        assert!(
            !agent_loop_source.contains("RuntimeExtensionStores::new("),
            "agent_loop must not reconstruct grouped runtime extension stores from turn-state fields"
        );
        assert!(
            !agent_loop_source.contains("RuntimeExtensionContext::new("),
            "agent_loop must ask RuntimeTurnState for the grouped runtime extension context"
        );
        assert!(
            !agent_loop_runtime_source.contains("extension_context_from_parts("),
            "agent_loop runtime must not reconstruct extension context from turn-state parts"
        );
        assert!(
            !agent_loop_runtime_source.contains("extension_registry")
                && !agent_loop_runtime_source.contains("thread_extensions")
                && !agent_loop_runtime_source.contains("turn_extensions"),
            "agent_loop runtime must consume grouped loop state instead of destructuring extension fields"
        );
    }

    #[test]
    fn runtime_turn_state_directives_route_through_runtime_reducer() {
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_state_source = include_str!("runtime_state.rs");

        assert!(
            runtime_state_source.contains("pub fn apply_directive"),
            "runtime_state must expose reducer-owned runtime directive application"
        );
        assert!(
            lifecycle_source.contains("RuntimeTurnReducer::new("),
            "RuntimeTurnState::apply_directive must instantiate the runtime turn reducer"
        );
        assert!(
            !lifecycle_source.contains("self.directive_state.apply("),
            "RuntimeTurnState should not write directive state directly"
        );
    }

    #[test]
    fn server_operation_dispatch_is_owned_by_router_module() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let server_source =
            std::fs::read_to_string(manifest_dir.join("src/server.rs")).expect("server source");
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");

        assert!(
            server_source.contains("mod router;"),
            "server entry module must declare a focused router submodule"
        );
        assert!(
            server_source.contains("router::dispatch_submission("),
            "server entry module must delegate decoded submissions to router"
        );
        assert!(
            !server_source.contains("match &submission.op"),
            "server entry module must not own the ClientOp dispatch match"
        );
        assert!(
            router_source.contains("pub(super) fn dispatch_submission"),
            "server router must expose submission dispatch inside the server module"
        );
        assert!(
            router_source.contains("submit::dispatch_submit_operation("),
            "server router must delegate submit-family operations"
        );
        assert!(
            router_source.contains("command_exec::dispatch_command_exec_operation("),
            "server router must delegate command exec operations"
        );
        assert!(
            router_source.contains("permission::dispatch_permission_operation("),
            "server router must delegate permission operations"
        );
    }

    #[test]
    fn runtime_special_dispatch_is_owned_by_runtime_special_module() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let lib_source =
            std::fs::read_to_string(manifest_dir.join("src/lib.rs")).expect("lib source");
        let lifecycle_source = std::fs::read_to_string(manifest_dir.join("src/lifecycle.rs"))
            .expect("lifecycle source");
        let runtime_special_source =
            std::fs::read_to_string(manifest_dir.join("src/runtime_special.rs"))
                .expect("runtime special source");

        assert!(
            lib_source.contains("pub(crate) mod runtime_special;"),
            "runtime crate must declare a focused runtime_special module"
        );

        for marker in [
            "pub enum RuntimeSpecialToolDispatch",
            "pub fn classify_dispatch",
            "pub fn execute_request_permissions_tool",
            "pub fn execute_request_permissions_tool_with_handler",
            "pub fn execute_workflow_ipc_tool",
            "pub fn execute_subagent_status_tool",
            "pub fn execute_task_list_tool",
            "pub fn execute_task_stop_tool",
            "pub fn execute_workflow_draft_tool",
        ] {
            assert!(
                runtime_special_source.contains(marker),
                "runtime_special must own runtime-special detail {marker}"
            );
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own runtime-special detail {marker}"
            );
        }
    }

    #[test]
    fn readonly_tool_turn_is_owned_by_runtime_readonly_module() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let lib_source =
            std::fs::read_to_string(manifest_dir.join("src/lib.rs")).expect("lib source");
        let tool_turn_source = std::fs::read_to_string(manifest_dir.join("src/tool_turn.rs"))
            .expect("tool turn source");
        let readonly_source =
            std::fs::read_to_string(manifest_dir.join("src/runtime_readonly_tool_turn.rs"))
                .expect("runtime readonly tool turn source");

        assert!(
            lib_source.contains("pub(crate) mod runtime_readonly_tool_turn;"),
            "runtime crate must declare a focused readonly tool-turn module"
        );
        for marker in [
            "pub(crate) fn execute_readonly_batch<W: io::Write>",
            "pub(crate) fn should_run_readonly_batch(",
            "pub(crate) fn collect_readonly_batch(",
            "pub(crate) fn record_readonly_batch_results(",
            "pub(crate) fn run_readonly_tool_turn<W: io::Write>",
        ] {
            assert!(
                readonly_source.contains(marker),
                "runtime_readonly_tool_turn must own readonly detail {marker}"
            );
            assert!(
                !tool_turn_source.contains(marker),
                "tool_turn must not own readonly detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_event_projector_projects_reasoning_lifecycle() {
        use crate::protocol::ServerEvent;
        use crate::runtime_event_projector::RuntimeEventProjector;

        let mut projector = RuntimeEventProjector::default();
        let started = projector
            .project_line(r#"{"type":"assistant.reasoning.delta","payload":{"text":"thinking"}}"#);

        assert_eq!(started.len(), 3);
        assert!(matches!(
            &started[0],
            ServerEvent::ItemStarted { item, .. }
                if item["id"] == "item-reasoning-1"
                    && item["type"] == "reasoning"
                    && item["summary"] == ""
        ));
        assert!(matches!(
            &started[1],
            ServerEvent::ItemReasoningDelta { item_id, delta }
                if item_id == "item-reasoning-1" && delta == "thinking"
        ));
        assert!(matches!(
            &started[2],
            ServerEvent::ReasoningDelta { text } if text == "thinking"
        ));

        let completed = projector
            .project_line(r#"{"type":"session.completed","payload":{"status":"success"}}"#);
        assert!(matches!(
            &completed[0],
            ServerEvent::TurnCompleted { status } if status == "success"
        ));
        assert!(matches!(
            completed.last(),
            Some(ServerEvent::ItemCompleted { item, .. })
                if item["id"] == "item-reasoning-1"
                    && item["type"] == "reasoning"
                    && item["summary"] == "thinking"
        ));
    }

    #[test]
    fn server_thread_query_dispatch_is_owned_by_thread_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/thread.rs"))
                .expect("server thread processor source");

        assert!(
            router_source.contains("mod processors;"),
            "server router must declare focused processor modules"
        );
        assert!(
            router_source.contains("thread::dispatch_query_operation("),
            "server router must delegate thread query operations to the thread processor"
        );
        for variant in [
            "ClientOp::ThreadRead",
            "ClientOp::ThreadList",
            "ClientOp::ThreadSearch",
            "ClientOp::ThreadTurnsList",
            "ClientOp::ThreadItemsList",
            "ClientOp::ThreadMetadataUpdate",
        ] {
            assert!(
                !router_source.contains(variant),
                "server router must not own {variant} dispatch details"
            );
            assert!(
                processor_source.contains(variant),
                "server thread processor must own {variant} dispatch details"
            );
        }
        assert!(
            processor_source.contains("fn dispatch_query_operation"),
            "server thread processor must expose query dispatch inside the router module"
        );
    }

    #[test]
    fn server_turn_control_dispatch_is_owned_by_turn_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let server_source =
            std::fs::read_to_string(manifest_dir.join("src/server.rs")).expect("server source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/turn.rs"))
                .expect("server turn processor source");

        assert!(
            router_source.contains("turn::dispatch_control_operation("),
            "server router must delegate turn control operations to the turn processor"
        );
        for variant in [
            "ClientOp::TurnInterrupt",
            "ClientOp::TurnResume",
            "ClientOp::TurnSteer",
        ] {
            assert!(
                !router_source.contains(variant),
                "server router must not own {variant} dispatch details"
            );
            assert!(
                processor_source.contains(variant),
                "server turn processor must own {variant} dispatch details"
            );
        }
        assert!(
            processor_source.contains("fn dispatch_control_operation"),
            "server turn processor must expose control dispatch inside the router module"
        );
        assert!(
            !server_source.contains("fn run_turn_control"),
            "server.rs must not own turn control handler behavior"
        );
        assert!(
            processor_source.contains("fn run_turn_control"),
            "server turn processor must own turn control handler behavior"
        );
        assert!(
            processor_source.contains("ServerEvent::TurnControlled")
                && processor_source.contains("ServerEvent::ItemStarted"),
            "server turn processor must own turn-control event emission"
        );
    }

    #[test]
    fn server_shell_dispatch_is_owned_by_shell_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/shell.rs"))
                .expect("server shell processor source");

        assert!(
            router_source.contains("shell::dispatch_shell_operation("),
            "server router must delegate shell operations to the shell processor"
        );
        for variant in [
            "ClientOp::ShellStart",
            "ClientOp::ShellWrite",
            "ClientOp::ShellUpdate",
            "ClientOp::ShellClose",
            "ClientOp::ShellResize",
            "ClientOp::ShellList",
            "ClientOp::ShellRead",
            "ClientOp::ShellKill",
        ] {
            assert!(
                !router_source.contains(variant),
                "server router must not own {variant} dispatch details"
            );
            assert!(
                processor_source.contains(variant),
                "server shell processor must own {variant} dispatch details"
            );
        }
        assert!(
            processor_source.contains("fn dispatch_shell_operation"),
            "server shell processor must expose shell dispatch inside the router module"
        );
    }

    #[test]
    fn server_command_exec_dispatch_is_owned_by_command_exec_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/command_exec.rs"))
                .expect("server command exec processor source");

        assert!(
            router_source.contains("command_exec::dispatch_command_exec_operation("),
            "server router must delegate command exec operations to the command exec processor"
        );
        for variant in [
            "ClientOp::CommandExec",
            "ClientOp::CommandExecWrite",
            "ClientOp::CommandExecResize",
            "ClientOp::CommandExecTerminate",
        ] {
            assert!(
                !router_source.contains(variant),
                "server router must not own {variant} dispatch details"
            );
            assert!(
                processor_source.contains(variant),
                "server command exec processor must own {variant} dispatch details"
            );
        }
        assert!(
            processor_source.contains("fn dispatch_command_exec_operation"),
            "server command exec processor must expose command exec dispatch inside the router module"
        );
    }

    #[test]
    fn server_command_exec_manager_is_owned_by_command_exec_manager_module() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let server_source =
            std::fs::read_to_string(manifest_dir.join("src/server.rs")).expect("server source");
        let manager_source =
            std::fs::read_to_string(manifest_dir.join("src/server/command_exec_manager.rs"))
                .expect("server command exec manager source");

        assert!(
            server_source.contains("mod command_exec_manager;"),
            "server must declare the command exec manager module"
        );
        for type_name in [
            "struct CommandExecProcess",
            "struct CommandExecManager",
            "enum CommandExecDrainOutcome",
        ] {
            assert!(
                !server_source.contains(type_name),
                "server.rs must not own {type_name}"
            );
            assert!(
                manager_source.contains(type_name),
                "server/command_exec_manager.rs must own {type_name}"
            );
        }
        assert!(
            manager_source.contains("impl CommandExecManager"),
            "server/command_exec_manager.rs must own command exec manager behavior"
        );
    }

    #[test]
    fn server_active_turn_manager_is_owned_by_active_turn_manager_module() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let server_source =
            std::fs::read_to_string(manifest_dir.join("src/server.rs")).expect("server source");
        let manager_source =
            std::fs::read_to_string(manifest_dir.join("src/server/active_turn_manager.rs"))
                .expect("server active turn manager source");

        assert!(
            server_source.contains("mod active_turn_manager;"),
            "server must declare the active turn manager module"
        );
        for type_name in [
            "struct ActiveTurnControl",
            "struct ActiveTurnHandle",
            "struct ActiveTurnManager",
        ] {
            assert!(
                !server_source.contains(type_name),
                "server.rs must not own {type_name}"
            );
            assert!(
                manager_source.contains(type_name),
                "server/active_turn_manager.rs must own {type_name}"
            );
        }
        assert!(
            manager_source.contains("fn merge_completed_turn_metadata"),
            "server/active_turn_manager.rs must own completed-turn metadata merge"
        );
    }

    #[test]
    fn server_permission_manager_is_owned_by_permission_manager_module() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let server_source =
            std::fs::read_to_string(manifest_dir.join("src/server.rs")).expect("server source");
        let manager_source =
            std::fs::read_to_string(manifest_dir.join("src/server/permission_manager.rs"))
                .expect("server permission manager source");

        assert!(
            server_source.contains("mod permission_manager;"),
            "server must declare the permission manager module"
        );
        for type_name in [
            "struct PendingCommandExecPermissionRequest",
            "enum PendingPermissionRequest",
            "struct PendingPermissionManager",
            "struct ServerPermissionRequestHandler",
        ] {
            assert!(
                !server_source.contains(type_name),
                "server.rs must not own {type_name}"
            );
            assert!(
                manager_source.contains(type_name),
                "server/permission_manager.rs must own {type_name}"
            );
        }
        assert!(
            manager_source.contains("RuntimePermissionRequestHandler")
                && manager_source.contains("for ServerPermissionRequestHandler"),
            "server/permission_manager.rs must own runtime permission request handling"
        );
        assert!(
            manager_source.contains("fn insert_command_exec"),
            "server/permission_manager.rs must own command/exec pending permission insertion"
        );
    }

    #[test]
    fn server_user_input_manager_is_owned_by_user_input_manager_module() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let server_source =
            std::fs::read_to_string(manifest_dir.join("src/server.rs")).expect("server source");
        let manager_source =
            std::fs::read_to_string(manifest_dir.join("src/server/user_input_manager.rs"))
                .expect("server user input manager source");

        assert!(
            server_source.contains("mod user_input_manager;"),
            "server must declare the user input manager module"
        );
        for type_name in [
            "struct PendingUserInputRequest",
            "struct PendingUserInputManager",
            "struct ServerUserInputRequestHandler",
        ] {
            assert!(
                !server_source.contains(type_name),
                "server.rs must not own {type_name}"
            );
            assert!(
                manager_source.contains(type_name),
                "server/user_input_manager.rs must own {type_name}"
            );
        }
        assert!(
            manager_source.contains("RuntimeUserInputHandler")
                && manager_source.contains("for ServerUserInputRequestHandler"),
            "server/user_input_manager.rs must own runtime user-input request handling"
        );
    }

    #[test]
    fn server_shell_manager_is_owned_by_shell_manager_module() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let server_source =
            std::fs::read_to_string(manifest_dir.join("src/server.rs")).expect("server source");
        let manager_source =
            std::fs::read_to_string(manifest_dir.join("src/server/shell_manager.rs"))
                .expect("server shell manager source");

        assert!(
            server_source.contains("mod shell_manager;"),
            "server must declare the shell manager module"
        );
        assert!(
            !server_source.contains("shell_sessions: Option<RuntimeShellSessionManager>"),
            "server.rs must not store raw RuntimeShellSessionManager state"
        );
        assert!(
            !server_source.contains("fn shell_manager("),
            "server.rs must not own shell manager lazy initialization"
        );
        assert!(
            manager_source.contains("struct ServerShellManager"),
            "server/shell_manager.rs must own ServerShellManager"
        );
        assert!(
            manager_source.contains("Option<RuntimeShellSessionManager>"),
            "server/shell_manager.rs must own optional runtime shell session storage"
        );
        assert!(
            manager_source.contains("TaskRegistry::new_for_cwd"),
            "server/shell_manager.rs must own server shell task registry creation"
        );
        assert!(
            manager_source.contains("fn sessions_mut"),
            "server/shell_manager.rs must expose borrowed runtime shell sessions for command/exec compatibility"
        );
    }

    #[test]
    fn task_registry_cwd_constructor_uses_orca_home_task_sessions() {
        let tasks_source = include_str!("tasks.rs");
        let constructor_source = tasks_source
            .split("pub fn new_for_cwd")
            .nth(1)
            .and_then(|source| source.split("pub fn session_id").next())
            .expect("TaskRegistry::new_for_cwd source");

        assert!(
            constructor_source.contains("task_sessions_root()"),
            "TaskRegistry::new_for_cwd must resolve persistent task storage through the ORCA_HOME/home boundary"
        );
        assert!(
            !constructor_source.contains(".orca") && !constructor_source.contains("task-sessions"),
            "TaskRegistry::new_for_cwd must not assemble project .orca/task-sessions paths directly"
        );
    }

    #[test]
    fn server_permission_dispatch_is_owned_by_permission_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/permission.rs"))
                .expect("server permission processor source");

        assert!(
            router_source.contains("permission::dispatch_permission_operation("),
            "server router must delegate permission operations to the permission processor"
        );
        assert!(
            !router_source.contains("ClientOp::PermissionRespond"),
            "server router must not own PermissionRespond dispatch details"
        );
        assert!(
            processor_source.contains("ClientOp::PermissionRespond"),
            "server permission processor must own PermissionRespond dispatch details"
        );
        assert!(
            processor_source.contains("fn dispatch_permission_operation"),
            "server permission processor must expose permission dispatch inside the router module"
        );
    }

    #[test]
    fn server_user_input_dispatch_is_owned_by_user_input_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/user_input.rs"))
                .expect("server user input processor source");

        assert!(
            router_source.contains("user_input::dispatch_user_input_operation("),
            "server router must delegate user-input operations to the user-input processor"
        );
        assert!(
            !router_source.contains("ClientOp::UserInputRespond"),
            "server router must not own UserInputRespond dispatch details"
        );
        assert!(
            processor_source.contains("ClientOp::UserInputRespond"),
            "server user-input processor must own UserInputRespond dispatch details"
        );
        assert!(
            processor_source.contains("fn dispatch_user_input_operation"),
            "server user-input processor must expose user-input dispatch inside the router module"
        );
    }

    #[test]
    fn server_submit_dispatch_is_owned_by_submit_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/submit.rs"))
                .expect("server submit processor source");

        assert!(
            router_source.contains("submit::dispatch_submit_operation("),
            "server router must delegate submit-family operations to the submit processor"
        );
        for variant in [
            "ClientOp::Submit",
            "ClientOp::ThreadStart",
            "ClientOp::ThreadResume",
            "ClientOp::ThreadFork",
        ] {
            assert!(
                !router_source.contains(variant),
                "server router must not own {variant} dispatch details"
            );
            assert!(
                processor_source.contains(variant),
                "server submit processor must own {variant} dispatch details"
            );
        }
        assert!(
            processor_source.contains("fn dispatch_submit_operation"),
            "server submit processor must expose submit dispatch inside the router module"
        );
    }

    #[test]
    fn protocol_uses_focused_submodules() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let protocol_dir = manifest_dir.join("src/protocol");
        let facade = std::fs::read_to_string(manifest_dir.join("src/protocol.rs"))
            .expect("protocol facade source");

        for module in [
            "command_exec",
            "events",
            "permissions",
            "shell",
            "thread",
            "turn",
            "wire",
        ] {
            let module_path = protocol_dir.join(format!("{module}.rs"));
            assert!(
                module_path.is_file(),
                "protocol module {module} must live in src/protocol/{module}.rs"
            );
            assert!(
                facade.contains(&format!("mod {module};")),
                "protocol facade must declare focused module {module}"
            );
        }

        let command_exec =
            std::fs::read_to_string(protocol_dir.join("command_exec.rs")).unwrap_or_default();
        assert!(
            command_exec.contains("pub struct CommandExecOptions")
                && command_exec.contains("pub enum CommandSandboxPolicy"),
            "command execution wire types must be owned by protocol::command_exec"
        );

        let events = std::fs::read_to_string(protocol_dir.join("events.rs")).unwrap_or_default();
        assert!(
            events.contains("pub enum ServerEvent") && events.contains("pub fn write_server_event"),
            "server event serialization must be owned by protocol::events"
        );

        let permissions =
            std::fs::read_to_string(protocol_dir.join("permissions.rs")).unwrap_or_default();
        assert!(
            permissions.contains("pub enum PermissionResponseDecision")
                && permissions.contains("pub struct RequestPermissionProfile"),
            "permission response wire types must be owned by protocol::permissions"
        );

        assert!(
            facade.lines().count() <= 180,
            "protocol facade should stay small enough to show module boundaries at a glance"
        );

        for reexport in [
            "pub use command_exec::",
            "pub use events::",
            "pub use permissions::",
            "pub use shell::",
            "pub use wire::",
        ] {
            assert!(
                facade.contains(reexport),
                "protocol facade must preserve external API with {reexport}"
            );
        }
    }

    #[test]
    fn runtime_turn_context_types_are_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        for type_name in [
            "RuntimeTurnContext",
            "RuntimeTurnDeps",
            "RuntimeTurnState",
            "RuntimeTurnExecution",
        ] {
            assert!(
                !agent_loop_source.contains(&format!("struct {type_name}")),
                "agent_loop must not own runtime turn context type {type_name}"
            );
            assert!(
                !agent_loop_source.contains(&format!("impl<'a> {type_name}")),
                "agent_loop must not own runtime turn context behavior {type_name}"
            );
            assert!(
                lifecycle_source.contains(&format!("struct {type_name}")),
                "lifecycle must own runtime turn context type {type_name}"
            );
            assert!(
                lifecycle_source.contains(&format!("impl<'a> {type_name}")),
                "lifecycle must own runtime turn context behavior {type_name}"
            );
        }

        assert!(
            !lifecycle_source.contains("struct RuntimeTurnConfig"),
            "immutable per-turn inputs should use the Codex-style RuntimeTurnContext boundary name"
        );
    }

    #[test]
    fn runtime_turn_continuation_is_owned_by_turn_context() {
        let lifecycle_source = include_str!("lifecycle.rs");
        let agent_loop_context = lifecycle_source
            .split("pub(crate) struct AgentLoopContext")
            .nth(1)
            .and_then(|source| source.split("#[derive(Clone").next())
            .expect("AgentLoopContext source");
        let turn_context_source = lifecycle_source
            .split("pub(crate) struct RuntimeTurnContext")
            .nth(1)
            .and_then(|source| source.split("#[derive(Clone").next())
            .expect("RuntimeTurnContext source");

        assert!(
            !agent_loop_context.contains("continuation: Option<RuntimeTurnContinuation>"),
            "AgentLoopContext must not carry turn continuation outside the frozen turn context"
        );
        assert!(
            turn_context_source.contains("continuation: Option<RuntimeTurnContinuation>"),
            "RuntimeTurnContext must own the turn continuation with the other immutable turn inputs"
        );
        assert!(
            lifecycle_source.contains("impl<'a> RuntimeTurnContext<'a>")
                && lifecycle_source.contains("pub(crate) fn with_continuation(")
                && lifecycle_source.contains("pub(crate) fn continuation("),
            "RuntimeTurnContext must expose continuation construction and read access"
        );
    }

    #[test]
    fn runtime_turn_steer_handle_is_owned_by_turn_context() {
        let lifecycle_source = include_str!("lifecycle.rs");
        let agent_loop_context = lifecycle_source
            .split("pub(crate) struct AgentLoopContext")
            .nth(1)
            .and_then(|source| source.split("#[derive(Clone").next())
            .expect("AgentLoopContext source");
        let turn_context_source = lifecycle_source
            .split("pub(crate) struct RuntimeTurnContext")
            .nth(1)
            .and_then(|source| source.split("#[derive(Clone").next())
            .expect("RuntimeTurnContext source");

        assert!(
            !agent_loop_context.contains("steer_handle: Option<&'a ThreadSteerHandle>"),
            "AgentLoopContext must not carry steer handles outside the frozen turn context"
        );
        assert!(
            turn_context_source.contains("steer_handle: Option<&'a ThreadSteerHandle>"),
            "RuntimeTurnContext must own steer handles with the other immutable turn inputs"
        );
        assert!(
            lifecycle_source.contains("impl<'a> RuntimeTurnContext<'a>")
                && lifecycle_source.contains("pub(crate) fn with_steer_handle(")
                && lifecycle_source.contains("pub(crate) fn steer_handle("),
            "RuntimeTurnContext must expose steer handle construction and read access"
        );
    }

    #[test]
    fn runtime_lifecycle_state_machine_is_owned_by_runtime_lifecycle_module() {
        let lib_source = include_str!("lib.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_lifecycle_source =
            std::fs::read_to_string("src/runtime_lifecycle.rs").expect("runtime lifecycle source");

        assert!(
            lib_source.contains("mod runtime_lifecycle;"),
            "runtime crate must declare a focused runtime_lifecycle module"
        );

        for marker in [
            "pub struct RuntimeSessionLifecycle",
            "pub struct RuntimeTaskLifecycle",
            "pub enum RuntimeTaskKind",
            "pub enum RuntimeTaskStatus",
            "pub struct RuntimeTurnLifecycle",
            "pub struct RuntimeTurnRunner",
            "pub struct RuntimeStartedTurn",
            "pub struct RuntimeAdvancedTurn",
            "impl RuntimeSessionLifecycle",
            "impl<'a> RuntimeTurnRunner<'a>",
            "impl RuntimeTaskLifecycle",
            "impl RuntimeTurnLifecycle",
        ] {
            assert!(
                runtime_lifecycle_source.contains(marker),
                "runtime_lifecycle must own lifecycle state-machine detail {marker}"
            );
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own lifecycle state-machine detail {marker}"
            );
        }

        assert!(
            lifecycle_source.contains("pub use crate::runtime_lifecycle::"),
            "lifecycle must preserve existing public imports by re-exporting runtime_lifecycle types"
        );
    }

    #[test]
    fn runtime_tool_actor_context_is_owned_by_runtime_tool_actor_module() {
        let lib_source = include_str!("lib.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_tool_actor_source = std::fs::read_to_string("src/runtime_tool_actor.rs")
            .expect("runtime tool actor source");

        assert!(
            lib_source.contains("mod runtime_tool_actor;"),
            "runtime crate must declare a focused runtime_tool_actor module"
        );

        for marker in [
            "pub struct RuntimeToolActorContext",
            "impl RuntimeToolActorContext",
        ] {
            assert!(
                runtime_tool_actor_source.contains(marker),
                "runtime_tool_actor must own tool actor context detail {marker}"
            );
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own tool actor context detail {marker}"
            );
        }

        for marker in [
            "pub fn new(run_id: impl Into<String>, max_turns: u32) -> Self",
            "pub fn resolve_tool_approval(",
            "pub(crate) fn execute_normal_tool_invocation(",
            "pub fn execute_user_input_tool(",
        ] {
            assert!(
                runtime_tool_actor_source.contains(marker),
                "runtime_tool_actor must expose tool actor context adapter detail {marker}"
            );
        }

        assert!(
            lifecycle_source
                .contains("pub use crate::runtime_tool_actor::RuntimeToolActorContext;"),
            "lifecycle must preserve existing public imports by re-exporting RuntimeToolActorContext"
        );
    }

    #[test]
    fn runtime_user_input_boundary_is_owned_by_runtime_user_input_module() {
        let lib_source = include_str!("lib.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_user_input_source = std::fs::read_to_string("src/runtime_user_input.rs")
            .expect("runtime user input source");

        assert!(
            lib_source.contains("pub(crate) mod runtime_user_input;"),
            "runtime crate must declare a focused runtime_user_input module"
        );

        for marker in [
            "pub struct RuntimeUserInputRequest",
            "pub trait RuntimeUserInputHandler",
            "struct RuntimeUserInputRequestArgs",
            "pub(crate) fn parse_runtime_user_input_request(",
            "pub(crate) fn execute_user_input_tool(",
        ] {
            assert!(
                runtime_user_input_source.contains(marker),
                "runtime_user_input must own user-input runtime detail {marker}"
            );
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own user-input runtime detail {marker}"
            );
        }

        assert!(
            lifecycle_source.contains("pub use crate::runtime_user_input::"),
            "lifecycle must preserve existing public imports by re-exporting runtime user-input types"
        );
    }

    #[test]
    fn runtime_pending_interaction_boundary_is_owned_by_focused_module() {
        let lib_source = include_str!("lib.rs");
        let pending_source = std::fs::read_to_string("src/runtime_pending_interaction.rs")
            .expect("runtime pending interaction source");

        assert!(
            lib_source.contains("pub mod runtime_pending_interaction;"),
            "runtime crate must declare a focused runtime_pending_interaction module"
        );
        for marker in [
            "pub enum RuntimePendingInteractionKind",
            "pub struct RuntimePendingInteractionRecord",
            "pub struct RuntimePendingInteractionStore",
            "pub fn from_tool_approval",
            "pub fn from_permission_request",
            "pub fn from_user_input",
        ] {
            assert!(
                pending_source.contains(marker),
                "runtime_pending_interaction must own pending interaction detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_permission_boundary_is_owned_by_runtime_permission_module() {
        let lib_source = include_str!("lib.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_permission_source = std::fs::read_to_string("src/runtime_permission.rs")
            .expect("runtime permission source");

        assert!(
            lib_source.contains("pub(crate) mod runtime_permission;"),
            "runtime crate must declare a focused runtime_permission module"
        );

        for marker in [
            "pub struct RuntimePermissionRequest",
            "pub struct RuntimePermissionResponse",
            "pub trait RuntimePermissionRequestHandler",
            "pub(crate) struct AllowRequestedPermissions",
            "pub struct TurnPermissionOverlay",
            "impl TurnPermissionOverlay",
        ] {
            assert!(
                runtime_permission_source.contains(marker),
                "runtime_permission must own permission runtime detail {marker}"
            );
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own permission runtime detail {marker}"
            );
        }

        assert!(
            lifecycle_source.contains("pub use crate::runtime_permission::"),
            "lifecycle must preserve existing public imports by re-exporting runtime permission types"
        );
    }

    #[test]
    fn runtime_approval_boundary_is_owned_by_runtime_approval_module() {
        let lib_source = include_str!("lib.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_approval_source =
            std::fs::read_to_string("src/runtime_approval.rs").expect("runtime approval source");

        assert!(
            lib_source.contains("pub(crate) mod runtime_approval;"),
            "runtime crate must declare a focused runtime_approval module"
        );

        for marker in [
            "pub enum RuntimeApprovalDecision",
            "pub trait RuntimeApprovalHandler",
            "pub struct RuntimeConfigApprovalHandler",
            "impl RuntimeApprovalHandler for RuntimeConfigApprovalHandler",
        ] {
            assert!(
                runtime_approval_source.contains(marker),
                "runtime_approval must own approval runtime detail {marker}"
            );
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own approval runtime detail {marker}"
            );
        }

        assert!(
            lifecycle_source.contains("pub use crate::runtime_approval::"),
            "lifecycle must preserve existing public imports by re-exporting runtime approval types"
        );
    }

    #[test]
    fn child_agent_loop_setup_boundary_is_owned_by_focused_module() {
        let lib_source = include_str!("lib.rs");
        let agent_child_source = include_str!("agent_child.rs");
        let child_loop_setup_source = std::fs::read_to_string("src/child_agent_loop_setup.rs")
            .expect("child agent loop setup source");

        assert!(
            lib_source.contains("mod child_agent_loop_setup;"),
            "runtime crate must declare a focused child_agent_loop_setup module"
        );

        for marker in [
            "pub const DEFAULT_CHILD_AGENT_MAX_TURNS",
            "pub struct ChildAgentLoopSetup",
            "pub enum ChildAgentTurnBudget",
            "pub fn prepare_child_agent_loop",
            "pub fn advance_child_agent_turn",
            "pub fn advance_child_agent_turn_with_limit",
        ] {
            assert!(
                child_loop_setup_source.contains(marker),
                "child_agent_loop_setup must own child loop setup detail {marker}"
            );
            assert!(
                !agent_child_source.contains(marker),
                "agent_child facade must not own child loop setup detail {marker}"
            );
        }

        assert!(
            agent_child_source.contains("pub use crate::child_agent_loop_setup::"),
            "agent_child must preserve existing imports by re-exporting child loop setup APIs"
        );
    }

    #[test]
    fn child_agent_provider_turn_boundary_is_owned_by_focused_module() {
        let lib_source = include_str!("lib.rs");
        let agent_child_source = include_str!("agent_child.rs");
        let agent_child_runtime_source = agent_child_source
            .split_once("#[cfg(test)]")
            .map(|(runtime_source, _)| runtime_source)
            .unwrap_or(agent_child_source);
        let child_provider_turn_source =
            std::fs::read_to_string("src/child_agent_provider_turn.rs")
                .expect("child agent provider turn source");

        assert!(
            lib_source.contains("mod child_agent_provider_turn;"),
            "runtime crate must declare a focused child_agent_provider_turn module"
        );

        for marker in [
            "pub enum ChildAgentProviderErrorDecision",
            "pub enum ChildAgentProviderTurn",
            "pub fn route_child_agent_model",
            "pub fn run_child_agent_provider_turn",
            "pub fn compact_child_agent_conversation_if_needed",
            "pub fn handle_child_agent_provider_error",
        ] {
            assert!(
                child_provider_turn_source.contains(marker),
                "child_agent_provider_turn must own provider-turn detail {marker}"
            );
            assert!(
                !agent_child_source.contains(marker),
                "agent_child facade must not own provider-turn detail {marker}"
            );
        }

        for marker in [
            "conversation_with_hook_context",
            "RuntimeCompactionStep::new",
            "is_prompt_too_long_error",
            "PreModelCall",
            "PostModelCall",
        ] {
            assert!(
                child_provider_turn_source.contains(marker),
                "child_agent_provider_turn must keep provider-turn behavior detail {marker}"
            );
            assert!(
                !agent_child_runtime_source.contains(marker),
                "agent_child facade must delegate provider-turn behavior detail {marker}"
            );
        }

        assert!(
            agent_child_source.contains("pub use crate::child_agent_provider_turn::"),
            "agent_child must preserve existing imports by re-exporting child provider-turn APIs"
        );
    }

    #[test]
    fn child_agent_response_folding_boundary_is_owned_by_focused_module() {
        let lib_source = include_str!("lib.rs");
        let agent_child_source = include_str!("agent_child.rs");
        let agent_child_runtime_source = agent_child_source
            .split_once("#[cfg(test)]")
            .map(|(runtime_source, _)| runtime_source)
            .unwrap_or(agent_child_source);
        let child_response_folding_source =
            std::fs::read_to_string("src/child_agent_response_folding.rs")
                .expect("child agent response folding source");

        assert!(
            lib_source.contains("mod child_agent_response_folding;"),
            "runtime crate must declare a focused child_agent_response_folding module"
        );

        for marker in [
            "pub enum ChildAgentProviderResponseFold",
            "pub enum ChildAgentToolResultFold",
            "pub struct ChildAgentToolExecution",
            "pub struct ChildAgentToolContext",
            "pub fn fold_child_agent_provider_response",
            "pub fn child_agent_tool_requests",
            "pub fn fold_child_agent_tool_result",
        ] {
            assert!(
                child_response_folding_source.contains(marker),
                "child_agent_response_folding must own response/tool folding detail {marker}"
            );
            assert!(
                !agent_child_runtime_source.contains(marker),
                "agent_child facade must not own response/tool folding detail {marker}"
            );
        }

        for marker in [
            "add_usage",
            "add_assistant",
            "ProviderStep::ToolCall",
            "format_tool_result_for_model",
            "add_tool_result",
        ] {
            assert!(
                child_response_folding_source.contains(marker),
                "child_agent_response_folding must keep folding behavior detail {marker}"
            );
            assert!(
                !agent_child_runtime_source.contains(marker),
                "agent_child facade must delegate folding behavior detail {marker}"
            );
        }

        assert!(
            agent_child_source.contains("pub use crate::child_agent_response_folding::"),
            "agent_child must preserve existing imports by re-exporting child response-folding APIs"
        );
    }

    #[test]
    fn child_agent_loop_runner_boundary_is_owned_by_focused_module() {
        let lib_source = include_str!("lib.rs");
        let agent_child_source = include_str!("agent_child.rs");
        let agent_child_runtime_source = agent_child_source
            .split_once("#[cfg(test)]")
            .map(|(runtime_source, _)| runtime_source)
            .unwrap_or(agent_child_source);
        let child_loop_runner_source = std::fs::read_to_string("src/child_agent_loop_runner.rs")
            .expect("child agent loop runner source");

        assert!(
            lib_source.contains("mod child_agent_loop_runner;"),
            "runtime crate must declare a focused child_agent_loop_runner module"
        );

        for marker in [
            "pub fn run_child_agent_loop_with_tool_executor",
            "pub fn run_child_agent_with_tool_executor",
            "pub struct ChildAgentLoopContext<'a>",
        ] {
            assert!(
                child_loop_runner_source.contains(marker),
                "child_agent_loop_runner must own child loop runner detail {marker}"
            );
            assert!(
                !agent_child_runtime_source.contains(marker),
                "agent_child facade must not own child loop runner detail {marker}"
            );
        }

        for marker in [
            "prepare_child_agent_loop(",
            "advance_child_agent_turn(",
            "compact_child_agent_conversation_if_needed(",
            "run_child_agent_provider_turn(",
            "fold_child_agent_provider_response(",
            "child_agent_tool_requests(",
            "fold_child_agent_tool_result(",
        ] {
            assert!(
                child_loop_runner_source.contains(marker),
                "child_agent_loop_runner must compose child loop behavior detail {marker}"
            );
            assert!(
                !agent_child_runtime_source.contains(marker),
                "agent_child facade must delegate child loop behavior detail {marker}"
            );
        }

        for marker in [
            "pub request: &'a ChildAgentRequest",
            "pub cwd: &'a Path",
            "pub instructions: &'a ProjectInstructions",
            "pub memory: &'a MemoryBlock",
            "pub hooks: &'a HookRunner",
            "pub child_cost_tracker: &'a mut CostTracker",
            "context: ChildAgentLoopContext<'_>",
        ] {
            assert!(
                child_loop_runner_source.contains(marker),
                "child_agent_loop_runner must group loop runner input behind {marker}"
            );
        }
        assert!(
            !child_loop_runner_source.contains(
                "request: &ChildAgentRequest,\n    cwd: &Path,\n    instructions: &ProjectInstructions,\n    memory: &MemoryBlock,\n    hooks: &HookRunner,\n    child_cost_tracker: &mut CostTracker,"
            ),
            "child-agent loop runner must not expose a long request/environment argument list"
        );

        assert!(
            agent_child_source.contains("pub use crate::child_agent_loop_runner::"),
            "agent_child must preserve existing imports by re-exporting child loop runner APIs"
        );
    }

    #[test]
    fn child_agent_types_boundary_is_owned_by_focused_module() {
        let lib_source = include_str!("lib.rs");
        let agent_child_source = include_str!("agent_child.rs");
        let agent_child_runtime_source = agent_child_source
            .split_once("#[cfg(test)]")
            .map(|(runtime_source, _)| runtime_source)
            .unwrap_or(agent_child_source);
        let child_types_source =
            std::fs::read_to_string("src/child_agent_types.rs").expect("child agent types source");

        assert!(
            lib_source.contains("mod child_agent_types;"),
            "runtime crate must declare a focused child_agent_types module"
        );

        for marker in [
            "pub struct ChildAgentRequest",
            "impl ChildAgentRequest",
            "pub struct ChildAgentResult",
            "pub(crate) type ChildAgentExecutor",
            "pub(crate) struct ChildAgentRuntime",
            "pub(crate) struct ChildAgentRuntimeContext<'a, W: io::Write>",
            "impl<'a, W: io::Write> ChildAgentRuntime<'a, W>",
        ] {
            assert!(
                child_types_source.contains(marker),
                "child_agent_types must own child-agent shared type {marker}"
            );
            assert!(
                !agent_child_runtime_source.contains(marker),
                "agent_child facade must not own child-agent shared type {marker}"
            );
        }

        for marker in [
            "pub cwd: &'a Path",
            "pub events: &'a mut EventFactory",
            "pub sink: &'a mut EventSink<W>",
            "pub instructions: &'a ProjectInstructions",
            "pub memory: &'a MemoryBlock",
            "pub mcp_registry: &'a McpRegistry",
            "pub hooks: &'a HookRunner",
            "pub cancel: &'a CancelToken",
            "pub lifecycle: Option<&'a mut RuntimeSessionLifecycle>",
            "pub executor: ChildAgentExecutor<W>",
            "pub(crate) fn new(context: ChildAgentRuntimeContext<'a, W>) -> Self",
        ] {
            assert!(
                child_types_source.contains(marker),
                "child_agent_types must group child runtime constructor input behind {marker}"
            );
        }
        assert!(
            !child_types_source.contains(
                "cwd: &'a Path,\n        events: &'a mut EventFactory,\n        sink: &'a mut EventSink<W>,\n        instructions: &'a ProjectInstructions,\n        memory: &'a MemoryBlock,\n        mcp_registry: &'a McpRegistry,\n        hooks: &'a HookRunner,\n        cancel: &'a CancelToken,\n        lifecycle: Option<&'a mut RuntimeSessionLifecycle>,\n        executor: ChildAgentExecutor<W>,"
            ),
            "child-agent runtime constructor must not expose a long runtime dependency list"
        );

        assert!(
            agent_child_source.contains("pub use crate::child_agent_types::"),
            "agent_child must preserve existing imports by re-exporting child-agent shared types"
        );
    }

    #[test]
    fn child_agent_behavior_tests_are_owned_by_focused_module() {
        let lib_source = include_str!("lib.rs");
        let agent_child_source = include_str!("agent_child.rs");
        let child_tests_source =
            std::fs::read_to_string("src/child_agent_tests.rs").expect("child agent tests source");

        assert!(
            lib_source.contains("mod child_agent_tests;"),
            "runtime crate must declare focused child-agent behavior tests"
        );
        assert!(
            !agent_child_source.contains("#[cfg(test)]"),
            "agent_child facade must not own the child-agent behavior test module"
        );

        for marker in [
            "fn config(model: Option<&str>) -> RunConfig",
            "fn runtime<'a>(",
            "prepare_child_agent_loop_builds_provider_conversation_and_policy",
            "run_child_agent_loop_with_tool_executor_runs_tools_until_provider_completes",
            "run_child_agent_prompt_with_tool_executor_builds_runtime_request",
        ] {
            assert!(
                child_tests_source.contains(marker),
                "child_agent_tests must own behavior test detail {marker}"
            );
        }
    }

    #[test]
    fn child_agent_entrypoints_are_owned_by_focused_module() {
        let lib_source = include_str!("lib.rs");
        let agent_child_source = include_str!("agent_child.rs");
        let child_entrypoints_source = std::fs::read_to_string("src/child_agent_entrypoints.rs")
            .expect("child agent entrypoints source");

        assert!(
            lib_source.contains("mod child_agent_entrypoints;"),
            "runtime crate must declare a focused child_agent_entrypoints module"
        );
        assert!(
            agent_child_source.contains("pub use crate::child_agent_entrypoints::"),
            "agent_child must preserve existing imports by re-exporting child-agent entrypoints"
        );

        for marker in [
            "pub(crate) fn run_child_agent<W: io::Write>",
            "pub fn run_child_agent_with_executor<F>",
            "pub fn run_child_agent_prompt_with_tool_executor<F>",
        ] {
            assert!(
                child_entrypoints_source.contains(marker),
                "child_agent_entrypoints must own child-agent entrypoint {marker}"
            );
            assert!(
                !agent_child_source.contains(marker),
                "agent_child facade must not own child-agent entrypoint {marker}"
            );
        }

        for marker in [
            "pub struct ChildAgentPromptContext<'a>",
            "pub prompt: String",
            "pub subagent_type: &'a SubagentType",
            "pub subagent_model: Option<String>",
            "pub subagent_depth: u32",
            "pub cwd: &'a Path",
            "pub instructions: &'a ProjectInstructions",
            "pub memory: &'a MemoryBlock",
            "pub hooks: &'a HookRunner",
            "context: ChildAgentPromptContext<'_>",
        ] {
            assert!(
                child_entrypoints_source.contains(marker),
                "child_agent_entrypoints must group prompt entrypoint input behind {marker}"
            );
        }
        assert!(
            !child_entrypoints_source.contains("prompt: String,\n    subagent_type: &SubagentType"),
            "child-agent prompt entrypoint must not expose a long prompt/subagent argument list"
        );
    }

    #[test]
    fn thread_steer_handle_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        assert!(
            !agent_loop_source.contains("struct ThreadSteerHandle"),
            "agent_loop must not own the thread turn steer handle"
        );
        assert!(
            !agent_loop_source.contains("impl ThreadSteerHandle"),
            "agent_loop must not own thread turn steer handle behavior"
        );
        assert!(
            lifecycle_source.contains("struct ThreadSteerHandle"),
            "lifecycle must own the thread turn steer handle"
        );
        assert!(
            lifecycle_source.contains("impl ThreadSteerHandle"),
            "lifecycle must own thread turn steer handle behavior"
        );
    }

    #[test]
    fn runtime_steer_step_is_owned_by_runtime_steer_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lib_source = include_str!("lib.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_steer_source =
            std::fs::read_to_string("src/runtime_steer.rs").expect("runtime steer source");

        assert!(
            !agent_loop_source.contains("struct RuntimeSteerStep"),
            "agent_loop must not own runtime steer step state"
        );
        assert!(
            !agent_loop_source.contains("impl RuntimeSteerStep"),
            "agent_loop must not own runtime steer step behavior"
        );
        assert!(
            lib_source.contains("mod runtime_steer;"),
            "runtime crate must declare a focused runtime_steer module"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeSteerStep"),
            "lifecycle must not own runtime steer step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeSteerStep"),
            "lifecycle must not own runtime steer step behavior"
        );
        assert!(
            !agent_loop_source.contains("for input in steer_handle.drain()"),
            "agent_loop must not directly drain steer inputs into conversation"
        );
        for marker in [
            "struct RuntimeSteerStep",
            "struct RuntimeSteerInput",
            "impl RuntimeSteerStep",
            "for steer_input in steer_handle.drain()",
            "input.conversation.add_user(steer_input)",
            "writer.append_message(message)",
        ] {
            assert!(
                runtime_steer_source.contains(marker),
                "runtime_steer must own runtime steer detail {marker}"
            );
        }
        assert!(
            runtime_steer_source.contains("turn_context: RuntimeTurnContext<'a>"),
            "RuntimeSteerInput must carry immutable turn refs through RuntimeTurnContext"
        );
        let runtime_steer_input = runtime_steer_source
            .split("struct RuntimeSteerInput")
            .nth(1)
            .and_then(|source| source.split("impl RuntimeSteerStep").next())
            .expect("RuntimeSteerInput source");
        assert!(
            !runtime_steer_input.contains("steer_handle: Option<&'a ThreadSteerHandle>"),
            "RuntimeSteerInput must not duplicate turn-entry steer_handle"
        );
    }

    #[test]
    fn agent_loop_context_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        assert!(
            !agent_loop_source.contains("struct AgentLoopContext"),
            "agent_loop must not own the runtime agent loop context"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> AgentLoopContext"),
            "agent_loop must not own runtime agent loop context behavior"
        );
        assert!(
            lifecycle_source.contains("struct AgentLoopContext"),
            "lifecycle must own the runtime agent loop context"
        );
        assert!(
            lifecycle_source.contains("impl<'a> AgentLoopContext"),
            "lifecycle must own runtime agent loop context behavior"
        );
    }

    #[test]
    fn agent_tool_policy_context_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        assert!(
            !agent_loop_source.contains("struct AgentToolPolicyContext"),
            "agent_loop must not own agent tool policy context"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> AgentToolPolicyContext"),
            "agent_loop must not own agent tool policy behavior"
        );
        assert!(
            tool_invocation_source.contains("struct AgentToolPolicyContext"),
            "tool_invocation must own agent tool policy context"
        );
        assert!(
            tool_invocation_source.contains("impl<'a> AgentToolPolicyContext"),
            "tool_invocation must own agent tool policy behavior"
        );
    }

    #[test]
    fn agent_tool_schema_override_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        for marker in [
            "deepseek_tools_schema_for_allowed_names_with_mcp_and_external",
            "deepseek_tools_schema_for_type_with_mcp_and_external",
            "deepseek_tools_schema_with_mcp_and_external",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own provider tool schema override detail {marker}"
            );
            assert!(
                tool_invocation_source.contains(marker),
                "tool_invocation must own provider tool schema override detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnSetupStep"),
            "agent_loop must delegate provider tool schema override through runtime turn setup"
        );
        assert!(
            include_str!("runtime_turn_setup.rs").contains("provider_config_for_agent_loop"),
            "runtime_turn_setup must delegate provider tool schema override through provider config construction"
        );
        assert!(
            tool_invocation_source.contains("pub(crate) fn provider_tool_schema_override"),
            "tool_invocation must expose provider tool schema override construction"
        );
    }

    #[test]
    fn provider_tool_request_extraction_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        assert!(
            !agent_loop_source.contains("ProviderStep::ToolCall"),
            "agent_loop must not match provider tool-call steps directly"
        );
        assert!(
            !agent_loop_source.contains("tool_requests_from_provider_steps("),
            "agent_loop must delegate provider tool request extraction through turn loop"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate provider tool request extraction through turn loop"
        );
        assert!(
            tool_invocation_source.contains("pub(crate) fn tool_requests_from_provider_steps"),
            "tool_invocation must expose provider tool request extraction"
        );
        assert!(
            tool_invocation_source.contains("ProviderStep::ToolCall"),
            "tool_invocation must own provider tool-call step matching"
        );
    }

    #[test]
    fn normal_tool_execution_entrypoint_is_owned_by_tool_execution_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_execution_source = include_str!("tool_execution.rs");

        assert!(
            !agent_loop_source.contains("fn execute_tool_with_approval"),
            "agent_loop must not own normal tool execution entrypoint"
        );
        assert!(
            !agent_loop_source.contains("execute_tool_with_approval("),
            "agent_loop must delegate normal tool execution through tool-turn dispatch runner"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate normal tool turn execution through turn loop"
        );
        assert!(
            tool_execution_source.contains("pub(crate) fn execute_tool_with_approval"),
            "tool_execution must expose normal tool execution entrypoint"
        );
        assert!(
            tool_execution_source.contains("ToolExecutionActor::new"),
            "tool_execution must own tool actor construction"
        );
    }

    #[test]
    fn tool_request_cursor_is_owned_by_sampling_request_state() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");
        let step_context_source = include_str!("step_context.rs");

        for marker in ["let mut index = 0", "index += 1", "index = batch_end"] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own tool request cursor detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("ToolRequestCursor"),
            "agent_loop must delegate tool request cursor state through tool-turn dispatch"
        );
        assert!(
            !tool_invocation_source.contains("struct ToolRequestCursor"),
            "tool_invocation must not own tool-turn cursor state"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate tool request cursor use through turn loop"
        );
        assert!(
            step_context_source.contains("tool_cursor_index: usize"),
            "sampling request state must own tool request cursor position"
        );
        assert!(
            !tool_turn_source
                .split("mod tests")
                .next()
                .expect("tool turn runtime source")
                .contains("struct ToolRequestCursor"),
            "tool_turn production code must not own tool request cursor state"
        );
        assert!(
            step_context_source.contains("fn advance_tool_cursor_to"),
            "sampling request state must own cursor batch advancement"
        );
    }

    #[test]
    fn tool_turn_outcome_is_owned_by_tool_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        assert!(
            !agent_loop_source.contains("return Ok(AgentLoopResult {\n                            status,\n                            final_message: None,\n                            error,\n                        });"),
            "agent_loop must not own tool-turn terminal result shape"
        );
        assert!(
            !agent_loop_source.contains("ToolTurnOutcome"),
            "agent_loop must delegate tool-turn outcome state through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderResponseOutcome"),
            "agent_loop must delegate provider response outcome folding through lifecycle"
        );
        assert!(
            !tool_invocation_source.contains("enum ToolTurnOutcome"),
            "tool_invocation must not own tool-turn outcome state"
        );
        assert!(
            tool_turn_source.contains("pub(crate) enum ToolTurnOutcome"),
            "tool_turn must own tool-turn outcome state"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn terminal_tool_turn"),
            "tool_turn must expose terminal tool-turn construction"
        );
    }

    #[test]
    fn normal_tool_turn_runner_is_owned_by_tool_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        for marker in [
            "execute_tool_with_approval(",
            "ToolExecutionContext::new",
            "record_normal_tool_result(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own normal tool-turn runner detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("run_normal_tool_turn("),
            "agent_loop must delegate normal tool-turn execution through tool-turn dispatch"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate normal tool-turn execution through turn loop"
        );
        assert!(
            !tool_invocation_source.contains("fn run_normal_tool_turn"),
            "tool_invocation must not own normal tool-turn runner"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn run_normal_tool_turn"),
            "tool_turn must expose normal tool-turn runner"
        );
        assert!(
            tool_turn_source.contains("execute_tool_with_approval"),
            "tool_turn must compose normal tool execution"
        );
        assert!(
            tool_turn_source.contains("record_normal_tool_result"),
            "tool_turn must compose normal tool result recording"
        );
    }

    #[test]
    fn normal_tool_turn_runner_uses_grouped_context() {
        let tool_turn_source = include_str!("tool_turn.rs");

        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeNormalToolTurnContext"),
            "tool_turn must group normal tool-turn inputs into RuntimeNormalToolTurnContext"
        );
        assert!(
            tool_turn_source.contains("context: RuntimeNormalToolTurnContext<"),
            "run_normal_tool_turn must accept the grouped normal tool-turn context"
        );
        assert!(
            tool_turn_source.contains("run_normal_tool_turn(RuntimeNormalToolTurnContext"),
            "run_tool_turns must pass normal tool-turn inputs as one grouped context"
        );
        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeNormalToolTurnIo"),
            "tool_turn must group normal tool-turn I/O refs into RuntimeNormalToolTurnIo"
        );
        assert!(
            tool_turn_source.contains("io: RuntimeNormalToolTurnIo<'a, W>"),
            "RuntimeNormalToolTurnContext must carry normal tool-turn I/O refs through one named field"
        );
        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeNormalToolTurnExecutors"),
            "tool_turn must group normal tool-turn executors into RuntimeNormalToolTurnExecutors"
        );
        assert!(
            tool_turn_source.contains("executors: RuntimeNormalToolTurnExecutors<W>"),
            "RuntimeNormalToolTurnContext must carry normal tool-turn executors through one named field"
        );
        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeNormalToolTurnServices"),
            "tool_turn must group normal tool-turn services into RuntimeNormalToolTurnServices"
        );
        assert!(
            tool_turn_source.contains("services: RuntimeNormalToolTurnServices<'a>"),
            "RuntimeNormalToolTurnContext must carry normal tool-turn services through one named field"
        );
        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeNormalToolTurnRuntime"),
            "tool_turn must group normal tool-turn runtime refs into RuntimeNormalToolTurnRuntime"
        );
        assert!(
            tool_turn_source.contains("runtime: RuntimeNormalToolTurnRuntime<'a>"),
            "RuntimeNormalToolTurnContext must carry normal tool-turn runtime refs through one named field"
        );
        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeNormalToolTurnInteractions"),
            "tool_turn must group normal tool-turn interaction handlers into RuntimeNormalToolTurnInteractions"
        );
        assert!(
            tool_turn_source.contains("interactions: RuntimeNormalToolTurnInteractions<'a>"),
            "RuntimeNormalToolTurnContext must carry normal tool-turn interaction handlers through one named field"
        );
        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeNormalToolTurnRequest"),
            "tool_turn must group normal tool-turn request snapshot refs into RuntimeNormalToolTurnRequest"
        );
        assert!(
            tool_turn_source.contains("request: RuntimeNormalToolTurnRequest<'a>"),
            "RuntimeNormalToolTurnContext must carry normal tool-turn request snapshot refs through one named field"
        );
        assert!(
            !tool_turn_source.contains("run_normal_tool_turn(\n            config,"),
            "run_tool_turns must not call run_normal_tool_turn with the old long argument list"
        );
        let normal_context = tool_turn_source
            .split("pub(crate) struct RuntimeNormalToolTurnContext")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) struct RuntimeNormalToolTurnRequest")
                    .next()
            })
            .expect("RuntimeNormalToolTurnContext source");
        for field_name in ["sampling_state:"] {
            assert!(
                normal_context.contains(field_name),
                "RuntimeNormalToolTurnContext must carry normal tool-turn field {field_name}"
            );
        }
        for field_name in [
            "events:",
            "sink:",
            "conversation:",
            "history_writer:",
            "cost_tracker:",
            "background_workflows:",
            "child_executor:",
            "workflow_child_executor:",
            "instructions:",
            "memory:",
            "mcp_registry:",
            "hooks:",
            "cancel:",
            "task_registry:",
            "workflow_ipc:",
            "permission_handler:",
            "user_input_handler:",
            "config:",
            "cwd:",
            "tool_request:",
            "subagent_depth:",
            "emit_deltas:",
            "policy:",
        ] {
            assert!(
                !normal_context.contains(field_name),
                "RuntimeNormalToolTurnContext must not expose grouped field {field_name} directly"
            );
        }
    }

    #[test]
    fn bash_runtime_runner_uses_grouped_invocation_context() {
        let normal_tool_source = include_str!("runtime_normal_tool.rs");
        let runtime_bash_source = include_str!("runtime_bash.rs");

        assert!(
            runtime_bash_source.contains("pub(crate) struct RuntimeBashInvocationContext"),
            "runtime_bash must group shell-session bash inputs into RuntimeBashInvocationContext"
        );
        assert!(
            runtime_bash_source.contains("context: RuntimeBashInvocationContext"),
            "execute_bash_with_shell_session must accept the grouped bash invocation context"
        );
        assert!(
            !runtime_bash_source.contains(
                "#[allow(clippy::too_many_arguments)]\npub(crate) fn execute_bash_with_shell_session"
            ),
            "runtime_bash must not need a too_many_arguments escape hatch for bash invocation"
        );
        assert!(
            normal_tool_source.contains("RuntimeBashInvocationContext"),
            "runtime_normal_tool must construct the grouped bash invocation context"
        );
        assert!(
            !normal_tool_source
                .contains("execute_bash_with_shell_session(\n                config,"),
            "runtime_normal_tool must not pass bash execution state through the old long argument list"
        );
        for field_name in [
            "config:",
            "request:",
            "cwd:",
            "additional_roots:",
            "output_truncation:",
            "shell_timeout_secs:",
            "task_registry:",
            "cancel:",
            "permission_handler:",
            "permission_overlay:",
        ] {
            assert!(
                runtime_bash_source.contains(field_name),
                "RuntimeBashInvocationContext must carry bash field {field_name}"
            );
        }
    }

    #[test]
    fn runtime_bash_internal_execution_uses_grouped_contexts() {
        let runtime_bash_source = include_str!("runtime_bash.rs");

        for marker in [
            "struct RuntimeBashSandboxContext",
            "struct RuntimeBashOnceContext",
            "fn execute_bash_with_sandbox(context: RuntimeBashSandboxContext",
            "fn execute_bash_once(context: RuntimeBashOnceContext",
        ] {
            assert!(
                runtime_bash_source.contains(marker),
                "runtime_bash must own grouped internal bash execution detail {marker}"
            );
        }

        assert!(
            !runtime_bash_source.contains("#[allow(clippy::too_many_arguments)]"),
            "runtime_bash internal bash execution must not need too_many_arguments escape hatches"
        );

        for field_name in [
            "command:",
            "cwd:",
            "additional_roots:",
            "sandbox:",
            "shell_timeout_secs:",
            "task_registry:",
            "cancel:",
        ] {
            assert!(
                runtime_bash_source.contains(field_name),
                "RuntimeBashSandboxContext must carry sandbox field {field_name}"
            );
        }

        for field_name in [
            "additional_readable_directories:",
            "additional_working_directories:",
            "denied_working_directories:",
            "allowed_unix_socket_roots:",
            "env:",
            "sandbox:",
        ] {
            assert!(
                runtime_bash_source.contains(field_name),
                "RuntimeBashOnceContext must carry shell-spawn field {field_name}"
            );
        }
    }

    #[test]
    fn tool_turn_dispatch_uses_grouped_context() {
        let provider_turn_source = include_str!("provider_turn.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeToolTurnsContext"),
            "tool_turn must group dispatch-loop inputs into RuntimeToolTurnsContext"
        );
        assert!(
            tool_turn_source.contains("context: RuntimeToolTurnsContext<"),
            "run_tool_turns must accept the grouped dispatch-loop context"
        );
        assert!(
            provider_turn_source.contains("run_tool_turns(RuntimeToolTurnsContext"),
            "provider response must pass tool-turn dispatch inputs as one grouped context"
        );
        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeToolTurnsIo"),
            "tool_turn must group dispatch-loop I/O refs into RuntimeToolTurnsIo"
        );
        assert!(
            tool_turn_source.contains("io: RuntimeToolTurnsIo<'a, W>"),
            "RuntimeToolTurnsContext must carry tool-turn I/O refs through one named field"
        );
        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeToolTurnsExecutors"),
            "tool_turn must group child-agent dispatch executors into RuntimeToolTurnsExecutors"
        );
        assert!(
            tool_turn_source.contains("executors: RuntimeToolTurnsExecutors<W>"),
            "RuntimeToolTurnsContext must carry child-agent dispatch executors through one named field"
        );
        assert!(
            !provider_turn_source.contains("run_tool_turns(\n            step_context,"),
            "provider response must not call run_tool_turns with the old long argument list"
        );
        let tool_turn_context = tool_turn_source
            .split("pub(crate) struct RuntimeToolTurnsContext")
            .nth(1)
            .and_then(|source| source.split("pub(crate) struct RuntimeToolTurnsIo").next())
            .expect("RuntimeToolTurnsContext source");
        for field_name in ["step_context:", "tool_requests:"] {
            assert!(
                tool_turn_context.contains(field_name),
                "RuntimeToolTurnsContext must carry tool-turn dispatch field {field_name}"
            );
        }
        for field_name in [
            "events:",
            "sink:",
            "conversation:",
            "history_writer:",
            "cost_tracker:",
            "background_workflows:",
            "child_executor:",
            "workflow_child_executor:",
            "batch_child_executor:",
        ] {
            assert!(
                !tool_turn_context.contains(field_name),
                "RuntimeToolTurnsContext must not expose grouped field {field_name} directly"
            );
        }
    }

    #[test]
    fn readonly_tool_turn_runner_is_owned_by_runtime_readonly_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");
        let readonly_source = include_str!("runtime_readonly_tool_turn.rs");

        for marker in ["execute_readonly_batch(", "record_readonly_batch_results("] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own readonly tool-turn runner detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("run_readonly_tool_turn("),
            "agent_loop must delegate readonly tool-turn execution through tool-turn dispatch"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate readonly tool-turn execution through turn loop"
        );
        assert!(
            !tool_invocation_source.contains("fn run_readonly_tool_turn"),
            "tool_invocation must not own readonly tool-turn runner"
        );
        assert!(
            !tool_turn_source.contains("pub(crate) fn run_readonly_tool_turn"),
            "tool_turn must not own readonly tool-turn runner"
        );
        assert!(
            readonly_source.contains("pub(crate) fn run_readonly_tool_turn"),
            "runtime_readonly_tool_turn must expose readonly tool-turn runner"
        );
        assert!(
            readonly_source.contains("execute_readonly_batch"),
            "runtime_readonly_tool_turn must compose readonly batch execution"
        );
        assert!(
            readonly_source.contains("record_readonly_batch_results"),
            "runtime_readonly_tool_turn must compose readonly batch result recording"
        );
    }

    #[test]
    fn child_tool_policy_gate_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        assert!(
            !agent_loop_source.contains("fn child_tool_policy_failure"),
            "agent_loop must not own child tool policy gate behavior"
        );
        assert!(
            tool_invocation_source.contains("fn child_tool_policy_failure"),
            "tool_invocation must own child tool policy gate behavior"
        );
        assert!(
            tool_invocation_source.contains("pub(crate) fn reject_disallowed_child_tool"),
            "tool_invocation must expose child tool policy gate to the agent loop"
        );
    }

    #[test]
    fn normal_tool_result_recording_is_owned_by_sampling_request_state() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let step_context_source = include_str!("step_context.rs");
        let tool_turn_source = include_str!("tool_turn.rs");
        let tool_turn_runtime_source = tool_turn_source
            .split("mod tests")
            .next()
            .expect("tool turn runtime source");

        for marker in [
            "record_plan_state_for_agent(",
            "status == RunStatus::ApprovalRequired",
            "tool_request.name == tool_types::ToolName::Subagent",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own normal tool result detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("record_normal_tool_result("),
            "agent_loop must delegate normal tool result recording through tool-turn dispatch"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate normal tool turn recording through turn loop"
        );
        assert!(
            !tool_invocation_source.contains("fn record_normal_tool_result"),
            "tool_invocation must not own normal tool result recording"
        );
        assert!(
            step_context_source.contains("fn record_normal_tool_result("),
            "sampling request state must expose normal tool result recording"
        );
        assert!(
            step_context_source.contains("record_plan_state_for_agent"),
            "sampling request state must own normal tool plan-state recording"
        );
        assert!(
            step_context_source.contains("record_tool_result_for_agent"),
            "sampling request state must own normal tool result recording"
        );
        assert!(
            tool_turn_runtime_source.contains(".record_normal_tool_result("),
            "tool_turn must delegate normal tool result recording to sampling request state"
        );
        assert!(
            !tool_turn_runtime_source.contains("record_plan_state_for_agent"),
            "tool_turn production code must not own normal tool plan-state recording"
        );
    }

    #[test]
    fn readonly_tool_batch_is_owned_by_runtime_readonly_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");
        let readonly_source = include_str!("runtime_readonly_tool_turn.rs");

        assert!(
            !agent_loop_source.contains("fn execute_readonly_batch"),
            "agent_loop must not own readonly tool batch execution"
        );
        assert!(
            !tool_invocation_source.contains("fn execute_readonly_batch"),
            "tool_invocation must not own readonly tool batch execution"
        );
        assert!(
            !tool_turn_source.contains("pub(crate) fn execute_readonly_batch"),
            "tool_turn must not own readonly tool batch execution"
        );
        assert!(
            readonly_source.contains("pub(crate) fn execute_readonly_batch"),
            "runtime_readonly_tool_turn must expose readonly tool batch execution"
        );
        assert!(
            readonly_source.contains("pub(crate) fn should_run_readonly_batch"),
            "runtime_readonly_tool_turn must expose readonly batch planning"
        );
        assert!(
            readonly_source.contains("pub(crate) fn collect_readonly_batch"),
            "runtime_readonly_tool_turn must expose readonly batch range collection"
        );
        for marker in [
            "orca_tools::should_run_readonly_batch",
            "orca_tools::collect_readonly_batch",
            "run_readonly_batch_parallel_with_policy",
            "HookEvent::PreToolUse",
            "HookEvent::PostToolUse",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own readonly batch detail {marker}"
            );
        }
    }

    #[test]
    fn readonly_tool_batch_result_recording_is_owned_by_runtime_readonly_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");
        let readonly_source = include_str!("runtime_readonly_tool_turn.rs");

        assert!(
            !agent_loop_source.contains("record_tool_result_for_agent("),
            "agent_loop must not own readonly batch result recording"
        );
        assert!(
            !agent_loop_source.contains("record_readonly_batch_results("),
            "agent_loop must delegate readonly batch result recording through tool-turn dispatch"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate readonly tool turn recording through turn loop"
        );
        assert!(
            !tool_invocation_source.contains("fn record_readonly_batch_results"),
            "tool_invocation must not own readonly batch result recording"
        );
        assert!(
            !tool_turn_source.contains("pub(crate) fn record_readonly_batch_results"),
            "tool_turn must not own readonly batch result recording"
        );
        assert!(
            readonly_source.contains("pub(crate) fn record_readonly_batch_results"),
            "runtime_readonly_tool_turn must expose readonly batch result recording"
        );
        assert!(
            readonly_source.contains("record_tool_result_for_agent"),
            "runtime_readonly_tool_turn must reuse shared session tool result recording"
        );
    }

    #[test]
    fn subagent_batch_result_recording_is_owned_by_subagent_execution_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let subagent_execution_source = include_str!("subagent_execution.rs");

        assert!(
            !agent_loop_source.contains("for (status, result) in results"),
            "agent_loop must not own subagent batch result recording"
        );
        assert!(
            subagent_execution_source.contains("pub(crate) fn record_subagent_batch_results"),
            "subagent_execution must expose subagent batch result recording"
        );
        assert!(
            subagent_execution_source.contains("record_tool_result_for_agent"),
            "subagent_execution must record subagent batch tool results"
        );
        assert!(
            subagent_execution_source.contains("RunStatus::ApprovalRequired"),
            "subagent_execution must own subagent batch approval folding"
        );
    }

    #[test]
    fn subagent_batch_tool_turn_runner_is_owned_by_subagent_execution_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let subagent_execution_source = include_str!("subagent_execution.rs");

        for marker in [
            "execute_subagent_batch(",
            "record_subagent_batch_results(",
            "SubagentBatchRecordOutcome",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own subagent batch tool-turn detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("run_subagent_batch_tool_turn("),
            "agent_loop must delegate subagent batch tool turns through tool-turn dispatch"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate subagent batch tool turns through turn loop"
        );
        assert!(
            subagent_execution_source.contains("pub(crate) fn run_subagent_batch_tool_turn"),
            "subagent_execution must expose subagent batch tool-turn runner"
        );
        assert!(
            subagent_execution_source.contains("execute_subagent_batch"),
            "subagent_execution must compose subagent batch execution"
        );
        assert!(
            subagent_execution_source.contains("record_subagent_batch_results"),
            "subagent_execution must compose subagent batch result recording"
        );
    }

    #[test]
    fn async_subagent_worker_is_owned_by_async_worker_module() {
        let lib_source = include_str!("lib.rs");
        let subagent_execution_source = include_str!("subagent_execution.rs");
        let async_worker_source = include_str!("subagent_async_worker.rs");

        assert!(
            lib_source.contains("pub mod subagent_async_worker;"),
            "orca-runtime must expose the async subagent worker module"
        );
        for marker in [
            "pub fn run_async_subagent_worker(",
            "fn spawn_async_subagent_worker(",
            "fn async_subagent_result_payload(",
        ] {
            assert!(
                !subagent_execution_source.contains(marker),
                "subagent_execution must not own async worker detail {marker}"
            );
        }
        for marker in [
            "pub struct AsyncSubagentWorkerInput",
            "pub config: RunConfig",
            "pub cwd: PathBuf",
            "pub child_cwd: PathBuf",
            "pub task_session_id: String",
            "pub agent_id: String",
            "pub request: subagent::SubagentRequest",
            "pub child_depth: u32",
            "pub worktree: Option<AsyncSubagentWorktree>",
            "pub(crate) struct AsyncSubagentWorkerContext",
            "pub input: AsyncSubagentWorkerInput",
            "pub child_executor: ChildAgentExecutor<io::Sink>",
            "pub(crate) struct AsyncSubagentLaunchContext<'a>",
            "pub config: &'a RunConfig",
            "pub cwd: &'a Path",
            "pub tool_request: &'a tool_types::ToolRequest",
            "pub request: subagent::SubagentRequest",
            "pub subagent_depth: u32",
            "pub task_registry: &'a TaskRegistry",
            "struct AsyncSubagentWorkerSpawnContext<'a>",
            "config: &'a RunConfig",
            "child_cwd: &'a Path",
            "task_session_id: &'a str",
            "agent_id: &'a str",
            "request: &'a subagent::SubagentRequest",
            "worktree: Option<&'a AsyncSubagentWorktree>",
            "pub fn run_async_subagent_worker(input: AsyncSubagentWorkerInput) -> i32",
            "pub(crate) fn run_async_subagent_worker_with_executor(context: AsyncSubagentWorkerContext) -> i32",
            "context: AsyncSubagentLaunchContext<'_>",
            "context: AsyncSubagentWorkerSpawnContext<'_>",
            "pub fn run_async_subagent_worker(",
            "pub(crate) fn run_async_subagent_worker_with_executor(",
            "pub(crate) fn launch_async_subagent(",
            "fn spawn_async_subagent_worker(",
            "fn async_subagent_result_payload(",
            ".arg(\"subagent-worker\")",
            "TaskRegistry::new_for_cwd",
            "mark_worker_spawned",
            "complete_with_usage",
            "fail_with_usage",
        ] {
            assert!(
                async_worker_source.contains(marker),
                "subagent_async_worker must own async worker detail {marker}"
            );
        }
        assert!(
            !async_worker_source.contains("#[allow(clippy::too_many_arguments)]"),
            "async subagent worker APIs should use grouped context inputs instead of suppressing long-argument lints"
        );
    }

    #[test]
    fn tool_turn_dispatch_loop_is_owned_by_tool_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        for marker in [
            "let mut cursor = ToolRequestCursor::new",
            "while let Some(tool_request)",
            "collect_subagent_batch(",
            "collect_readonly_batch(",
            "run_normal_tool_turn(",
            "run_readonly_tool_turn(",
            "run_subagent_batch_tool_turn(",
            "reject_disallowed_child_tool(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own tool-turn dispatch loop detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("run_tool_turns("),
            "agent_loop must delegate tool-turn dispatch through turn loop"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate tool-turn dispatch through turn loop"
        );
        assert!(
            !tool_invocation_source.contains("fn run_tool_turns"),
            "tool_invocation must not own the tool-turn dispatch runner"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn run_tool_turns"),
            "tool_turn must expose the tool-turn dispatch runner"
        );
        assert!(
            tool_turn_source.contains("sampling_state.current_tool_request(tool_requests)"),
            "tool_turn must use sampling-state-owned dispatch cursor state"
        );
        assert!(
            tool_turn_source.contains("run_normal_tool_turn"),
            "tool_turn must compose normal tool turns"
        );
        assert!(
            tool_turn_source.contains("run_readonly_tool_turn"),
            "tool_turn must compose readonly tool turns"
        );
        assert!(
            tool_turn_source.contains("run_subagent_batch_tool_turn"),
            "tool_turn must compose subagent batch tool turns"
        );
    }

    #[test]
    fn agent_conversation_context_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        assert!(
            !agent_loop_source.contains("struct AgentConversationContext"),
            "agent_loop must not own agent conversation context"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> AgentConversationContext"),
            "agent_loop must not own agent conversation context behavior"
        );
        assert!(
            session_source.contains("struct AgentConversationContext"),
            "session must own agent conversation context"
        );
        assert!(
            session_source.contains("impl<'a> AgentConversationContext"),
            "session must own agent conversation context behavior"
        );
    }

    #[test]
    fn agent_tool_result_recording_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        assert!(
            !agent_loop_source.contains("format_tool_result_for_model"),
            "agent_loop must not own tool result model-content formatting"
        );
        assert!(
            !agent_loop_source.contains("append_tool_result_message"),
            "agent_loop must not own tool result history writing"
        );
        assert!(
            session_source.contains("pub(crate) fn record_tool_result_for_agent"),
            "session must expose agent tool result recording"
        );
        assert!(
            session_source.contains("format_tool_result_for_model"),
            "session must own tool result model-content formatting"
        );
        assert!(
            session_source.contains("append_tool_result_message"),
            "session must own tool result history writing"
        );
    }

    #[test]
    fn agent_plan_state_recording_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        for marker in [
            "orca_tools::update_plan::parse_args",
            "replace_plan_state",
            "append_plan_state",
            "format_context_message",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own plan-state recording detail {marker}"
            );
            assert!(
                session_source.contains(marker),
                "session must own plan-state recording detail {marker}"
            );
        }
        assert!(
            session_source.contains("pub(crate) fn record_plan_state_for_agent"),
            "session must expose agent plan-state recording"
        );
    }

    #[test]
    fn agent_assistant_response_recording_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        assert!(
            !agent_loop_source.contains("conversation.add_assistant"),
            "agent_loop must not own assistant response conversation recording"
        );
        assert!(
            session_source.contains("pub(crate) fn record_assistant_response_for_agent"),
            "session must expose agent assistant response recording"
        );
        assert!(
            session_source.contains("add_assistant"),
            "session must own assistant response conversation recording"
        );
        assert!(
            session_source.contains("append_message(message)"),
            "session must own assistant response history writing"
        );
    }

    #[test]
    fn final_memory_extraction_is_owned_by_memory_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let memory_source = include_str!("memory.rs");

        for marker in [
            "model::auxiliary_model",
            "memory::extract_project_memory(",
            "memory extraction failed:",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own final memory extraction detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("extract_project_memory_after_final_response("),
            "agent_loop must delegate final memory extraction through turn loop"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate final memory extraction through turn loop"
        );
        assert!(
            memory_source.contains("pub(crate) fn extract_project_memory_after_final_response"),
            "memory module must expose final memory extraction helper"
        );
        assert!(
            memory_source.contains("model::auxiliary_model"),
            "memory module must own auxiliary model selection for memory extraction"
        );
    }

    #[test]
    fn agent_initial_history_recording_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        for marker in ["writer.append_message", "append_summary_state"] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own initial history recording detail {marker}"
            );
            assert!(
                session_source.contains(marker),
                "session must own initial history recording detail {marker}"
            );
        }
        assert!(
            session_source.contains("pub(crate) fn record_initial_history_for_agent"),
            "session must expose initial history recording"
        );
    }

    #[test]
    fn runtime_conversation_bootstrap_step_is_owned_by_runtime_conversation_bootstrap_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_conversation_bootstrap_source =
            std::fs::read_to_string("src/runtime_conversation_bootstrap.rs")
                .expect("runtime conversation bootstrap source");
        let lib_source = include_str!("lib.rs");

        for marker in [
            "let mut owned_conversation",
            "bootstrap_agent_conversation_for_loop(",
            "record_initial_history_for_agent(",
            "resumed.is_some()",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime conversation bootstrap detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeConversationBootstrapStep"),
            "agent_loop must delegate runtime conversation bootstrap"
        );
        assert!(
            lib_source.contains("mod runtime_conversation_bootstrap;"),
            "runtime crate must declare a focused runtime_conversation_bootstrap module"
        );
        for marker in [
            "struct RuntimeConversationBootstrapStep",
            "impl RuntimeConversationBootstrapStep",
            "bootstrap_agent_conversation_for_loop(",
            "record_initial_history_for_agent(",
            "resumed.is_some()",
        ] {
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own runtime conversation bootstrap detail {marker}"
            );
            assert!(
                runtime_conversation_bootstrap_source.contains(marker),
                "runtime_conversation_bootstrap must own runtime conversation bootstrap detail {marker}"
            );
        }
    }

    #[test]
    fn agent_conversation_bootstrap_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        for marker in [
            "agent_common::build_agent_system_prompt",
            "thread_store::resume_conversation",
            "Conversation::new()",
            "add_system(system_prompt)",
            "replace_skill_context",
            "add_user(prompt.to_string())",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own conversation bootstrap detail {marker}"
            );
            assert!(
                session_source.contains(marker),
                "session must own conversation bootstrap detail {marker}"
            );
        }
        assert!(
            session_source.contains("pub(crate) fn bootstrap_agent_conversation"),
            "session must expose agent conversation bootstrap"
        );
        assert!(
            agent_loop_source.contains("RuntimeConversationBootstrapStep"),
            "agent_loop must delegate system prompt construction with conversation bootstrap"
        );
        assert!(
            session_source.contains("pub(crate) fn bootstrap_agent_conversation_for_loop"),
            "session must expose agent-loop conversation bootstrap"
        );
    }

    #[test]
    fn agent_provider_config_construction_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        assert!(
            !agent_loop_source.contains("ProviderConfig {"),
            "agent_loop must not own provider config construction"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnSetupStep"),
            "agent_loop must delegate runtime turn setup"
        );
        assert!(
            tool_invocation_source.contains("pub(crate) fn provider_config_for_agent_loop"),
            "tool_invocation must expose agent-loop provider config construction"
        );
        assert!(
            tool_invocation_source.contains("provider_tool_schema_override"),
            "tool_invocation must keep provider config close to tool schema selection"
        );
    }

    #[test]
    fn agent_tool_approval_policy_construction_is_owned_by_tool_execution_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_execution_source = include_str!("tool_execution.rs");

        assert!(
            !agent_loop_source.contains("ApprovalPolicy::new"),
            "agent_loop must not own tool approval policy construction"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnSetupStep"),
            "agent_loop must delegate runtime turn setup"
        );
        assert!(
            tool_execution_source.contains("pub(crate) fn policy_for_tool_execution"),
            "tool_execution must expose approval policy construction"
        );
        assert!(
            tool_execution_source.contains("with_permission_rules"),
            "tool_execution must preserve config permission rules in approval policy"
        );
    }

    #[test]
    fn tool_execution_approval_gate_uses_grouped_context() {
        let tool_execution_source = include_str!("tool_execution.rs");

        assert!(
            tool_execution_source.contains("struct ToolApprovalGateContext"),
            "tool_execution must group approval gate inputs into ToolApprovalGateContext"
        );
        assert!(
            tool_execution_source.contains("fn handle_approval<W: io::Write>(")
                && tool_execution_source.contains("context: ToolApprovalGateContext<"),
            "ToolExecutionActor::handle_approval must accept the grouped approval gate context"
        );
        assert!(
            tool_execution_source.contains("self.handle_approval(ToolApprovalGateContext"),
            "ToolExecutionActor::execute must pass approval gate inputs as one grouped context"
        );
        assert!(
            !tool_execution_source.contains("self.handle_approval(\n            config,"),
            "ToolExecutionActor::execute must not call handle_approval with the old long argument list"
        );
        for field_name in [
            "config:",
            "events:",
            "sink:",
            "tool_request:",
            "invocation:",
            "policy:",
            "permission_overlay:",
            "emit_deltas:",
        ] {
            assert!(
                tool_execution_source.contains(field_name),
                "ToolApprovalGateContext must carry approval input field {field_name}"
            );
        }
    }

    #[test]
    fn runtime_compaction_step_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let compaction_source = include_str!("compaction.rs");

        assert!(
            !agent_loop_source.contains("struct RuntimeCompactionStep"),
            "agent_loop must not own runtime compaction step state"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> RuntimeCompactionStep"),
            "agent_loop must not own runtime compaction step behavior"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeCompactionStep"),
            "lifecycle must not keep owning runtime compaction step state after extraction"
        );
        assert!(
            !lifecycle_source.contains("impl<'a, W: io::Write> RuntimeCompactionStep<'a, W>"),
            "lifecycle must not keep owning runtime compaction step behavior after extraction"
        );
        assert!(
            compaction_source.contains("struct RuntimeCompactionStep"),
            "compaction module must own runtime compaction step state"
        );
        assert!(
            compaction_source.contains("impl<'a, W: io::Write> RuntimeCompactionStep<'a, W>"),
            "compaction module must own runtime compaction step behavior"
        );
        assert!(
            compaction_source.contains("turn_context: RuntimeTurnContext<'a>"),
            "RuntimeCompactionStep must carry immutable turn refs through RuntimeTurnContext"
        );
        let compaction_step_struct = compaction_source
            .split("struct RuntimeCompactionStep")
            .nth(1)
            .and_then(|source| source.split("impl<'a, W: io::Write>").next())
            .expect("RuntimeCompactionStep source");
        for marker in ["cwd: &'a Path", "emit_deltas: bool"] {
            assert!(
                !compaction_step_struct.contains(marker),
                "RuntimeCompactionStep must not duplicate turn-entry field {marker}"
            );
        }

        for marker in [
            "HookEvent::OnBudgetWarning",
            "HookEvent::PreCompact",
            "HookEvent::PostCompact",
            "compact_with_summary(",
            "append_compaction(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime compaction detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_provider_turn_step_is_owned_by_provider_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let provider_turn_source = include_str!("provider_turn.rs");

        assert!(
            !agent_loop_source.contains("struct RuntimeProviderTurnStep"),
            "agent_loop must not own runtime provider turn step state"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> RuntimeProviderTurnStep"),
            "agent_loop must not own runtime provider turn step behavior"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeProviderTurnStep"),
            "lifecycle must not own runtime provider turn step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeProviderTurnStep"),
            "lifecycle must not own runtime provider turn step behavior"
        );
        assert!(
            provider_turn_source.contains("struct RuntimeProviderTurnStep"),
            "provider_turn must own runtime provider turn step state"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderTurnStep"),
            "provider_turn must own runtime provider turn step behavior"
        );

        for marker in [
            "assistant_reasoning_delta",
            "assistant_message_delta",
            "usage_updated",
            "provider_replay_updated",
            "ProviderStep::ReplayState",
            "ProviderStep::Error",
            "is_prompt_too_long_error",
            "compaction.emit_error(",
            "append_usage(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own provider turn detail {marker}"
            );
            assert!(
                provider_turn_source.contains(marker),
                "provider_turn must own provider turn detail {marker}"
            );
        }
    }

    #[test]
    fn thread_store_trait_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        assert!(
            !history_source.contains("pub trait ThreadStore"),
            "history must not own the storage-neutral ThreadStore trait"
        );
        assert!(
            thread_store_source.contains("pub trait ThreadStore"),
            "thread_store must own the storage-neutral ThreadStore trait"
        );
    }

    #[test]
    fn thread_store_uses_focused_submodules() {
        let thread_store_source = include_str!("thread_store.rs");
        let types_source = include_str!("thread_store/types.rs");
        let local_source = include_str!("thread_store/local.rs");
        let live_thread_source = include_str!("thread_store/live_thread.rs");
        let projection_source = include_str!("thread_store/projection.rs");
        let pagination_source = include_str!("thread_store/pagination.rs");
        let writer_source = include_str!("thread_store/writer.rs");

        assert!(
            thread_store_source.contains("mod types;")
                && thread_store_source.contains("mod local;")
                && thread_store_source.contains("mod live_thread;")
                && thread_store_source.contains("mod projection;")
                && thread_store_source.contains("mod pagination;")
                && thread_store_source.contains("mod writer;"),
            "thread_store.rs should be a facade over focused storage modules"
        );
        assert!(
            types_source.contains("pub trait ThreadStore"),
            "thread_store/types.rs must own storage-neutral types and trait"
        );
        assert!(
            local_source.contains("impl ThreadStore for JsonlThreadStore"),
            "thread_store/local.rs must own JSONL-backed ThreadStore behavior"
        );
        assert!(
            live_thread_source.contains("pub struct LiveThread"),
            "thread_store/live_thread.rs must own live thread handles"
        );
        assert!(
            projection_source.contains("messages_to_thread_turns"),
            "thread_store/projection.rs must own thread projection helpers"
        );
        assert!(
            pagination_source.contains("page_vec"),
            "thread_store/pagination.rs must own pagination helpers"
        );
        assert!(
            writer_source.contains("pub struct SessionWriter"),
            "thread_store/writer.rs must own JSONL writing"
        );
    }

    #[test]
    fn jsonl_thread_store_impl_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        assert!(
            !history_source.contains("impl ThreadStore for JsonlThreadStore"),
            "history must not own the JSONL ThreadStore trait implementation"
        );
        assert!(
            thread_store_source.contains("impl ThreadStore for JsonlThreadStore"),
            "thread_store must own the JSONL ThreadStore trait implementation"
        );
    }

    #[test]
    fn thread_projection_helpers_are_owned_by_focused_thread_store_modules() {
        let history_source = include_str!("history.rs");
        let facade_source = include_str!("thread_store.rs");
        let local_source = include_str!("thread_store/local.rs");
        let projection_source = include_str!("thread_store/projection.rs");
        let pagination_source = include_str!("thread_store/pagination.rs");

        for function_name in [
            "thread_summary_matches",
            "thread_summary_matches_filters",
            "sort_thread_summaries",
            "sort_thread_search_hits",
        ] {
            let signature = format!("fn {function_name}(");
            assert!(
                !history_source.contains(&signature) && !facade_source.contains(&signature),
                "history/facade must not own ThreadStore summary helper {function_name}"
            );
            assert!(
                local_source.contains(&signature),
                "thread_store/local.rs must own ThreadStore summary helper {function_name}"
            );
        }

        for function_name in [
            "message_to_thread_json",
            "stored_message_to_thread_json",
            "messages_to_thread_turns",
            "messages_to_thread_items",
            "stored_messages_to_thread_turns",
            "stored_messages_to_thread_items",
            "next_turn_id_for_messages",
        ] {
            let signature = format!("fn {function_name}(");
            assert!(
                !history_source.contains(&signature) && !facade_source.contains(&signature),
                "history/facade must not own ThreadStore projection helper {function_name}"
            );
            assert!(
                projection_source.contains(&signature),
                "thread_store/projection.rs must own ThreadStore projection helper {function_name}"
            );
        }

        for function_name in ["page_thread_turns", "page_thread_items", "page_vec"] {
            let plain_fn = format!("fn {function_name}(");
            let generic_fn = format!("fn {function_name}<");
            assert!(
                !history_source.contains(&plain_fn)
                    && !history_source.contains(&generic_fn)
                    && !facade_source.contains(&plain_fn)
                    && !facade_source.contains(&generic_fn),
                "history/facade must not own ThreadStore pagination helper {function_name}"
            );
            assert!(
                pagination_source.contains(&plain_fn) || pagination_source.contains(&generic_fn),
                "thread_store/pagination.rs must own ThreadStore pagination helper {function_name}"
            );
        }
    }

    #[test]
    fn tool_item_projection_helpers_are_owned_by_shared_projection_module() {
        let server_runtime_source = include_str!("server_runtime.rs");
        let thread_store_source = include_str!("thread_store.rs");
        let thread_projection_source = include_str!("thread_store/projection.rs");
        let projection_source = include_str!("tool_item_projection.rs");

        for function_name in [
            "mcp_tool_parts",
            "parse_json_or_null",
            "mcp_result_from_content",
            "mcp_tool_started_item",
            "dynamic_tool_started_item",
            "mcp_tool_completed_item",
            "dynamic_tool_completed_item",
            "file_change_started_item",
            "file_change_completed_item",
            "workflow_started_item",
            "workflow_completed_item",
            "persisted_command_execution_started_item",
            "persisted_command_execution_completed_item",
            "persisted_file_change_started_item",
            "persisted_file_change_completed_item",
            "complete_projected_tool_item",
            "tool_error_object_from_value",
            "tool_status_is_completed",
        ] {
            let signature = format!("fn {function_name}(");
            assert!(
                !server_runtime_source.contains(&signature),
                "server_runtime must not own shared tool item projection helper {function_name}"
            );
            assert!(
                !thread_store_source.contains(&signature)
                    && !thread_projection_source.contains(&signature),
                "thread_store must not own shared tool item projection helper {function_name}"
            );
            assert!(
                projection_source.contains(&signature),
                "tool_item_projection must own shared tool item projection helper {function_name}"
            );
        }

        for completed_constructor in [
            "mcp_tool_completed_item(",
            "dynamic_tool_completed_item(",
            "persisted_command_execution_completed_item(",
            "persisted_file_change_completed_item(",
        ] {
            assert!(
                !thread_projection_source.contains(completed_constructor),
                "thread_store/projection.rs must complete projected tool items through the shared projection helper, not call {completed_constructor} directly"
            );
        }
    }

    #[test]
    fn server_thread_runtime_owns_agent_state_through_runtime_thread() {
        let server_runtime_source = include_str!("server_runtime.rs");

        assert!(
            server_runtime_source.contains("thread: RuntimeThread"),
            "ServerThread must hold runtime-owned agent state through RuntimeThread"
        );
        for forbidden in [
            "InteractiveSession",
            "RuntimeSessionLifecycle",
            "ThreadTurnExecutor",
        ] {
            assert!(
                !server_runtime_source.contains(forbidden),
                "server_runtime must not directly own or assemble {forbidden}; use RuntimeThread"
            );
        }
    }

    #[test]
    fn headless_run_inner_enters_agent_loop_through_runtime_thread() {
        let controller_source = include_str!("controller.rs");
        let run_inner_source = controller_source
            .split("fn run_inner")
            .nth(1)
            .and_then(|source| source.split("pub fn run_thread_turn_to_writer").next())
            .expect("controller run_inner body");

        assert!(
            run_inner_source.contains("RuntimeThread::start"),
            "headless run_inner must create long-lived agent state through RuntimeThread"
        );
        assert!(
            run_inner_source.contains(".run_request_with_event_factory("),
            "headless run_inner must delegate turn execution through RuntimeThread"
        );
        for forbidden in [
            "RuntimeSessionLifecycle::new(new_run_id())",
            "TaskRegistry::new_for_cwd",
            "run_agent_loop(",
        ] {
            assert!(
                !run_inner_source.contains(forbidden),
                "headless run_inner must not directly assemble {forbidden}; use RuntimeThread"
            );
        }
    }

    #[test]
    fn jsonl_thread_store_type_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        assert!(
            !history_source.contains("pub struct JsonlThreadStore"),
            "history must not own the JSONL ThreadStore backend type"
        );
        assert!(
            !history_source.contains("pub type SessionStore"),
            "history must not own the SessionStore compatibility alias"
        );
        assert!(
            thread_store_source.contains("pub struct JsonlThreadStore"),
            "thread_store must own the JSONL ThreadStore backend type"
        );
        assert!(
            thread_store_source.contains("pub type SessionStore"),
            "thread_store must own the SessionStore compatibility alias"
        );
    }

    #[test]
    fn thread_store_api_types_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        for type_name in [
            "StoredThreadProjection",
            "ThreadListFilters",
            "SortDirection",
            "TurnItemsView",
        ] {
            assert!(
                !history_source.contains(&format!("pub struct {type_name}"))
                    && !history_source.contains(&format!("pub enum {type_name}")),
                "history must not own ThreadStore API type {type_name}"
            );
            assert!(
                thread_store_source.contains(&format!("pub struct {type_name}"))
                    || thread_store_source.contains(&format!("pub enum {type_name}")),
                "thread_store must own ThreadStore API type {type_name}"
            );
        }
    }

    #[test]
    fn live_thread_handle_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/live_thread.rs");

        assert!(
            !history_source.contains("pub struct LiveThread"),
            "history must not own the live ThreadStore handle"
        );
        assert!(
            !history_source.contains("impl LiveThread"),
            "history must not own live ThreadStore handle behavior"
        );
        assert!(
            thread_store_source.contains("pub struct LiveThread"),
            "thread_store must own the live ThreadStore handle"
        );
        assert!(
            thread_store_source.contains("impl LiveThread"),
            "thread_store must own live ThreadStore handle behavior"
        );
    }

    #[test]
    fn session_meta_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        assert!(
            !history_source.contains("pub struct SessionMeta"),
            "history must not own ThreadStore session metadata"
        );
        assert!(
            thread_store_source.contains("pub struct SessionMeta"),
            "thread_store must own ThreadStore session metadata"
        );
    }

    #[test]
    fn session_summary_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        assert!(
            !history_source.contains("pub struct SessionSummary"),
            "history must not own ThreadStore session summary"
        );
        assert!(
            thread_store_source.contains("pub struct SessionSummary"),
            "thread_store must own ThreadStore session summary"
        );
    }

    #[test]
    fn session_transcript_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        assert!(
            !history_source.contains("pub struct SessionTranscript"),
            "history must not own ThreadStore session transcript"
        );
        assert!(
            thread_store_source.contains("pub struct SessionTranscript"),
            "thread_store must own ThreadStore session transcript"
        );
    }

    #[test]
    fn session_writer_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/writer.rs");

        assert!(
            !history_source.contains("pub struct SessionWriter"),
            "history must not own ThreadStore session writer"
        );
        assert!(
            !history_source.contains("impl SessionWriter"),
            "history must not own ThreadStore session writer behavior"
        );
        assert!(
            thread_store_source.contains("pub struct SessionWriter"),
            "thread_store must own ThreadStore session writer"
        );
        assert!(
            thread_store_source.contains("impl SessionWriter"),
            "thread_store must own ThreadStore session writer behavior"
        );
    }

    #[test]
    fn jsonl_record_types_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        for type_name in ["SessionRecord", "StoredMessage"] {
            assert!(
                !history_source.contains(&format!("enum {type_name}")),
                "history must not own JSONL ThreadStore record type {type_name}"
            );
            assert!(
                thread_store_source.contains(&format!("enum {type_name}")),
                "thread_store must own JSONL ThreadStore record type {type_name}"
            );
        }
    }

    #[test]
    fn jsonl_append_writer_helpers_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/writer.rs");

        for function_name in [
            "write_record(",
            "write_record_line(",
            "redact_session_record(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own JSONL append helper {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own JSONL append helper {function_name}"
            );
        }
    }

    #[test]
    fn jsonl_read_rewrite_helpers_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/writer.rs");

        for function_name in ["read_records(", "rewrite_records(", "write_records_to("] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own JSONL read/rewrite helper {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own JSONL read/rewrite helper {function_name}"
            );
        }
    }

    #[test]
    fn session_read_models_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/writer.rs");

        for function_name in ["read_session_meta(", "read_transcript("] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore session reader {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore session reader {function_name}"
            );
        }
    }

    #[test]
    fn thread_record_lookup_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        for function_name in [
            "load_thread_records(",
            "find_session_path(",
            "collect_session_files(",
            "is_history_file(",
            "sessions_dir(",
            "archive_dir(",
            "orca_home(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore lookup helper {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore lookup helper {function_name}"
            );
        }
    }

    #[test]
    fn runtime_provider_response_step_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let provider_turn_source = include_str!("provider_turn.rs");

        for marker in [
            "record_assistant_response_for_agent(",
            "extract_project_memory_after_final_response(",
            "tool_requests_from_provider_steps(",
            "run_tool_turns(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own provider response handling detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate provider response handling through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderResponseResultStep"),
            "agent_loop must delegate provider response result folding through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderResponseOutcome::Continue"),
            "agent_loop must not own provider response continue outcome folding"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderResponseOutcome::Success"),
            "agent_loop must not own provider response success outcome folding"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderResponseOutcome::Return"),
            "agent_loop must not own provider response return outcome folding"
        );
        for marker in [
            "struct RuntimeProviderResponseStep",
            "struct RuntimeProviderResponseResultStep",
            "impl RuntimeProviderResponseStep",
            "impl RuntimeProviderResponseResultStep",
        ] {
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own provider response step detail {marker}"
            );
        }
        assert!(
            provider_turn_source.contains("struct RuntimeProviderResponseStep"),
            "provider_turn must own provider response step state"
        );
        assert!(
            provider_turn_source.contains("struct RuntimeProviderResponseResultStep"),
            "provider_turn must own provider response result folding step state"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderResponseStep"),
            "provider_turn must own provider response step behavior"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderResponseResultStep"),
            "provider_turn must own provider response result folding step behavior"
        );
        for marker in [
            "record_assistant_response_for_agent(",
            "extract_project_memory_after_final_response(",
            "tool_requests_from_provider_steps(",
            "run_tool_turns(",
        ] {
            assert!(
                provider_turn_source.contains(marker),
                "provider_turn must compose provider response handling detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_turn_setup_step_is_owned_by_runtime_turn_setup_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_setup_source = std::fs::read_to_string("src/runtime_turn_setup.rs")
            .expect("runtime turn setup source");
        let lib_source = include_str!("lib.rs");

        for marker in [
            "ContextConfig::for_model_with_runtime",
            "policy_for_tool_execution(",
            "provider_config_for_agent_loop(",
            "let budget_model = config.model.as_option()",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime turn setup detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnSetupStep"),
            "agent_loop must delegate runtime turn setup"
        );
        assert!(
            lib_source.contains("mod runtime_turn_setup;"),
            "runtime crate must declare a focused runtime_turn_setup module"
        );
        for marker in [
            "struct RuntimeTurnSetupStep",
            "impl RuntimeTurnSetupStep",
            "policy_for_tool_execution(",
            "provider_config_for_agent_loop(",
            "let budget_model = config.model.as_option()",
        ] {
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own runtime turn setup detail {marker}"
            );
            assert!(
                runtime_turn_setup_source.contains(marker),
                "runtime_turn_setup must own runtime turn setup detail {marker}"
            );
        }
    }

    #[test]
    fn agent_loop_result_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        for marker in [
            "struct AgentLoopResult",
            "impl AgentLoopResult",
            "status: RunStatus::Success",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own agent-loop result detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("AgentLoopResult"),
            "agent_loop must use the lifecycle-owned agent-loop result"
        );
        assert!(
            lifecycle_source.contains("struct AgentLoopResult"),
            "lifecycle must own agent-loop result shape"
        );
        assert!(
            lifecycle_source.contains("impl AgentLoopResult"),
            "lifecycle must own agent-loop result constructors"
        );
    }

    #[test]
    fn runtime_provider_turn_terminal_folding_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let provider_turn_source = include_str!("provider_turn.rs");

        for marker in [
            "provider_turn.response",
            "provider_turn.terminal_error",
            "provider_response_or_terminal(",
            "RunStatus::Cancelled",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own provider turn terminal folding detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate provider turn terminal folding through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderTurnResultResultStep"),
            "agent_loop must delegate provider turn result folding through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderTurnResultOutcome"),
            "agent_loop must not own provider turn result outcome folding"
        );
        for marker in [
            "struct RuntimeProviderTurnResultStep",
            "struct RuntimeProviderTurnResultResultStep",
            "impl RuntimeProviderTurnResultStep",
            "impl RuntimeProviderTurnResultResultStep",
            "pub(crate) fn provider_response_or_terminal",
        ] {
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own provider turn terminal folding detail {marker}"
            );
        }
        assert!(
            provider_turn_source.contains("struct RuntimeProviderTurnResultStep"),
            "provider_turn must own provider turn result step state"
        );
        assert!(
            provider_turn_source.contains("struct RuntimeProviderTurnResultResultStep"),
            "provider_turn must own provider turn result folding step state"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderTurnResultStep"),
            "provider_turn must own provider turn result step behavior"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderTurnResultResultStep"),
            "provider_turn must own provider turn result folding step behavior"
        );
        assert!(
            provider_turn_source.contains("pub(crate) fn provider_response_or_terminal"),
            "provider_turn must expose provider turn terminal folding"
        );
        assert!(
            provider_turn_source.contains("terminal_error"),
            "provider_turn must own provider turn terminal error extraction"
        );
    }

    #[test]
    fn runtime_turn_start_step_is_owned_by_runtime_turn_start_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lib_source = include_str!("lib.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_start_source = std::fs::read_to_string("src/runtime_turn_start.rs")
            .expect("runtime turn start source");

        for marker in [
            ".active_task()",
            "actor.start_turn(",
            "started_turn.into_event()",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime turn-start detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("AgentLoopResult::failure(error.status, error.message)"),
            "agent_loop must not own runtime turn-start error result folding"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime turn start through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeTurnStartResultStep"),
            "agent_loop must delegate runtime turn-start result folding through turn loop"
        );
        assert!(
            lib_source.contains("mod runtime_turn_start;"),
            "runtime crate must declare a focused runtime_turn_start module"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnStartStep"),
            "lifecycle must not own runtime turn-start step state"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnStartResultStep"),
            "lifecycle must not own runtime turn-start result step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnStartStep"),
            "lifecycle must not own runtime turn-start step behavior"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnStartResultStep"),
            "lifecycle must not own runtime turn-start result step behavior"
        );
        for marker in [
            "struct RuntimeTurnStartStep",
            "struct RuntimeTurnStartResultStep",
            "struct RuntimeTurnStartInput",
            "struct RuntimeTurnStartStepOutput",
            "enum RuntimeTurnStartResult",
            "impl RuntimeTurnStartStep",
            "impl RuntimeTurnStartResultStep",
            ".start_turn(",
            "started_turn.into_event()",
            "AgentLoopResult::failure(",
            "error.status",
            "error.message",
        ] {
            assert!(
                runtime_turn_start_source.contains(marker),
                "runtime_turn_start must own runtime turn-start detail {marker}"
            );
        }
        assert!(
            runtime_turn_start_source.contains("turn_context: RuntimeTurnContext<'a>"),
            "RuntimeTurnStartInput must carry immutable turn refs through RuntimeTurnContext"
        );
        let runtime_turn_start_input = runtime_turn_start_source
            .split("struct RuntimeTurnStartInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) struct RuntimeTurnStartStepOutput")
                    .next()
            })
            .expect("RuntimeTurnStartInput source");
        for marker in ["prompt: &'a str", "emit_deltas: bool"] {
            assert!(
                !runtime_turn_start_input.contains(marker),
                "RuntimeTurnStartInput must not duplicate turn-entry field {marker}"
            );
        }
    }

    #[test]
    fn runtime_model_route_step_is_owned_by_runtime_model_route_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lib_source = include_str!("lib.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_model_route_source = std::fs::read_to_string("src/runtime_model_route.rs")
            .expect("runtime model route source");

        for marker in [
            "actor.route_model_turn(",
            "events.model_routed(",
            "let turn_provider_config = routed_model.provider_config",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime model routing detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime model routing through turn loop"
        );
        assert!(
            lib_source.contains("mod runtime_model_route;"),
            "runtime crate must declare a focused runtime_model_route module"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeModelRouteStep"),
            "lifecycle must not own runtime model-route step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeModelRouteStep"),
            "lifecycle must not own runtime model-route step behavior"
        );
        for marker in [
            "struct RuntimeModelRouteStep",
            "struct RuntimeModelRouteInput",
            "impl RuntimeModelRouteStep",
            "actor.route_model_turn(",
            "events.model_routed(",
        ] {
            assert!(
                runtime_model_route_source.contains(marker),
                "runtime_model_route must own runtime model-route detail {marker}"
            );
        }
        assert!(
            runtime_model_route_source.contains("turn_context: RuntimeTurnContext<'a>"),
            "RuntimeModelRouteInput must carry immutable turn refs through RuntimeTurnContext"
        );
        let runtime_model_route_input = runtime_model_route_source
            .split("struct RuntimeModelRouteInput")
            .nth(1)
            .and_then(|source| source.split("impl RuntimeModelRouteStep").next())
            .expect("RuntimeModelRouteInput source");
        for marker in ["subagent_type: &'a SubagentType", "emit_deltas: bool"] {
            assert!(
                !runtime_model_route_input.contains(marker),
                "RuntimeModelRouteInput must not duplicate turn-entry field {marker}"
            );
        }
    }

    #[test]
    fn runtime_turn_opening_step_is_owned_by_runtime_turn_opening_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");
        let runtime_turn_opening_source = std::fs::read_to_string("src/runtime_turn_opening.rs")
            .expect("runtime turn opening source");

        for marker in [
            ".compact_if_needed(conversation)",
            "RuntimeTurnStartStep::new",
            "RuntimeTurnStartResultStep::new",
            "RuntimeModelRouteStep::new",
            "RuntimeSteerStep::new",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must delegate runtime turn opening detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime turn opening"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnOpeningStep"),
            "lifecycle must not own runtime turn opening step state"
        );
        assert!(
            !lifecycle_source.contains("enum RuntimeTurnOpeningResult"),
            "lifecycle must not own runtime turn opening result shape"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnOpeningInput"),
            "lifecycle must not own runtime turn opening input shape"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnOpeningStep"),
            "lifecycle must not own runtime turn opening step behavior"
        );
        assert!(
            runtime_turn_opening_source.contains("struct RuntimeTurnOpeningStep"),
            "runtime_turn_opening must own runtime turn opening step state"
        );
        assert!(
            runtime_turn_opening_source.contains("enum RuntimeTurnOpeningResult"),
            "runtime_turn_opening must own runtime turn opening result shape"
        );
        assert!(
            runtime_turn_opening_source.contains("struct RuntimeTurnOpeningInput"),
            "runtime_turn_opening must own grouped runtime turn opening input"
        );
        assert!(
            runtime_turn_opening_source.contains("turn_context: RuntimeTurnContext<'a>"),
            "RuntimeTurnOpeningInput must carry immutable turn refs through RuntimeTurnContext"
        );
        let runtime_turn_opening_input = runtime_turn_opening_source
            .split("struct RuntimeTurnOpeningInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) enum RuntimeTurnOpeningResult")
                    .next()
            })
            .expect("RuntimeTurnOpeningInput source");
        for marker in [
            "cwd: &'a Path",
            "emit_deltas: bool",
            "prompt: &'a str",
            "subagent_type: &'a SubagentType",
            "steer_handle: Option<&'a ThreadSteerHandle>",
        ] {
            assert!(
                !runtime_turn_opening_input.contains(marker),
                "RuntimeTurnOpeningInput must not duplicate turn-entry field {marker}"
            );
        }
        assert!(
            runtime_turn_opening_source.contains("impl RuntimeTurnOpeningStep"),
            "runtime_turn_opening must own runtime turn opening step behavior"
        );
        for marker in [
            "RuntimeCompactionStep::new",
            "RuntimeTurnStartStep::new",
            "RuntimeTurnStartResultStep::new",
            "RuntimeModelRouteStep::new",
            "RuntimeSteerStep::new",
        ] {
            assert!(
                runtime_turn_opening_source.contains(marker),
                "runtime_turn_opening must compose runtime turn opening detail {marker}"
            );
        }
        assert!(
            runtime_turn_iteration_source.contains("RuntimeTurnOpeningInput"),
            "runtime_turn_iteration must call runtime turn opening through its grouped input"
        );
    }

    #[test]
    fn runtime_provider_error_step_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let provider_turn_source = include_str!("provider_turn.rs");

        for marker in [
            "let mut reactive_compacted",
            "RuntimeProviderErrorOutcome::ContinueAfterCompaction",
            "RuntimeProviderErrorOutcome::Failed",
            "RuntimeProviderErrorOutcome::NoError",
            "handle_provider_error(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime provider-error detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("RuntimeProviderErrorStepOutcome"),
            "agent_loop must not own runtime provider-error outcome folding"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime provider-error handling through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderErrorResultStep"),
            "agent_loop must delegate runtime provider-error result folding through turn loop"
        );
        for marker in [
            "struct RuntimeProviderErrorStep",
            "struct RuntimeProviderErrorResultStep",
            "impl RuntimeProviderErrorStep",
            "impl RuntimeProviderErrorResultStep",
            "handle_provider_error(",
        ] {
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own runtime provider-error detail {marker}"
            );
        }
        assert!(
            provider_turn_source.contains("struct RuntimeProviderErrorStep"),
            "provider_turn must own runtime provider-error step state"
        );
        assert!(
            provider_turn_source.contains("struct RuntimeProviderErrorResultStep"),
            "provider_turn must own runtime provider-error result step state"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderErrorStep"),
            "provider_turn must own runtime provider-error step behavior"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderErrorResultStep"),
            "provider_turn must own runtime provider-error result step behavior"
        );
        assert!(
            provider_turn_source.contains("reactive_compacted"),
            "provider_turn must own reactive compaction loop state"
        );
        assert!(
            provider_turn_source.contains("handle_provider_error("),
            "provider_turn must keep provider error classification behind the step"
        );
    }

    #[test]
    fn runtime_turn_provider_cycle_step_is_composed_by_runtime_turn_iteration_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let provider_turn_source = include_str!("provider_turn.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");

        for marker in [
            "RuntimeProviderTurnStep::new",
            "RuntimeProviderTurnResultStep::new",
            "RuntimeProviderTurnResultResultStep::new",
            "RuntimeProviderErrorStep::new",
            "RuntimeProviderErrorResultStep::new",
            "RuntimeProviderResponseStep::new",
            "RuntimeProviderResponseResultStep::new",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must delegate runtime provider cycle detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime provider cycle"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnProviderCycleStep"),
            "lifecycle must not own runtime provider cycle step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnProviderCycleStep"),
            "lifecycle must not own runtime provider cycle step behavior"
        );
        assert!(
            !lifecycle_source.contains("RuntimeTurnProviderCycleStep::new"),
            "lifecycle must not compose runtime provider cycle after the turn-iteration split"
        );
        assert!(
            runtime_turn_iteration_source.contains("RuntimeTurnProviderCycleStep::new"),
            "runtime_turn_iteration must compose runtime provider cycle through provider_turn boundary"
        );
        assert!(
            provider_turn_source.contains("struct RuntimeTurnProviderCycleStep"),
            "provider_turn must own runtime provider cycle step state"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeTurnProviderCycleStep"),
            "provider_turn must own runtime provider cycle step behavior"
        );
        for marker in [
            "RuntimeProviderTurnStep::new",
            "RuntimeProviderTurnResultStep::new",
            "RuntimeProviderTurnResultResultStep::new",
            "RuntimeProviderErrorStep::new",
            "RuntimeProviderErrorResultStep::new",
            "RuntimeProviderResponseStep::new",
            "RuntimeProviderResponseResultStep::new",
        ] {
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own runtime provider cycle detail {marker}"
            );
            assert!(
                provider_turn_source.contains(marker),
                "provider_turn must compose runtime provider cycle detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_turn_iteration_step_is_owned_by_runtime_turn_iteration_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_iteration_source =
            std::fs::read_to_string("src/runtime_turn_iteration.rs")
                .expect("runtime turn iteration source");

        for marker in [
            "RuntimeTurnOpeningStep::new",
            "RuntimeTurnProviderCycleStep::new",
            "RuntimeTurnOpeningResult::Continue",
            "RuntimeTurnProviderCycleResult::ContinueLoop",
            "RuntimeTurnProviderCycleResult::ContinueTurn",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must delegate runtime turn iteration detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime turn loop"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnIterationStep"),
            "lifecycle must not own runtime turn iteration step state"
        );
        assert!(
            !lifecycle_source.contains("enum RuntimeTurnIterationResult"),
            "lifecycle must not own runtime turn iteration result shape"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnIterationStep"),
            "lifecycle must not own runtime turn iteration step behavior"
        );
        assert!(
            runtime_turn_iteration_source.contains("struct RuntimeTurnIterationStep"),
            "runtime_turn_iteration must own runtime turn iteration step state"
        );
        assert!(
            runtime_turn_iteration_source.contains("enum RuntimeTurnIterationResult"),
            "runtime_turn_iteration must own runtime turn iteration result shape"
        );
        assert!(
            runtime_turn_iteration_source.contains("impl RuntimeTurnIterationStep"),
            "runtime_turn_iteration must own runtime turn iteration step behavior"
        );
        for marker in [
            "RuntimeTurnOpeningStep::new",
            "RuntimeTurnProviderCycleStep::new",
            "RuntimeTurnOpeningResult::Continue",
            "RuntimeTurnProviderCycleResult::ContinueLoop",
            "RuntimeTurnProviderCycleResult::ContinueTurn",
        ] {
            assert!(
                runtime_turn_iteration_source.contains(marker),
                "runtime_turn_iteration must compose runtime turn iteration detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_turn_loop_step_is_owned_by_runtime_turn_loop_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");

        for marker in [
            "loop {",
            "RuntimeTurnIterationStep::new",
            "RuntimeTurnIterationResult::ContinueLoop",
            "RuntimeTurnIterationResult::Return",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must delegate runtime turn loop detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime turn loop"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnLoopStep"),
            "lifecycle must not own runtime turn loop step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnLoopStep"),
            "lifecycle must not own runtime turn loop step behavior"
        );
        assert!(
            runtime_turn_loop_source.contains("struct RuntimeTurnLoopStep"),
            "runtime_turn_loop must own runtime turn loop step state"
        );
        assert!(
            runtime_turn_loop_source.contains("impl RuntimeTurnLoopStep"),
            "runtime_turn_loop must own runtime turn loop step behavior"
        );
        for marker in [
            "RuntimeTurnIterationStep::new",
            "RuntimeTurnIterationResult::ContinueLoop",
            "RuntimeTurnIterationResult::Return",
        ] {
            assert!(
                runtime_turn_loop_source.contains(marker),
                "runtime_turn_loop must compose runtime turn loop detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_turn_loop_input_is_owned_by_runtime_turn_loop_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let agent_loop_runtime_source = agent_loop_source
            .split("#[cfg(test)]")
            .next()
            .expect("agent loop runtime source");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");

        assert!(
            agent_loop_source.contains("RuntimeAgentTurnLoopInput"),
            "agent_loop must pass turn loop inputs through a focused turn-loop entry object"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopExecutors"),
            "agent_loop must pass child executors through a lifecycle-owned executor object"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnLoopInput"),
            "lifecycle must not own runtime turn loop input shape"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnLoopExecutors"),
            "lifecycle must not own runtime turn loop executor shape"
        );
        assert!(
            !lifecycle_source.contains("impl<'a, 'runtime, W: io::Write> RuntimeTurnLoopInput"),
            "lifecycle must not own runtime turn loop input behavior"
        );
        assert!(
            !lifecycle_source.contains("impl<W: io::Write> RuntimeTurnLoopExecutors<W>"),
            "lifecycle must not own runtime turn loop executor behavior"
        );
        assert!(
            runtime_turn_loop_source.contains("struct RuntimeTurnLoopInput"),
            "runtime_turn_loop must own runtime turn loop input shape"
        );
        assert!(
            runtime_turn_loop_source.contains("struct RuntimeTurnLoopExecutors"),
            "runtime_turn_loop must own runtime turn loop executor shape"
        );
        assert!(
            runtime_turn_loop_source
                .contains("impl<'a, 'runtime, W: io::Write> RuntimeTurnLoopInput"),
            "runtime_turn_loop must own runtime turn loop input behavior"
        );
        assert!(
            runtime_turn_loop_source.contains("impl<W: io::Write> RuntimeTurnLoopExecutors<W>"),
            "runtime_turn_loop must own runtime turn loop executor behavior"
        );
        assert!(
            !agent_loop_runtime_source.contains("RuntimeTurnLoopInput::new("),
            "agent_loop must not construct the wide turn-loop input directly"
        );
        assert!(
            runtime_turn_loop_source.contains("pub(crate) fn run_agent_turn_loop"),
            "runtime_turn_loop must expose a focused entrypoint that owns turn-loop input construction"
        );
        assert!(
            !agent_loop_source.contains("execute_child_agent_loop,\n        execute_child_agent_loop,\n        execute_child_agent_loop"),
            "agent_loop must not pass child executors as a raw repeated argument list"
        );
    }

    #[test]
    fn runtime_turn_workflow_refs_are_grouped_for_turn_loop() {
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");
        let turn_loop_input = runtime_turn_loop_source
            .split("pub(crate) struct RuntimeTurnLoopInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) struct RuntimeTurnLoopExecutors")
                    .next()
            })
            .expect("RuntimeTurnLoopInput source");
        let turn_iteration_input = runtime_turn_iteration_source
            .split("pub(crate) struct RuntimeTurnIterationInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) enum RuntimeTurnIterationResult")
                    .next()
            })
            .expect("RuntimeTurnIterationInput source");

        assert!(
            runtime_turn_loop_source.contains("pub(crate) struct RuntimeTurnWorkflowContext"),
            "runtime_turn_loop must own a named workflow context for turn-loop workflow refs"
        );
        for source in [turn_loop_input, turn_iteration_input] {
            assert!(
                source.contains("workflow: RuntimeTurnWorkflowContext"),
                "turn-loop inputs must pass workflow refs through RuntimeTurnWorkflowContext"
            );
            assert!(
                !source.contains("background_workflows: &'a mut Vec<BackgroundWorkflowRun>")
                    && !source.contains("workflow_ipc: Option<&'a WorkflowIpcContext>"),
                "turn-loop inputs must not expose background workflow and workflow IPC as parallel fields"
            );
        }
    }

    #[test]
    fn runtime_turn_output_refs_are_grouped_for_turn_loop() {
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");
        let turn_loop_input = runtime_turn_loop_source
            .split("pub(crate) struct RuntimeTurnLoopInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) struct RuntimeTurnLoopExecutors")
                    .next()
            })
            .expect("RuntimeTurnLoopInput source");
        let turn_iteration_input = runtime_turn_iteration_source
            .split("pub(crate) struct RuntimeTurnIterationInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) enum RuntimeTurnIterationResult")
                    .next()
            })
            .expect("RuntimeTurnIterationInput source");

        assert!(
            runtime_turn_loop_source.contains("pub(crate) struct RuntimeTurnOutputContext"),
            "runtime_turn_loop must own a named output context for event emission refs"
        );
        for source in [turn_loop_input, turn_iteration_input] {
            assert!(
                source.contains("output: RuntimeTurnOutputContext"),
                "turn-loop inputs must pass event output refs through RuntimeTurnOutputContext"
            );
            assert!(
                !source.contains("events: &'a mut EventFactory")
                    && !source.contains("sink: &'a mut EventSink<W>"),
                "turn-loop inputs must not expose events and sink as parallel fields"
            );
        }
    }

    #[test]
    fn runtime_turn_provider_refs_are_grouped_for_turn_loop() {
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");
        let turn_loop_input = runtime_turn_loop_source
            .split("pub(crate) struct RuntimeTurnLoopInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) struct RuntimeTurnLoopExecutors")
                    .next()
            })
            .expect("RuntimeTurnLoopInput source");
        let turn_iteration_input = runtime_turn_iteration_source
            .split("pub(crate) struct RuntimeTurnIterationInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) enum RuntimeTurnIterationResult")
                    .next()
            })
            .expect("RuntimeTurnIterationInput source");

        assert!(
            runtime_turn_loop_source.contains("pub(crate) struct RuntimeTurnProviderContext"),
            "runtime_turn_loop must own a named provider context for turn-loop model refs"
        );
        for source in [turn_loop_input, turn_iteration_input] {
            assert!(
                source.contains("provider_context: RuntimeTurnProviderContext"),
                "turn-loop inputs must pass model provider refs through RuntimeTurnProviderContext"
            );
            for field_name in [
                "provider: ProviderKind",
                "context_config: &'a context::ContextConfig",
                "provider_config: &'a ProviderConfig",
                "model: &'a ModelSelection",
                "max_budget_usd: Option<f64>",
            ] {
                assert!(
                    !source.contains(field_name),
                    "turn-loop inputs must not expose provider field {field_name} outside RuntimeTurnProviderContext"
                );
            }
        }
    }

    #[test]
    fn runtime_turn_request_refs_are_grouped_for_turn_loop() {
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");
        let turn_loop_input = runtime_turn_loop_source
            .split("pub(crate) struct RuntimeTurnLoopInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) struct RuntimeTurnLoopExecutors")
                    .next()
            })
            .expect("RuntimeTurnLoopInput source");
        let turn_iteration_input = runtime_turn_iteration_source
            .split("pub(crate) struct RuntimeTurnIterationInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) enum RuntimeTurnIterationResult")
                    .next()
            })
            .expect("RuntimeTurnIterationInput source");

        assert!(
            runtime_turn_loop_source.contains("pub(crate) struct RuntimeTurnRequestContext"),
            "runtime_turn_loop must own a named request context for immutable turn inputs"
        );
        let request_context = runtime_turn_loop_source
            .split("pub(crate) struct RuntimeTurnRequestContext")
            .nth(1)
            .and_then(|source| source.split("#[derive(Clone, Copy)]").next())
            .expect("RuntimeTurnRequestContext source");
        assert!(
            request_context.contains("turn_context: RuntimeTurnContext"),
            "RuntimeTurnRequestContext must wrap the lifecycle-owned immutable turn context"
        );
        for field_name in [
            "cwd: &'a Path",
            "emit_deltas: bool",
            "prompt: &'a str",
            "subagent_type: &'a SubagentType",
            "continuation: Option<RuntimeTurnContinuation>",
            "steer_handle: Option<&'a ThreadSteerHandle>",
            "subagent_depth: u32",
        ] {
            assert!(
                !request_context.contains(field_name),
                "RuntimeTurnRequestContext must not duplicate turn context field {field_name}"
            );
        }
        for source in [turn_loop_input, turn_iteration_input] {
            assert!(
                source.contains("request: RuntimeTurnRequestContext"),
                "turn-loop inputs must pass immutable turn inputs through RuntimeTurnRequestContext"
            );
            for field_name in [
                "cwd: &'a Path",
                "emit_deltas: bool",
                "prompt: &'a str",
                "subagent_type: &'a SubagentType",
                "continuation: Option<RuntimeTurnContinuation>",
                "steer_handle: Option<&'a ThreadSteerHandle>",
                "subagent_depth: u32",
            ] {
                assert!(
                    !source.contains(field_name),
                    "turn-loop inputs must not expose request field {field_name} outside RuntimeTurnRequestContext"
                );
            }
        }
    }

    #[test]
    fn runtime_agent_turn_loop_input_uses_grouped_turn_contexts() {
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");
        let agent_turn_loop_input = runtime_turn_loop_source
            .split("pub(crate) struct RuntimeAgentTurnLoopInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) struct RuntimeTurnLoopInput")
                    .next()
            })
            .expect("RuntimeAgentTurnLoopInput source");

        assert!(
            agent_turn_loop_input.contains("provider_context: RuntimeTurnProviderContext"),
            "agent turn-loop input must enter the loop with grouped provider refs"
        );
        assert!(
            agent_turn_loop_input.contains("request: RuntimeTurnRequestContext"),
            "agent turn-loop input must enter the loop with grouped immutable turn inputs"
        );
        for field_name in [
            "context_config: &'a context::ContextConfig",
            "provider_config: &'a ProviderConfig",
            "cwd: &'a Path",
            "emit_deltas: bool",
            "prompt: &'a str",
            "subagent_type: &'a SubagentType",
            "continuation: Option<RuntimeTurnContinuation>",
            "steer_handle: Option<&'a ThreadSteerHandle>",
            "subagent_depth: u32",
        ] {
            assert!(
                !agent_turn_loop_input.contains(field_name),
                "agent turn-loop input must not expose field {field_name} outside grouped turn contexts"
            );
        }
    }

    #[test]
    fn runtime_turn_loop_inputs_use_runtime_turn_deps() {
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");
        let agent_turn_loop_input = runtime_turn_loop_source
            .split("pub(crate) struct RuntimeAgentTurnLoopInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) struct RuntimeTurnLoopInput")
                    .next()
            })
            .expect("RuntimeAgentTurnLoopInput source");
        let turn_loop_input = runtime_turn_loop_source
            .split("pub(crate) struct RuntimeTurnLoopInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) struct RuntimeTurnLoopExecutors")
                    .next()
            })
            .expect("RuntimeTurnLoopInput source");
        let turn_iteration_input = runtime_turn_iteration_source
            .split("pub(crate) struct RuntimeTurnIterationInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) enum RuntimeTurnIterationResult")
                    .next()
            })
            .expect("RuntimeTurnIterationInput source");

        assert!(
            runtime_turn_loop_source.contains("RuntimeTurnDeps"),
            "runtime_turn_loop must route turn-scoped services through RuntimeTurnDeps"
        );
        for source in [agent_turn_loop_input, turn_loop_input, turn_iteration_input] {
            assert!(
                source.contains("deps: RuntimeTurnDeps"),
                "turn-loop inputs must pass injected turn services through RuntimeTurnDeps"
            );
            for field_name in [
                "hooks: &'a HookRunner",
                "instructions: &'a ProjectInstructions",
                "memory: &'a MemoryBlock",
                "mcp_registry: &'a McpRegistry",
                "turn_interactions: RuntimeTurnInteractionState",
            ] {
                assert!(
                    !source.contains(field_name),
                    "turn-loop inputs must not expose dependency field {field_name} outside RuntimeTurnDeps"
                );
            }
        }
    }

    #[test]
    fn runtime_turn_loop_inputs_use_runtime_turn_policy_context() {
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");
        let agent_turn_loop_input = runtime_turn_loop_source
            .split("pub(crate) struct RuntimeAgentTurnLoopInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) struct RuntimeTurnLoopInput")
                    .next()
            })
            .expect("RuntimeAgentTurnLoopInput source");
        let turn_loop_input = runtime_turn_loop_source
            .split("pub(crate) struct RuntimeTurnLoopInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) struct RuntimeTurnLoopExecutors")
                    .next()
            })
            .expect("RuntimeTurnLoopInput source");
        let turn_iteration_input = runtime_turn_iteration_source
            .split("pub(crate) struct RuntimeTurnIterationInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) enum RuntimeTurnIterationResult")
                    .next()
            })
            .expect("RuntimeTurnIterationInput source");

        assert!(
            runtime_turn_loop_source.contains("pub(crate) struct RuntimeTurnPolicyContext"),
            "runtime_turn_loop must define an explicit policy context for turn-loop stages"
        );
        for source in [agent_turn_loop_input, turn_loop_input, turn_iteration_input] {
            assert!(
                source.contains("policy: RuntimeTurnPolicyContext"),
                "turn-loop inputs must group config and approval policy refs through RuntimeTurnPolicyContext"
            );
            for field_name in [
                "config: &'a RunConfig",
                "tool_policy: AgentToolPolicyContext",
                "policy: &'a ApprovalPolicy",
            ] {
                assert!(
                    !source.contains(field_name),
                    "turn-loop inputs must not expose policy field {field_name} outside RuntimeTurnPolicyContext"
                );
            }
        }
    }

    #[test]
    fn runtime_turn_iteration_input_uses_loop_iteration_state() {
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");
        let turn_iteration_input = runtime_turn_iteration_source
            .split("pub(crate) struct RuntimeTurnIterationInput")
            .nth(1)
            .and_then(|source| {
                source
                    .split("pub(crate) enum RuntimeTurnIterationResult")
                    .next()
            })
            .expect("RuntimeTurnIterationInput source");

        assert!(
            turn_iteration_input.contains("loop_state: RuntimeTurnLoopIterationState"),
            "turn iteration input must keep lifecycle-owned iteration state grouped"
        );
        for field_name in [
            "runtime_system_messages: &'a [String]",
            "model_override: Option<&'a str>",
            "cost_tracker: &'a mut CostTracker",
            "cancel: &'a CancelToken",
            "task_registry: &'a TaskRegistry",
            "extensions: RuntimeExtensionContext",
        ] {
            assert!(
                !turn_iteration_input.contains(field_name),
                "turn iteration input must not expose lifecycle iteration field {field_name}"
            );
        }
    }

    #[test]
    fn runtime_turn_loop_state_resolves_runtime_directive_policy() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let agent_loop_runtime_source = agent_loop_source
            .split("#[cfg(test)]")
            .next()
            .expect("agent loop runtime source");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");

        assert!(
            lifecycle_source.contains("struct RuntimeTurnLoopIterationState"),
            "lifecycle must own the directive-resolved turn loop iteration state"
        );
        assert!(
            lifecycle_source.contains("fn iteration_state"),
            "RuntimeTurnLoopState must expose directive-resolved iteration state"
        );
        assert!(
            lifecycle_source.contains("replace_allowed_tools(")
                && lifecycle_source.contains("pending_system_messages()")
                && lifecycle_source.contains("model_override()"),
            "RuntimeTurnLoopState must resolve directive tool policy, system messages, and model override"
        );
        assert!(
            runtime_turn_loop_source.contains("loop_state: RuntimeTurnLoopState"),
            "runtime_turn_loop must consume the lifecycle-owned loop state"
        );
        assert!(
            runtime_turn_loop_source.contains(".iteration_state(self.policy.tool_policy)"),
            "runtime_turn_loop must request directive-resolved iteration state at the iteration boundary"
        );
        assert!(
            !runtime_turn_loop_source.contains("runtime: RuntimeTurnLoopRuntime"),
            "runtime_turn_loop must not receive raw loop runtime without its directive policy state"
        );
        assert!(
            !agent_loop_runtime_source.contains("RuntimeTurnLoopState {"),
            "agent_loop must not destructure runtime turn loop state"
        );
        assert!(
            !agent_loop_runtime_source.contains("tool_policy_for_directive_state"),
            "agent_loop must not resolve runtime directive tool policy directly"
        );
        assert!(
            !agent_loop_runtime_source.contains("directive_state."),
            "agent_loop must not directly read runtime directive policy accessors"
        );
    }

    #[test]
    fn session_list_load_operations_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        for function_name in [
            "list_sessions(",
            "list_sessions_with_archived(",
            "load_session(",
            "summarize_session_with_archive_flag(",
            "collect_summaries_from_root(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore read operation {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore read operation {function_name}"
            );
        }
    }

    #[test]
    fn session_search_operations_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        assert!(
            !history_source.contains("pub struct SearchHit"),
            "history must not own ThreadStore search result type"
        );
        assert!(
            thread_store_source.contains("pub struct SearchHit"),
            "thread_store must own ThreadStore search result type"
        );

        for function_name in [
            "search_sessions(",
            "search_roots_with_ripgrep(",
            "search_root_in_process(",
            "search_compressed_root(",
            "push_matching_lines(",
            "push_search_hit(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore search operation {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore search operation {function_name}"
            );
        }
    }

    #[test]
    fn session_mutation_operations_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        for function_name in [
            "delete_session(",
            "archive_session(",
            "rename_session(",
            "compress_session(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore mutation operation {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore mutation operation {function_name}"
            );
        }
    }

    #[test]
    fn protocol_imports_thread_types_from_thread_store_boundary() {
        let protocol_source = include_str!("protocol.rs");

        assert!(
            !protocol_source.contains("use crate::history"),
            "protocol must import thread protocol types through thread_store"
        );
    }

    #[test]
    fn agent_loop_imports_session_types_from_thread_store_boundary() {
        let agent_loop_source = include_str!("agent_loop.rs");

        assert!(
            !agent_loop_source.contains("use crate::history"),
            "agent loop must import session transcript/writer types through thread_store"
        );
    }

    #[test]
    fn session_imports_session_types_from_thread_store_boundary() {
        let session_source = include_str!("session.rs");

        assert!(
            !session_source.contains("use crate::history::{self, SessionWriter};"),
            "session production code must import session transcript/writer types through thread_store"
        );
    }
}

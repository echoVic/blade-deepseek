pub mod acp;
pub mod agent_child;
pub mod agent_common;
pub mod agent_loop;
pub mod approval_resolution;
pub mod background_turn;
mod budget_soft_landing;
mod child_agent_entrypoints;
mod child_agent_loop_runner;
mod child_agent_loop_setup;
mod child_agent_provider_turn;
mod child_agent_response_folding;
#[cfg(test)]
mod child_agent_tests;
mod child_agent_types;
pub mod command;
pub mod compaction;
pub use compaction::{
    TuiAgentProviderErrorAction, TuiAgentTurnCompactionInput, TuiAgentTurnCompactionOutcome,
    TuiAgentTurnCompactionState, handle_tui_agent_provider_error, run_tui_agent_turn_compaction,
};
pub mod controller;
pub mod cost;
pub mod extension;
pub mod goal_actor;
pub mod goal_store;
pub mod goal_tracker;
pub mod goal_verifier;
pub mod history;
pub mod hooks;
pub mod instructions;
pub mod lifecycle;
pub mod memory;
pub mod mentions;
pub mod model_response;
pub mod network_proxy;
pub mod notify;
pub mod protocol;
pub mod provider_stream;
pub mod provider_turn;
pub(crate) mod runtime_approval;
pub(crate) mod runtime_bash;
pub mod runtime_capability;
mod runtime_conversation_bootstrap;
pub mod runtime_directive;
pub(crate) mod runtime_event_projector;
pub mod runtime_host;
mod runtime_lifecycle;
mod runtime_model_route;
mod runtime_normal_tool;
pub mod runtime_pending_interaction;
pub mod runtime_permission;
pub(crate) mod runtime_readonly_tool_turn;
pub(crate) mod runtime_special;
pub mod runtime_state;
mod runtime_steer;
mod runtime_subagent_call;
mod runtime_surface;

/// Curated public access to runtime-owned surface types.
///
/// The implementation namespace is intentionally private:
///
/// ```compile_fail
/// use orca_runtime::runtime_surface::SurfaceCursor;
/// ```
pub mod surface {
    pub use crate::runtime_surface::{
        AcpRequestId, AdmissionOutput, AssistantChannel, AssistantPatch, AttachResult,
        BackgroundTarget, ByteCount, ByteOffset, CancelOperationOutput, CanonicalMime,
        CanonicalPath, CanonicalUri, CommitClass, CompactionReason, CompactionState,
        ContextRevision, CursorSourceRevision, DeferredCommandValue, DetachRequest, DetachResult,
        DisplayText, DurableRevision, ExpectedGoal, FailureClass, FreshAttachRequest,
        FreshSurfaceAttachment, GoalMutationAction, GoalMutationOutput, GoalPatch, GoalRunInput,
        GoalTokenBudgetUpdate, InteractionPatch, LegacyTurnId, MaintenanceOperationOutput,
        MutationDisposition, MutationReply, NonEmptyText, NonEmptyVec, NotAdmittedReason,
        OperationBudget, OperationIngressCorrelation, OperationKind, OperationPatch,
        OperationRequestIntent, OperationSettingsPreparation, OperationTerminal,
        PermissionGrantScope, PinnedContextAction, PinnedContextMutationOutput, PinnedContextPatch,
        PinnedContextRevision, PinnedContextSourceRevision, PinnedUserRevision,
        ProviderReplayHealth, ReplayabilityRequest, RuntimeSettingsPatch,
        RuntimeSurfaceClientHandle, RuntimeSurfaceHandle, RuntimeSurfaceHostHandle,
        RuntimeSurfaceThreadHandle, SequenceNumber, SessionMetadataPatch,
        SessionMetadataPrecondition, Set, Sha256Digest, SnapshotAtCursor, SurfaceAllowDeny,
        SurfaceApprovalMode, SurfaceAssistantStream, SurfaceAssistantStreamState,
        SurfaceAttachmentId, SurfaceAttachmentRole, SurfaceCapability, SurfaceCatalogEntryId,
        SurfaceClientCommandError, SurfaceClientInteractionAnswer, SurfaceCommitBatch,
        SurfaceCommitId, SurfaceCompletedModelResponse, SurfaceConnectionId,
        SurfaceContextSnapshot, SurfaceCursor, SurfaceDataProperty, SurfaceDataValue, SurfaceEvent,
        SurfaceEventEnvelope, SurfaceEventId, SurfaceFileChange, SurfaceGoal, SurfaceGoalFence,
        SurfaceGoalPauseReason, SurfaceGoalReceiptState, SurfaceGoalState,
        SurfaceHistoryAssistantRole, SurfaceHistoryId, SurfaceHistoryMessage,
        SurfaceHistorySystemRole, SurfaceHistoryToolRole, SurfaceHistoryUserRole,
        SurfaceIncarnation, SurfaceInput, SurfaceInputBlock, SurfaceInputCorrelationId,
        SurfaceInputPresentation, SurfaceInputRequest, SurfaceInputRequestBlock,
        SurfaceInteractionId, SurfaceInteractionKind, SurfaceInteractionLifecycle,
        SurfaceInteractionRequest, SurfaceInteractionRoute, SurfaceInteractionView, SurfaceItem,
        SurfaceItemId, SurfaceItemOrigin, SurfaceMcpElicitationDecision,
        SurfaceMcpElicitationRequest, SurfaceOperationFence, SurfaceOperationId,
        SurfacePermissionClientDecision, SurfacePermissionProfile, SurfacePinnedContextEntry,
        SurfacePinnedContextKind, SurfacePlanStatus, SurfaceReasoningEffort,
        SurfaceRecoverableOperation, SurfaceReduceMode, SurfaceReduceResult,
        SurfaceReducerErrorCode, SurfaceReducerState, SurfaceRequestId, SurfaceResolvedInputFact,
        SurfaceScope, SurfaceSettingsSnapshot, SurfaceShutdownReason, SurfaceSnapshot,
        SurfaceStreamId, SurfaceSubscriptionHandle, SurfaceSubscriptionItem,
        SurfaceSubscriptionReceiver, SurfaceTask, SurfaceTaskFence, SurfaceTaskId,
        SurfaceTaskStatus, SurfaceTaskType, SurfaceThreadId, SurfaceToolAction, SurfaceToolRequest,
        SurfaceToolResultKind, SurfaceTurnId, SurfaceUnavailableReason, SurfaceUserInputDecision,
        SurfaceUserInputState, SurfaceWorkflow, SurfaceWorkflowAgentStatus, SurfaceWorkflowRunId,
        SurfaceWorkflowStatus, TaskControlAction, TaskControlOutput, TaskPatch, ThreadOwnerEpoch,
        ToolPatch, TransferBackgroundOutput, TurnRequestBudgetScope, UncommittedMutation,
        UnixMillis, UsageTotals, WaitOperationTerminalResult, WorkflowCatalogRevision,
        WorkflowControlAction, WorkflowControlOutput, WorkflowPatch, reduce_batch,
    };
}
mod runtime_tool_actor;
mod runtime_tool_call;
pub(crate) mod runtime_tool_scheduler;
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
pub mod session;
pub mod shell_session;
mod step_context;
pub mod subagent;
pub mod subagent_async_worker;
pub mod subagent_execution;
pub mod task_output;
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

pub mod unstable_surface {
    //! Compatibility access to runtime-surface types that are not in the curated facade.
    //!
    //! Authority-bearing values intentionally keep construction, fields, and debug output
    //! private:
    //!
    //! ```compile_fail
    //! fn requires_debug<T: std::fmt::Debug>() {}
    //! requires_debug::<orca_runtime::unstable_surface::AuthorityFingerprint>();
    //! ```
    //!
    //! ```compile_fail
    //! fn leak(value: &orca_runtime::unstable_surface::AuthorityFingerprint) {
    //!     let _ = &value.operation_id;
    //! }
    //! ```
    //!
    //! Reservation leases can be decoded and inspected but not minted by consumers:
    //!
    //! ```compile_fail
    //! let _ = orca_runtime::unstable_surface::ReservationLease::new;
    //! ```
    pub use crate::runtime_surface::*;
}

#[cfg(test)]
mod tests {
    use crate::extension::{
        ExtensionData, ExtensionRegistryBuilder, ToolCallOutcome, ToolFinishInput,
        ToolLifecycleContributor,
    };
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
    use crate::runtime_state::RuntimeTurnReducer;
    use crate::thread_store::{SessionStore, ThreadStore};
    use orca_core::config::PermissionProfileNetworkAccess;
    use std::collections::HashMap;
    use std::io;
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
        let write_root = std::env::temp_dir().join("orca-write-root");
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
    fn runtime_event_projector_projects_reasoning_lifecycle() {
        use orca_core::event_schema::EventFactory;
        use orca_core::event_sink::EventSink;
        use orca_core::thread_identity::TurnId;
        use orca_core::thread_item_projection::{CompletedModelResponse, ModelResponseIdentity};
        use orca_core::{config::OutputFormat, event_schema::EventDraft};

        use crate::protocol::ServerEvent;
        use crate::runtime_event_projector::RuntimeEventProjector;

        fn runtime_line(event: EventDraft) -> String {
            let mut output = Vec::new();
            EventSink::new(&mut output, OutputFormat::Jsonl)
                .emit(event)
                .expect("serialize runtime event");
            String::from_utf8(output)
                .expect("runtime event is utf8")
                .trim()
                .to_string()
        }

        let identity = ModelResponseIdentity::new(TurnId::new());
        let reasoning_item_id = identity.item_ids.reasoning_item_id.to_string();
        let response = CompletedModelResponse::new(
            identity.clone(),
            Some("answer".to_string()),
            Some("thinking".to_string()),
            Vec::new(),
        );
        let mut events = EventFactory::new("reasoning-lifecycle".to_string());
        let mut projector = RuntimeEventProjector::default();
        let started = projector.project_line(&runtime_line(
            events.assistant_reasoning_delta(&identity, "thinking"),
        ));

        assert_eq!(started.len(), 3);
        assert!(matches!(
            &started[0],
            ServerEvent::ItemStarted { item, .. }
                if item["id"] == reasoning_item_id
                    && item["type"] == "reasoning"
                    && item["summary"] == ""
        ));
        assert!(matches!(
            &started[1],
            ServerEvent::ItemReasoningDelta { item_id, delta }
                if item_id == &reasoning_item_id && delta == "thinking"
        ));
        assert!(matches!(
            &started[2],
            ServerEvent::ReasoningDelta { text } if text == "thinking"
        ));

        let completed =
            projector.project_line(&runtime_line(events.model_response_completed(&response)));
        assert!(matches!(
            completed.last(),
            Some(ServerEvent::ItemCompleted { item, .. })
                if item["id"] == reasoning_item_id
                    && item["type"] == "reasoning"
                    && item["summary"] == "thinking"
        ));
    }

    #[test]
    fn runtime_event_projector_forwards_core_completed_item_shapes() {
        use orca_core::event_schema::EventFactory;
        use orca_core::event_sink::EventSink;
        use orca_core::thread_identity::TurnId;
        use orca_core::thread_item_projection::{CompletedModelResponse, ModelResponseIdentity};
        use orca_core::{config::OutputFormat, event_schema::EventDraft};

        use crate::protocol::ServerEvent;
        use crate::runtime_event_projector::RuntimeEventProjector;

        fn runtime_line(event: EventDraft) -> String {
            let mut output = Vec::new();
            EventSink::new(&mut output, OutputFormat::Jsonl)
                .emit(event)
                .expect("serialize runtime event");
            String::from_utf8(output)
                .expect("runtime event is utf8")
                .trim()
                .to_string()
        }

        let response = CompletedModelResponse::new(
            ModelResponseIdentity::new(TurnId::new()),
            Some("Preface\n<proposed_plan>\n- step\n</proposed_plan>".to_string()),
            Some("thinking".to_string()),
            Vec::new(),
        );
        let expected = response
            .completed_items()
            .into_iter()
            .map(|item| item.into_value())
            .collect::<Vec<_>>();
        let mut events = EventFactory::new("completed-item-shapes".to_string());
        let projected = RuntimeEventProjector::default()
            .project_line(&runtime_line(events.model_response_completed(&response)));
        let actual = projected
            .into_iter()
            .filter_map(|event| match event {
                ServerEvent::ItemCompleted { item, .. } => Some(item),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
    }
    #[test]
    fn session_records_one_canonical_assistant_completion() {
        use orca_core::config::OutputFormat;
        use orca_core::conversation::{Conversation, Message};
        use orca_core::event_schema::{EventEnvelope, EventFactory, EventType};
        use orca_core::event_sink::EventSink;
        use orca_core::thread_identity::TurnId;
        use orca_core::thread_item_projection::{CompletedModelResponse, ModelResponseIdentity};

        let response = CompletedModelResponse::new(
            ModelResponseIdentity::new(TurnId::new()),
            Some("answer".to_string()),
            Some("thinking".to_string()),
            Vec::new(),
        );
        let mut conversation = Conversation::new();
        conversation.add_user("question".to_string());
        let mut output = Vec::new();
        let mut events = EventFactory::new("assistant-recording".to_string());
        {
            let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
            crate::session::record_assistant_response_for_agent(
                &mut conversation,
                &response,
                true,
                &mut events,
                &mut sink,
            )
            .expect("record assistant response");
        }

        assert!(matches!(
            conversation.messages.last(),
            Some(Message::Assistant {
                content: Some(content),
                reasoning_content: Some(reasoning),
                ..
            }) if content == "answer" && reasoning == "thinking"
        ));
        let envelopes = String::from_utf8(output)
            .expect("runtime output is utf8")
            .lines()
            .map(|line| serde_json::from_str::<EventEnvelope>(line).expect("runtime event"))
            .collect::<Vec<_>>();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].event_type, EventType::ModelResponseCompleted);
    }
}

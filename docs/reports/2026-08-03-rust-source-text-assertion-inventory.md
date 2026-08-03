# Rust Source-Text Assertion Inventory

- Date: 2026-08-03
- Baseline: 282 include_str! sites
- Runtime library: 254 sites across 112 tests
- TUI and integration tests: 28 sites
- Additional source readers found beyond the include_str! baseline: 17 sites

## Classification

| Classification | Sites | Disposition |
|---|---:|---|
| Dependency/import and private Rust layout | 271 | Removed; Cargo metadata, normal compilation, typed facades, and compile-fail visibility own these boundaries |
| Behavioral assertions expressed as source text | 7 | Removed or rewritten against runtime values; existing focused input, Vim, UI, and AppState tests retain behavior coverage |
| Exact public fixture bytes | 3 | Retained with contract comments: runtime-surface manifest, incident JSONL, workflow host JavaScript |
| Obsolete duplicate fixture inclusion | 1 | Consolidated into the single workflow-host JavaScript fixture constant |

## Runtime Library Sites

Every site below inspected private Rust spelling or placement and was removed. The containing behavior tests that did not read source remain in orca-runtime.

| Containing test | Sites | Classification |
|---|---:|---|
| tool_execution_context_groups_extension_store_refs | 1 | dependency/import |
| step_and_normal_tool_turn_contexts_group_runtime_extensions | 2 | dependency/import |
| runtime_step_context_exposes_request_snapshot_contract | 3 | dependency/import |
| runtime_step_snapshot_groups_runtime_capabilities_contract | 2 | dependency/import |
| runtime_provider_cycle_reuses_step_capability_snapshot_contract | 2 | dependency/import |
| runtime_turn_interaction_state_groups_turn_scoped_interaction_handlers | 4 | dependency/import |
| sampling_request_state_owns_tool_permission_overlay | 2 | dependency/import |
| sampling_request_state_owns_tool_dispatch_cursor | 3 | dependency/import |
| sampling_request_state_owns_tool_dispatch_windows | 3 | dependency/import |
| sampling_request_state_owns_normal_tool_result_recording | 2 | dependency/import |
| runtime_turn_kernel_owns_sampling_request_state_and_reducer | 2 | dependency/import |
| runtime_turn_kernel_binds_provider_step_context_extensions | 2 | dependency/import |
| runtime_turn_kernel_assembles_provider_response_input | 2 | dependency/import |
| runtime_provider_response_input_groups_io_refs_contract | 2 | dependency/import |
| runtime_provider_turn_input_groups_io_refs_contract | 1 | dependency/import |
| runtime_turn_kernel_assembles_turn_loop_state | 2 | dependency/import |
| turn_loop_iteration_and_provider_contexts_group_runtime_extensions | 4 | dependency/import |
| runtime_turn_state_exposes_grouped_runtime_extension_context | 2 | dependency/import |
| runtime_turn_state_directives_route_through_runtime_reducer | 2 | dependency/import |
| tool_call_item_lifecycle_projection_is_owned_by_tool_item_projection | 2 | dependency/import |
| file_change_item_lifecycle_projection_is_owned_by_tool_item_projection | 2 | dependency/import |
| workflow_item_lifecycle_projection_is_owned_by_tool_item_projection | 2 | dependency/import |
| user_message_item_type_is_owned_by_core_thread_item_projection | 3 | dependency/import |
| persisted_message_item_type_is_owned_by_core_thread_item_projection | 3 | dependency/import |
| runtime_turn_context_types_are_owned_by_lifecycle_module | 2 | dependency/import |
| runtime_turn_continuation_is_owned_by_turn_context | 1 | dependency/import |
| runtime_turn_steer_handle_is_owned_by_turn_context | 1 | dependency/import |
| runtime_lifecycle_state_machine_is_owned_by_runtime_lifecycle_module | 2 | dependency/import |
| runtime_tool_actor_context_is_owned_by_runtime_tool_actor_module | 2 | dependency/import |
| runtime_user_input_boundary_is_owned_by_runtime_user_input_module | 2 | dependency/import |
| runtime_pending_interaction_boundary_is_owned_by_focused_module | 1 | dependency/import |
| runtime_permission_boundary_is_owned_by_runtime_permission_module | 2 | dependency/import |
| runtime_approval_boundary_is_owned_by_runtime_approval_module | 3 | dependency/import |
| child_agent_loop_setup_boundary_is_owned_by_focused_module | 2 | dependency/import |
| child_agent_provider_turn_boundary_is_owned_by_focused_module | 2 | dependency/import |
| child_agent_response_folding_boundary_is_owned_by_focused_module | 2 | dependency/import |
| child_agent_loop_runner_boundary_is_owned_by_focused_module | 2 | dependency/import |
| child_agent_types_boundary_is_owned_by_focused_module | 2 | dependency/import |
| child_agent_behavior_tests_are_owned_by_focused_module | 2 | dependency/import |
| child_agent_entrypoints_are_owned_by_focused_module | 2 | dependency/import |
| thread_steer_handle_is_owned_by_lifecycle_module | 2 | dependency/import |
| runtime_steer_step_is_owned_by_runtime_steer_module | 3 | dependency/import |
| agent_loop_context_is_owned_by_lifecycle_module | 2 | dependency/import |
| agent_tool_policy_context_is_owned_by_tool_invocation_module | 2 | dependency/import |
| agent_tool_schema_override_is_owned_by_tool_invocation_module | 3 | dependency/import |
| provider_tool_request_extraction_is_owned_by_tool_invocation_module | 2 | dependency/import |
| normal_tool_execution_entrypoint_is_owned_by_tool_execution_module | 2 | dependency/import |
| tool_request_cursor_is_owned_by_sampling_request_state | 4 | dependency/import |
| tool_turn_outcome_is_owned_by_tool_turn_module | 3 | dependency/import |
| normal_tool_turn_runner_is_owned_by_tool_turn_module | 3 | dependency/import |
| normal_tool_turn_runner_uses_grouped_context | 1 | dependency/import |
| bash_runtime_runner_uses_grouped_invocation_context | 2 | dependency/import |
| runtime_bash_internal_execution_uses_grouped_contexts | 1 | dependency/import |
| tool_turn_dispatch_uses_grouped_context | 2 | dependency/import |
| child_tool_policy_gate_is_owned_by_tool_invocation_module | 2 | dependency/import |
| normal_tool_result_recording_is_owned_by_sampling_request_state | 4 | dependency/import |
| subagent_batch_result_recording_is_owned_by_subagent_execution_module | 2 | dependency/import |
| subagent_batch_tool_turn_runner_is_owned_by_subagent_execution_module | 3 | dependency/import |
| async_subagent_worker_is_owned_by_async_worker_module | 3 | dependency/import |
| tool_turn_dispatch_loop_is_owned_by_tool_turn_module | 3 | dependency/import |
| agent_tool_result_recording_is_owned_by_session_module | 2 | dependency/import |
| agent_plan_state_recording_is_owned_by_session_module | 2 | dependency/import |
| final_memory_extraction_is_owned_by_memory_module | 2 | dependency/import |
| agent_conversation_bootstrap_is_owned_by_session_module | 2 | dependency/import |
| agent_provider_config_construction_is_owned_by_tool_invocation_module | 2 | dependency/import |
| agent_tool_approval_policy_construction_is_owned_by_tool_execution_module | 2 | dependency/import |
| tool_execution_approval_gate_uses_grouped_context | 1 | dependency/import |
| runtime_compaction_step_is_owned_by_lifecycle_module | 3 | dependency/import |
| runtime_compaction_policy_task_and_outcome_are_owned_by_compaction_module | 1 | dependency/import |
| runtime_provider_turn_step_is_owned_by_provider_turn_module | 3 | dependency/import |
| thread_store_trait_is_owned_by_thread_store_module | 2 | dependency/import |
| thread_store_uses_focused_submodules | 7 | dependency/import |
| jsonl_thread_store_impl_is_owned_by_thread_store_module | 2 | dependency/import |
| thread_projection_helpers_are_owned_by_focused_thread_store_modules | 5 | dependency/import |
| tool_item_projection_helpers_are_owned_by_shared_projection_module | 4 | dependency/import |
| jsonl_thread_store_type_is_owned_by_thread_store_module | 2 | dependency/import |
| thread_store_api_types_are_owned_by_thread_store_module | 2 | dependency/import |
| live_thread_handle_is_owned_by_thread_store_module | 2 | dependency/import |
| session_meta_is_owned_by_thread_store_module | 2 | dependency/import |
| session_summary_is_owned_by_thread_store_module | 2 | dependency/import |
| session_transcript_is_owned_by_thread_store_module | 2 | dependency/import |
| session_writer_is_owned_by_thread_store_module | 2 | dependency/import |
| jsonl_record_types_are_owned_by_thread_store_module | 2 | dependency/import |
| jsonl_append_writer_helpers_are_owned_by_thread_store_module | 2 | dependency/import |
| jsonl_read_rewrite_helpers_are_owned_by_thread_store_module | 2 | dependency/import |
| session_read_models_are_owned_by_thread_store_module | 2 | dependency/import |
| thread_record_lookup_is_owned_by_thread_store_module | 2 | dependency/import |
| runtime_provider_response_step_is_owned_by_lifecycle_module | 3 | dependency/import |
| runtime_turn_setup_step_is_owned_by_runtime_turn_setup_module | 3 | dependency/import |
| runtime_turn_start_step_is_owned_by_runtime_turn_start_module | 3 | dependency/import |
| runtime_model_route_step_is_owned_by_runtime_model_route_module | 3 | dependency/import |
| runtime_turn_opening_step_is_owned_by_runtime_turn_opening_module | 3 | dependency/import |
| runtime_provider_error_step_is_owned_by_lifecycle_module | 3 | dependency/import |
| runtime_turn_provider_cycle_step_is_composed_by_runtime_turn_iteration_module | 4 | dependency/import |
| runtime_turn_iteration_step_is_owned_by_runtime_turn_iteration_module | 2 | dependency/import |
| runtime_turn_loop_step_is_owned_by_runtime_turn_loop_module | 3 | dependency/import |
| runtime_turn_loop_input_is_owned_by_runtime_turn_loop_module | 3 | dependency/import |
| runtime_turn_workflow_refs_are_grouped_for_turn_loop | 2 | dependency/import |
| runtime_turn_output_refs_are_grouped_for_turn_loop | 2 | dependency/import |
| runtime_turn_provider_refs_are_grouped_for_turn_loop | 2 | dependency/import |
| runtime_turn_request_refs_are_grouped_for_turn_loop | 2 | dependency/import |
| runtime_agent_turn_loop_input_uses_grouped_turn_contexts | 1 | dependency/import |
| runtime_turn_loop_inputs_use_runtime_turn_deps | 2 | dependency/import |
| runtime_turn_loop_inputs_use_runtime_turn_policy_context | 2 | dependency/import |
| runtime_turn_iteration_input_uses_loop_iteration_state | 1 | dependency/import |
| runtime_turn_loop_state_resolves_runtime_directive_policy | 3 | dependency/import |
| session_list_load_operations_are_owned_by_thread_store_module | 2 | dependency/import |
| session_search_operations_are_owned_by_thread_store_module | 2 | dependency/import |
| session_mutation_operations_are_owned_by_thread_store_module | 2 | dependency/import |
| protocol_imports_thread_types_from_thread_store_boundary | 1 | dependency/import |
| agent_loop_imports_session_types_from_thread_store_boundary | 1 | dependency/import |
| session_imports_session_types_from_thread_store_boundary | 1 | dependency/import |

## TUI And Integration Sites

| Containing test or scope | Sites | Classification | Disposition |
|---|---:|---|---|
| surface_boundary_tests manifest constant | 1 | exact bytes | retained with contract comment |
| surface_boundary_tests Rust-source constants/assertions | 17 | dependency/import | removed; Node contract scanner plus structured Rust manifest tests remain |
| app source ownership tests | 4 | behavior | removed; input runtime, focus, clear, and hosted-loop behavior suites remain |
| vim line-positioning source test | 1 | behavior | removed; 20,000-column/row differential test remains |
| ui deterministic status source scan | 1 | behavior | rewritten to assert deterministic values directly |
| types workflow-notification source scan | 1 | behavior | rewritten by constructing and matching the typed variant |
| runtime lifecycle incident fixture | 1 | exact bytes | retained as released incident JSONL |
| two workflow-host EOF tests | 2 | exact bytes plus obsolete duplicate | consolidated into one released JavaScript fixture constant |

## Additional Source Readers

The baseline count covered the audit's include_str! search scope. A repository-wide sweep found 17 more sites that inspected private Rust spelling or placement; none are retained.

| Containing test or scope | Sites | Classification | Disposition |
|---|---:|---|---|
| app submitted-turn source readers | 2 | dependency/import | removed; typed construction and hosted request behavior remain |
| orca-tools sandbox constructor source inclusions | 2 | dependency/import | removed; sandbox command behavior and compilation own the boundary |
| restricted Windows PTY source inclusion | 1 | behavior | removed; native restricted ConPTY spawn, resize, and completion test remains |
| runtime-surface event schema source inclusion | 1 | dependency/import | replaced with an exhaustive typed EventType match against the reviewed manifest |
| runtime-surface visibility and zeroization source inclusions | 3 | dependency/import plus behavior | replaced with rustdoc compile-fail visibility checks and a direct zeroization test |
| async worker argv source inclusion | 1 | behavior | removed; the same test inspects the spawned process argv and private stdin handoff |
| JSONL server sandbox ownership source inclusion | 1 | dependency/import | removed; permission-profile resolution behavior tests remain |
| ACP import-boundary source readers | 2 | dependency/import | removed with the legacy scanner; ACP runtime-surface behavior suites remain |
| JSONL import-boundary source reader | 1 | dependency/import | removed with the legacy scanner; JSONL differential and interaction suites remain |
| JSONL surface-routing source readers | 3 | dependency/import | removed with the legacy scanner; runtime-surface host and JSONL behavior suites remain |

## Structured Replacements

- dependency_architecture_contract consumes cargo metadata format-version 1 JSON for package edges and target ownership.
- Public runtime boundaries compile through the surface, workflow, and update facades.
- Rustdoc compile-fail contracts prove the private runtime_surface namespace, authority fields and Debug output, and reservation constructor remain inaccessible.
- The runtime-surface event inventory is an exhaustive typed match, so new EventType variants fail compilation until the inventory is reviewed.
- Runtime, TUI, CLI, lifecycle, Vim, UI, sandbox, worker-argv, Windows ConPTY, ACP, and JSONL behavior remains covered by executable tests rather than source spelling.

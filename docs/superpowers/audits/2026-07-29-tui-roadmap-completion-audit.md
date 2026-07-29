# TUI Terminal Experience Roadmap Completion Audit

**Audit result: Complete**

**Audited branch:** `feature/tui-syntax-highlighting`

**Verified implementation HEAD:** `bd497d3c6d4245bdddd49383ae9e1cd8b5bd9408`

**Verified remote HEAD before this audit commit:** `bd497d3c6d4245bdddd49383ae9e1cd8b5bd9408`
**Audit dates:** 2026-07-29 through 2026-07-30, Asia/Shanghai

This audit maps every P0 and P2 item in the thread objective to committed
designs, plans, production artifacts, direct tests, and fresh verification.
Source-shape tests are noted where present, but no roadmap row is marked
Complete from a source-string assertion alone.

## Verification Summary

Fresh verification used `CARGO_BUILD_JOBS=2` after removing only this
worktree's rebuildable `target/debug/incremental` cache.

| Gate | Result |
|---|---|
| `cargo test -p orca-core` | 228 passed |
| `cargo test -p orca-tui` | 1095 passed |
| `cargo check --workspace` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `git diff --check` | Passed for tracked changes |
| `git diff --cached --check` | Required and passed after staging this new audit file |
| `cargo test --workspace --all-targets` | Three intermittent failures appeared progressively; each exact test passed once on rerun, and all other tests passed with only those three skipped |

Verification environment:

- `rustc 1.92.0 (ded5c06cf 2025-12-08)`
- `cargo 1.92.0 (344c4567c 2025-10-21)`
- verification window: 2026-07-30 01:55–02:16, Asia/Shanghai
- implementation HEAD for every recorded run:
  `bd497d3c6d4245bdddd49383ae9e1cd8b5bd9408`

Raw command output is retained locally for this run under
`/tmp/orca-final-verify`; it is not committed to the repository. The manifest
below makes the operator-recorded summary checkable while those local files
remain available:

| Command output | SHA-256 |
|---|---|
| `orca-core.log` | `51b6cc692f986d9b3adad0ceccf833dba99e7a537e7ba55a5f78a4604880b179` |
| `orca-tui.log` | `0cbe4e6b494152caba7343f10ebf57d89fbfefa54c233b911c829efe505aad65` |
| `workspace-all-targets.log` | `66005dbc51ec69fdb5827ec5c8d7366f63ac8f5c0edca98b1a84a6cad5b513d9` |
| `workspace-all-targets-skip-external-timeout.log` | `8a5777bb6a4b3bfde458656f5eaad9eed4f10d84989b1d6d563e206312bee7fe` |
| `workspace-all-targets-skip-two-flakes.log` | `25ceceab15da916090daba8fce1c1a19c19c241ffd03cdae3c324609305b134c` |
| `workspace-all-targets-skip-three-flakes.log` | `83fca13345d4a2cdee47f59a5eb40d390d2af1be7aff1c304d9ac41fdbfb81e1` |
| `workspace-check.log` | `2a68a9b3cf6a3a6f79185d106bf5953855c55cb18d12e7e5be0f24f93af28b01` |
| `exact-reruns.log` | `b609d9939b7b933a78a9ce95333f7ae79aa649a44cadb2f62f59e4e56a4cd47f` |
| `focused-roadmap.log` | `d239c7e798f9ba48201369b6a39b465c42d49c5bd9872a370c48bb93ad2a7da6` |
| `fmt-check.log` | `1a22ed01d182bb8394d150c7396159e334c5148288ed07e41d4c0be4d23b4c07` |
| `cached-diff-check.log` | `6ef30d0dab3ffaa482362512635b064a912b9785203a6599db89398788cc75d8` |

The exact commands were:

```bash
CARGO_BUILD_JOBS=2 cargo test -p orca-core
CARGO_BUILD_JOBS=2 cargo test -p orca-tui
CARGO_BUILD_JOBS=2 cargo test --workspace --all-targets
CARGO_BUILD_JOBS=2 cargo check --workspace
cargo fmt --all -- --check
git diff --check
```

`exact-reruns.log` records the three exact rerun commands. The focused loop
and every expanded command are recorded in `focused-roadmap.log`.

Fresh focused roadmap matrix:

| Filter | Result |
|---|---|
| `cargo test -p orca-tui syntax -- --quiet` | 27 passed |
| `cargo test -p orca-tui hardware_cursor -- --quiet` | 15 passed |
| `cargo test -p orca-tui markdown -- --quiet` | 27 passed |
| `cargo test -p orca-tui terminal_capabilities -- --quiet` | 7 passed |
| `cargo test -p orca-tui terminal_presentation -- --quiet` | 10 passed |
| `cargo test -p orca-tui diff_highlight -- --quiet` | 94 passed |
| `cargo test -p orca-tui streaming -- --quiet` | 26 passed |
| `cargo test -p orca-tui transcript_search -- --quiet` | 8 passed |
| `cargo test -p orca-tui queued -- --quiet` | 40 passed |
| `cargo test -p orca-tui workspace_status -- --quiet` | 15 passed |
| `cargo test -p orca-tui vim -- --quiet` | 54 passed |
| `cargo test -p orca-tui keybindings -- --quiet` | 39 passed |
| `cargo test -p orca-tui diagnostics -- --quiet` | 22 passed |
| `cargo test -p orca-tui onboarding -- --quiet` | 30 passed |

## P0 Audit

### P0.1 Code and Diff Syntax Highlighting — Complete

Production artifacts:

- `crates/orca-tui/src/syntax_highlight.rs`
  - syntect/two-face highlighting
  - `MAX_HIGHLIGHT_BYTES = 512 * 1024`
  - `MAX_HIGHLIGHT_LINES = 10_000`
  - `MAX_HIGHLIGHT_LINE_BYTES = 4 * 1024`
- `crates/orca-tui/src/diff_highlight.rs`
  - hunk-local first paint with bounded deadline
  - full-file parser-state refinement
  - aggregate and per-line fallback guards
- `crates/orca-tui/src/edit_highlight_worker.rs`
  - bounded regular-file reads
  - versioned background jobs and coalescing
- `crates/orca-tui/src/transcript_view.rs`
  - `syntax_theme_revision` in cache identity
- `crates/orca-tui/src/types.rs`
  - stale-result identity rejection and targeted revision updates

Direct evidence includes:

- `strict_limits_reject_only_values_above_each_ceiling`
- `path_highlighter_preserves_multiline_parser_state`
- `aggregate_guardrails_also_disable_refined_overlays`
- `full_file_guardrails_reject_total_bytes_above_limit`
- `full_file_guardrails_reject_lines_above_byte_limit`
- `run_job_accepts_exact_byte_line_count_and_line_length_boundaries`
- `syntax_theme_revision_rebuilds_highlighted_wrapped_lines_once`
- `idle_ready_poll_schedules_actual_render_with_refined_styles_once`

Design and plan:

- `docs/superpowers/specs/2026-07-23-tui-syntax-highlighting-design.md`
  - `c5918692507f46dbeda159f024854c5b6d58762f`
- `docs/superpowers/plans/2026-07-23-tui-syntax-highlighting.md`
  - `79098b2739827a5bcd8afca42d549833b6239b1f`

### P0.2 IME Hardware Cursor — Complete

Production artifacts:

- `crates/orca-tui/src/ui.rs`
  - `HardwareCursorProjection`
  - display-aware textarea cursor projection
  - one final `frame.set_cursor_position` owner
  - setup, search, composer, popup, modal, and hidden-cursor routing
- `crates/orca-tui/src/composer_textarea.rs`
  - software cursor layout shared with hardware projection
- `crates/orca-tui/src/capability_backend.rs`
  - complete cursor backend delegation

Direct evidence includes:

- `hardware_cursor_matches_idle_composer_software_cursor`
- `hardware_cursor_matches_wrapped_cjk_composer_software_cursor`
- `main_composer_internal_grapheme_cursor_uses_rendered_lead_cell`
- `only_api_key_step_moves_hardware_cursor`
- `compact_setup_api_key_keeps_mask_and_hardware_cursor_visible`
- `waiting_approval_frame_hides_the_hardware_cursor_without_moving_it`
- `composer_cursor_hides_and_restores_across_consecutive_frames`

Design and plan:

- `docs/superpowers/specs/2026-07-27-tui-ime-hardware-cursor-design.md`
  - `2abe83cf53cff985b24d76d226f486b58c21f9bc`
- `docs/superpowers/plans/2026-07-27-tui-ime-hardware-cursor.md`
  - `3a9ed051ff6313d441a27bcd568dd21ecd0757dd`

Residual limitation: automated tests validate final terminal cursor events and
the full grapheme/state matrix; they do not launch a real IME candidate window.

### P0.3 Markdown Theme Colors — Complete

Production artifacts:

- `crates/orca-tui/src/theme.rs`
  - semantic heading and inline-code fields for Dark, Light, Solarized, and
    Catppuccin
  - terminal-level adaptation
- `crates/orca-tui/src/ui.rs`
  - headings, inline code, tables, quotes, list markers, and fallback code use
    theme semantics rather than fixed setup colors
- `crates/orca-tui/src/transcript_view.rs`
  - Markdown theme colors participate in cache identity
- `crates/orca-tui/src/terminal_capabilities.rs`
  - monochrome resets colors while preserving semantic modifiers

Direct evidence includes:

- `named_themes_define_markdown_semantic_colors`
- `markdown_semantic_colors_do_not_use_fixed_ansi_accents`
- `markdown_roles_use_selected_theme_semantics`
- `markdown_tables_and_plain_code_use_theme_semantics`
- `inline_code_uses_the_selected_markdown_theme_color`
- `markdown_theme_color_change_rebuilds_wrapped_lines_once`
- `monochrome_style_preserves_modifiers_and_resets_colors`

Design and plan:

- `docs/superpowers/specs/2026-07-28-tui-markdown-theme-colors-design.md`
  - `dd2d339a88d8a0bd810882eb60ff96565e2670ea`
- `docs/superpowers/plans/2026-07-28-tui-markdown-theme-colors.md`
  - `0bbed876c5fb9ba1ec1a55978fd31f2402a6ffc9`

### P0.4 Terminal Capability Detection and Theme Degradation — Complete

Production artifacts:

- `crates/orca-tui/src/input_runtime.rs`
  - one input runtime owns the qwertty session
  - Auto theme probes captured background before modes and reads
- `crates/orca-tui/src/terminal_capabilities.rs`
  - TrueColor, ANSI 256, ANSI 16, and Monochrome classification
  - stable RGB quantization and background classification
- `crates/orca-tui/src/theme.rs`
  - every theme color adapts to the captured profile
- `crates/orca-tui/src/capability_backend.rs`
  - final changed-cell safety adaptation
- `crates/orca-tui/src/app.rs`
  - one startup profile feeds theme, diagnostics, and backend

Direct evidence includes:

- `auto_probe_precedes_modes_ready_reads_and_leave`
- `explicit_theme_skips_probe_but_keeps_mode_order`
- `color_support_facts_map_to_exact_levels`
- `rgb_quantization_uses_stable_xterm_palettes`
- `resolved_themes_choose_base_palette_and_obey_color_level`
- `capability_backend_adapts_changed_cells_and_preserves_metadata`
- `preview_preserves_each_captured_color_level_and_auto_background`

Design and plan:

- `docs/superpowers/specs/2026-07-28-tui-terminal-capabilities-design.md`
  - `e6b9ceb9dc2068b27a08664041226641f7c97171`
- `docs/superpowers/plans/2026-07-28-tui-terminal-capabilities.md`
  - `e9dd1e998736d6e11709e01e7a4541d7cc46e795`

Residual limitation: automated tests use an injected qwertty driver and do not
send OSC 11 to the developer's real terminal.

### P0.5 Notifications, Focus, and Terminal Titles — Complete

Production artifacts:

- `crates/orca-tui/src/terminal_presentation.rs`
  - OSC 9, tmux DCS passthrough, BEL fallback, OSC 0 titles, bounded queue,
    sanitized text, title reset
- `crates/orca-tui/src/input_event_actions.rs`
  - FocusGained/FocusLost projection
- `crates/orca-tui/src/input_runtime.rs`
  - opt-in focus event channel and mode ownership
- `crates/orca-tui/src/runtime_event_actions.rs`
  - safe event-to-notification classification
- `crates/orca-tui/src/app.rs`
  - startup, frame write, resume invalidation, and exit reset ordering

Direct evidence includes:

- `terminal_presentation_encodes_osc9_bel_osc0_and_tmux`
- `terminal_presentation_title_matrix_and_animation_are_stable`
- `terminal_presentation_suppresses_focused_and_disabled_notifications`
- `consume_focus_event_updates_only_presentation_focus`
- `terminal_title_writes_before_initial_draw`
- `presentation_resume_clears_invalidates_then_marks_dirty`
- `presentation_exit_resets_drops_then_finishes_input`

Design and plan:

- `docs/superpowers/specs/2026-07-28-tui-notifications-title-design.md`
  - `a8c2a669203af9a195ec656cc89c9e70f4eb2a18`
- `docs/superpowers/plans/2026-07-28-tui-notifications-title.md`
  - `b6183801fdeae4bbacde4febd132e33e57099e59`

Residual limitation: byte encoding is directly tested, but no automated test
writes OSC/BEL sequences to a real terminal emulator.

### P0.6 Diff Rendering — Complete

Production artifacts:

- `crates/orca-tui/src/theme.rs`
  - add/remove and emphasis backgrounds
- `crates/orca-tui/src/diff_highlight.rs`
  - right-aligned dual line-number gutters
  - `⋮` hunk separators
  - changed-row backgrounds
  - inline replacement emphasis
  - terminal capability fallbacks and malformed raw fallback

Direct evidence includes:

- `structured_diff_uses_one_right_aligned_dual_line_number_gutter`
- `multiple_hunks_share_the_largest_gutter_width`
- `hunk_separator_hides_coordinates_and_keeps_scope_context`
- `changed_rows_apply_exact_dark_backgrounds_to_gutter_and_content`
- `adjacent_replacement_emphasizes_only_changed_inline_tokens`
- `inline_emphasis_preserves_unicode_and_combining_text_exactly`
- `monochrome_inline_emphasis_uses_bold_without_backgrounds`
- `one_expired_inline_deadline_suppresses_emphasis_across_all_hunks`

Design and plan:

- `docs/superpowers/specs/2026-07-28-tui-diff-rendering-design.md`
  - initial `d80d504dc94a097a3e5c0f1775f488d16321d79f`
  - final contract revision `cd65417dc317c43c2611307f82a8a766113d88c8`
- `docs/superpowers/plans/2026-07-28-tui-diff-rendering.md`
  - initial `bb046b555f80785d98b4e31f3f53ac39f673bd61`
  - final contract revision `cd65417dc317c43c2611307f82a8a766113d88c8`

### P0.8 Streaming Checkpoints — Complete

Production artifacts:

- `crates/orca-tui/src/streaming_markdown.rs`
  - newline gate, stable block freeze, fence state, table candidate/holdback
- `crates/orca-tui/src/types.rs`
  - immutable `AssistantChunk` checkpoints and one mutable tail
- `crates/orca-tui/src/transcript_view.rs`
  - frozen revision/cache stability
- `crates/orca-tui/src/ui.rs`
  - direct frame projection of held and released content

Direct evidence includes:

- `partial_source_line_stays_hidden_until_newline_or_finish`
- `blank_line_freezes_the_visible_tail_and_starts_a_fresh_block`
- `confirmed_table_remains_hidden_until_boundary_then_emits_once`
- `complete_lines_mutate_only_the_active_assistant_tail_revision`
- `streaming_newline_gate_hides_partial_source_until_completion_frame`
- `streaming_table_holdback_reveals_the_whole_table_at_one_boundary`
- `streaming_auto_follow_tracks_checkpoints_without_showing_partial_tail`

Design and plan:

- `docs/superpowers/specs/2026-07-28-tui-streaming-checkpoints-design.md`
  - `8066b3ff15e418a0a43dbb876fe0175e340d7c77`
- `docs/superpowers/plans/2026-07-28-tui-streaming-checkpoints.md`
  - `833fcb324e2500b9310c01c9b585f780407e78fb`

## P2 Audit

### Transcript Search — Complete

Production artifacts:

- `crates/orca-tui/src/transcript_search.rs`
- `crates/orca-tui/src/status_key_actions.rs`
- `crates/orca-tui/src/ui.rs`
- `crates/orca-tui/src/shortcuts.rs`

Behavior:

- Ctrl+F opens search.
- Vim `/`, `n`, and `N` share the same state.
- Search scans cached transcript lines, preserves active identity through
  streaming/resize, wraps navigation, and styles only visible matches.

Direct evidence includes:

- `next_and_previous_wrap_without_rescanning`
- `search_keyboard_frames_move_active_match_without_composer_mutation`
- `vim_slash_opens_search_in_every_conversation_status_without_composer_edit`
- `vim_n_and_shift_n_navigate_closed_running_search_without_interrupt`
- `streaming_and_resize_refresh_matches_without_stealing_active_identity`
- `search_overlay_styles_only_visible_matches_and_selection_wins`

Design and plan:

- design `2690a59dd1ffddb6d4de589f3c3706c20fdd2bec`
- plan `cf833e35127691dd8dbedf4021040375f3a833ba`

### Queued Message Visibility and Editing — Complete

Production artifacts:

- `crates/orca-tui/src/queued_input.rs`
- `crates/orca-tui/src/queued_input_actions.rs`
- `crates/orca-tui/src/ui.rs`
- `crates/orca-tui/src/shortcuts.rs`

Behavior:

- bounded three-row preview
- head/omission/latest projection
- Alt+Up restores the latest queued input while preserving earlier FIFO items

Direct evidence includes:

- `queued_preview_uses_two_three_and_exactly_three_rows`
- `queued_preview_snapshot_reads_at_most_head_and_tail`
- `restore_latest_replaces_draft_and_preserves_earlier_fifo_items`
- `idle_alt_up_restores_but_waiting_input_keeps_queue_owned`

Design and plan:

- design `7ffcfbff271c1ee21843d923d8a1284c6e3d703a`
- plan `94467cb9260ab3642aa259434575a25476a04e73`

### CWD and Git Status — Complete

Production artifacts:

- `crates/orca-tui/src/workspace_status.rs`
- `crates/orca-tui/src/ui.rs`
- `crates/orca-tui/src/app.rs`

Behavior:

- component-safe home abbreviation
- compact cwd fallback
- symbolic branch or detached 8-character SHA
- captured once before the frame loop

Direct evidence includes:

- `display_cwd_shortens_only_a_component_safe_home_prefix`
- `compact_cwd_uses_full_middle_basename_then_grapheme_safe_truncation`
- `discovery_prefers_symbolic_branch_without_requesting_head`
- `workspace_status_spans_keep_full_then_compact_cwd_with_git`
- `startup_captures_workspace_status_once_before_frame_loop`

Design and plan:

- design `d8cb4c0d46b92d4131a48ce66b2be7c754297d0d`
- plan `6977d9b6b395f15cf93a4e35b1ef613f845c5324`

### Vim Command Core and Insert Escape — Complete

Production artifacts:

- `crates/orca-tui/src/vim.rs`
- `crates/orca-tui/src/vim_command.rs`
- `crates/orca-core/src/config/mod.rs`
- `crates/orca-core/src/config/file.rs`

Behavior:

- counts, `dd`, `gg`, `G`, named registers, linewise paste, and dot repeat
- bounded operations and atomic undo
- configurable two-character insert escape such as `jj`
- mismatch/timeout/mode-exit restore exactly once

Direct evidence includes:

- `counted_motions_and_goto_commands_land_on_exact_positions`
- `dd_deletes_whole_lines_as_one_undoable_change`
- `yy_and_named_registers_preserve_linewise_text`
- `dot_repeats_x_and_counted_line_delete`
- `vim_insert_escape_exact_pair_exits_without_text_or_history`
- `vim_insert_escape_mismatch_overlap_and_expiry_preserve_text_once`

Design and plan:

- command-core design `7d55929474bf7663f5b92ac5ca24c68401e602fd`
- command-core plan `58e0da733085dcf9dbf34165436e88816743d5a5`
- insert-escape design `3744750fff25fe0b4a5d7fa7ab0ece1ad39b7231`
- insert-escape plan `a1941accfded16fd4d0057757f03dc11c884ac94`

### Custom Keybindings — Complete

Production artifacts:

- `crates/orca-tui/src/keybindings/`
- `crates/orca-tui/src/shortcuts.rs`
- `crates/orca-tui/src/app.rs`
- `crates/orca-tui/src/ui.rs`

Behavior:

- Global, Idle, Running, and Approval contexts
- two- through four-stroke chords
- owner/deadline/mismatch handling
- bounded hot reload
- dynamic help and last-known-good retention

Direct evidence includes:

- `two_three_and_four_stroke_chords_complete`
- `mismatch_reroutes_current_key_once`
- `shared_global_prefix_keeps_running_and_approval_chords_reachable`
- `runtime_applies_valid_rejects_invalid_deduplicates_and_restores_defaults`
- `dynamic_shortcut_lines_keep_defaults_and_render_replacements`

Design and plan:

- design `b542d1c1b0cde0613efbf7ff52627d59e97888c4`
- plan `ebb515e0780b1f3f41fa3f041bde6bf0755faec4`

### Doctor and FPS HUD — Complete

Production artifacts:

- `crates/orca-tui/src/diagnostics.rs`
- `crates/orca-tui/src/commands/`
- `crates/orca-tui/src/slash_command_actions.rs`
- `crates/orca-tui/src/ui.rs`
- `crates/orca-tui/src/app.rs`

Behavior:

- bounded private report from captured facts
- capability, notification, keybinding, viewport, and auth projection
- successful-frame-only FPS/render/p95 metrics
- session-only HUD, compact bounds, color adaptation, cursor collision

Direct evidence includes:

- `doctor_report_has_fixed_safe_line_order_and_bounded_size`
- `first_successful_draw_has_zero_fps_and_one_render_sample`
- `fps_hud_uses_top_right_then_top_left_and_hides_on_double_collision`
- `enabled_fps_hud_overlays_top_row_and_preserves_composer_cursor`

Source-shape proxy used only as supplementary evidence:

- `doctor_formatter_source_has_no_runtime_io_or_probe_calls`

Design and plan:

- design `7102fb4ee7a240b28a5a0dd809b6dd6c08ace961`
- plan `b0fc4ed8f458cf35260b2c545b717a3aab08bcb1`

### Expanded Onboarding — Complete

Design and plan:

- initial design `52780861984c8f0935e6344d360903abcff09fbe`
- configuration boundary hardening `4ee54637ba1fae3a4e5053a32370fadc88ba1c0f`
- final design contract `0e1d69c087820d68488339857e4e59f10eed3e42`
- implementation plan `0fdb8f164502f889fdb0de06e8a67431db952e7e`
- production-aligned safety contract `7e74c1fa740ad059022f0990add76b1ca7cce43f`

Implementation commits:

| Scope | Commit |
|---|---|
| Provider persistence and precedence | `6f75511e3a1679173d713452c1bd2a28d0ac435f` |
| Atomic preferences | `9afc8535c1b746eda63e3e7727c50c0d39870276` |
| Hardened auth persistence | `56f3c075383660c83be2e8012a1adad13681d9e6` |
| Typed wizard model | `248f84a4123f2160b971bbae814c7f067eea3d61` |
| Typed actions and Review transaction | `08d75e29d48ef50338c701ee9583ba550fa3e3c5` |
| Capability-preserving theme preview | `d2f340e8ddffb0d6be3da008116a7c2b9acddb17` |
| Seven-step UI | `2f5f51f8ea0b52abbb41694925841c58fc73e681` |
| English and Chinese documentation | `be11cd57c1f8f822a3d3d9b1f563102e3c4f7197` |
| Safety contract alignment | `7e74c1fa740ad059022f0990add76b1ca7cce43f` |
| Secret ownership hardening | `9f5048925cf03f8640b8df84f81ae7d1581713d5` |
| Paste feedback consistency | `bd497d3c6d4245bdddd49383ae9e1cd8b5bd9408` |

Production artifacts:

- `crates/orca-core/src/config/file.rs`
  - provider layering/project deny
  - comment-preserving `toml_edit` patch
  - bounded reads, sidecar lock, owner-only `0600`, ACL reset
  - atomic exchange/no-replace, revalidation, rollback, parent sync
  - stable typed safe errors
- `crates/orca-tui/src/onboarding.rs`
  - seven typed steps and closed production options
  - private credential draft and safe rows/outcomes
- `crates/orca-tui/src/setup_actions.rs`
  - Review transaction and minimal secret ownership
- `crates/orca-tui/src/app.rs`
  - captured-profile theme preview and projection synchronization
- `crates/orca-tui/src/ui.rs`
  - bounded shell, compact masked input, API-key-only cursor
- `README.md`, `README.zh-CN.md`
  - trigger, exact choices, transaction boundary, persistence, fallback

Direct evidence includes:

- provider file/env/CLI precedence and project deny tests
- 90 focused `config::file` tests
- 30 focused onboarding tests
- 11 focused setup action tests
- API-key paste validation tests
- 20x6 compact mask/cursor and extreme geometry tests
- non-setup byte-for-byte parity
- complete initial prompt timing
- full independent spec review: Approved for
  `0e1d69c087820d68488339857e4e59f10eed3e42..7e74c1fa740ad059022f0990add76b1ca7cce43f`
  by review run `019faeea-f5dc-7e53-a2dc-1aa8491e4dd8`; later
  secret-ownership and paste-feedback commits were covered by focused
  re-reviews
- full independent code-quality/security review run
  `019faeee-0053-7f71-922a-ce74baef0b61` found the secret-ownership and
  API-key-error findings; fixes were committed as `9f5048925cf03f8640b8df84f81ae7d1581713d5`
  and `bd497d3c6d4245bdddd49383ae9e1cd8b5bd9408`, then approved by focused
  re-review runs `019faef9-a51f-7f32-adc8-975c15b9dfa9` and
  `019faf02-6444-7310-beaf-d4b7071a1a51`

These review run identifiers are external session evidence, not durable
repository artifacts.

## Intermittent Workspace Failures

No onboarding, setup, persistence, provider precedence, model, theme, cursor,
documentation, or roadmap-focused test was skipped.

The failures appeared progressively, with these commands and matching logs:

1. `cargo test --workspace --all-targets`
   (`workspace-all-targets.log`) failed on the external-tool timeout test.
2. The same command plus
   `--skip external::tests::external_tool_timeout_kills_descendant_processes`
   (`workspace-all-targets-skip-external-timeout.log`) failed on the provider
   cancellation test.
3. The same command plus both prior skip and
   `--skip context::tests::in_flight_summary_request_stops_waiting_for_headers_when_cancelled`
   (`workspace-all-targets-skip-two-flakes.log`) failed on the workflow-host
   callback test.
4. The final three-skip command below
   (`workspace-all-targets-skip-three-flakes.log`) completed successfully.

The three test source files were unchanged from the final onboarding design
baseline. Each exact test also passed once on a fresh rerun. This establishes
that the failures were intermittent and were not caused by direct edits to
those source files. It does **not** prove that branch-wide scheduling,
dependencies, shared state, or test interaction were unrelated.

| Test | Initial failure | Baseline/current blob | Exact rerun |
|---|---|---|---|
| `orca-tools::external::tests::external_tool_timeout_kills_descendant_processes` | trailing command produced `stdout-beforeafter` after a zero-second timeout | `f7f37c51c2b2d4eb2de945c75ea1036fd934e4c9` | 1 passed in 0.30s |
| `orca-provider::context::tests::in_flight_summary_request_stops_waiting_for_headers_when_cancelled` | cancellation observed after 530.926208ms | `8fb6a5f0eabd0569f942f77440f1cd5970a63519` | 1 passed in 0.42s |
| `orca-runtime::workflow::host::tests::event_callback_error_reaps_workflow_process_group` | descendant still observed after callback error | `a704c01d35345feef5a4db1ed0cc74743af64bb4` | 1 passed in 1.40s |

Final remaining-suite command:

```bash
CARGO_BUILD_JOBS=2 cargo test --workspace --all-targets -- \
  --skip external::tests::external_tool_timeout_kills_descendant_processes \
  --skip context::tests::in_flight_summary_request_stops_waiting_for_headers_when_cancelled \
  --skip workflow::host::tests::event_callback_error_reaps_workflow_process_group
```

Result: 41 test binaries reported `test result: ok`; failure scan was empty.
The skipped tests were separately green as shown above.

Residual verification risk: the repository does not currently have a
pull-request workflow that runs this unfiltered gate. Merge readiness therefore
remains conditional on an unfiltered workspace run in CI or another recorded
merge gate. This audit accepts the three failures under the implementation
plan's explicit baseline-match, exact-rerun, and remaining-suite protocol; it
does not claim a causal diagnosis.

## Commit and Remote Audit

For every commit from the final onboarding design baseline through verified
HEAD:

- required trailer count: exactly one
- final non-empty commit-message line:
  `Co-authored-by: TRAE CLI <noreply@bytedance.com>`

Before this audit file was created:

- local HEAD: `bd497d3c6d4245bdddd49383ae9e1cd8b5bd9408`
- remote branch HEAD: `bd497d3c6d4245bdddd49383ae9e1cd8b5bd9408`
- worktree: clean

The audit commit itself must receive the same trailer, then be pushed and
verified before the pull request is created.

## Final Decision

Every P0 and P2 roadmap row has:

1. committed design and implementation plan artifacts;
2. inspected production code;
3. direct behavior tests;
4. fresh focused verification;
5. crate-level and workspace-level verification;
6. explicit residual-risk or flake evidence where real terminal/process timing
   cannot be fully deterministic.

**Result: Complete. The branch is ready for the audit commit, final push, and
pull request creation. Merge readiness remains conditional on an unfiltered
workspace run in CI or another recorded merge gate.**

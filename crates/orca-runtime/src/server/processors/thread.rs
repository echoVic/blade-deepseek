use std::io::{self, Write};

use serde_json::Value;

use super::super::*;

pub(in crate::server::router) fn is_query_operation(op: &ClientOp) -> bool {
    matches!(
        op,
        ClientOp::ThreadRead { .. }
            | ClientOp::ThreadList { .. }
            | ClientOp::ThreadSearch { .. }
            | ClientOp::ThreadTurnsList { .. }
            | ClientOp::ThreadItemsList { .. }
            | ClientOp::ThreadMetadataUpdate { .. }
    )
}

pub(in crate::server::router) fn dispatch_query_operation<W: Write>(
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match op {
        ClientOp::ThreadRead {
            thread_id,
            include_messages,
            include_turns,
        } => {
            state.reclaim_finished_thread(thread_id);
            run_thread_read(
                state,
                thread_id,
                *include_messages,
                *include_turns,
                id,
                writer,
            )
        }
        ClientOp::ThreadList {
            cursor,
            sort_key,
            sort_direction,
            search_term,
            limit,
            filters,
        } => run_thread_list(
            cursor.as_deref(),
            *limit,
            filters.clone(),
            *sort_key,
            *sort_direction,
            search_term.as_deref(),
            id,
            writer,
        ),
        ClientOp::ThreadSearch {
            query,
            cursor,
            sort_key,
            sort_direction,
            include_archived,
            limit,
        } => run_thread_search(
            query,
            cursor.as_deref(),
            *limit,
            *include_archived,
            *sort_key,
            *sort_direction,
            id,
            writer,
        ),
        ClientOp::ThreadTurnsList {
            thread_id,
            cursor,
            sort_direction,
            items_view,
            limit,
        } => {
            state.reclaim_finished_thread(thread_id);
            run_thread_turns_list(
                state,
                thread_id,
                cursor.as_deref(),
                *limit,
                *sort_direction,
                *items_view,
                id,
                writer,
            )
        }
        ClientOp::ThreadItemsList {
            thread_id,
            turn_id,
            cursor,
            sort_direction,
            limit,
        } => {
            state.reclaim_finished_thread(thread_id);
            run_thread_items_list(
                state,
                thread_id,
                turn_id.as_deref(),
                cursor.as_deref(),
                *limit,
                *sort_direction,
                id,
                writer,
            )
        }
        ClientOp::ThreadMetadataUpdate { thread_id, title } => {
            run_thread_metadata_update(state, thread_id, title.clone(), id, writer)
        }
        _ => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("unsupported thread operation"),
        ),
    }
}

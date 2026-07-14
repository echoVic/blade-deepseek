use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::super::*;

pub(in crate::server::router) fn is_mention_search_operation(op: &ClientOp) -> bool {
    matches!(
        op,
        ClientOp::MentionSearchSessionStart { .. }
            | ClientOp::MentionSearchSessionUpdate { .. }
            | ClientOp::MentionSearchSessionStop { .. }
    )
}

pub(in crate::server::router) fn dispatch_mention_search_operation<W: Write + Send + 'static>(
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    match op {
        ClientOp::MentionSearchSessionStart {
            session_id,
            thread_id,
            exclude,
            respect_gitignore,
            result_limit,
        } => {
            if session_id.is_empty() {
                return write_locked_event(
                    &writer,
                    &id,
                    ServerEvent::error("sessionId must not be empty"),
                );
            }
            if thread_id.is_empty() {
                return write_locked_event(
                    &writer,
                    &id,
                    ServerEvent::error("threadId must not be empty"),
                );
            }
            let Some(thread) = state.threads.thread(thread_id) else {
                return write_locked_event(
                    &writer,
                    &id,
                    ServerEvent::error(format!("unknown thread: {thread_id}")),
                );
            };
            let roots = thread.runtime_workspace_roots().to_vec();
            let mcp_registry = thread.mcp_registry().clone();
            match state.mention_searches.start(
                session_id.clone(),
                roots,
                mcp_registry,
                exclude.clone(),
                *respect_gitignore,
                *result_limit,
                id.clone(),
                Arc::clone(&writer),
            ) {
                Ok(()) => write_locked_event(
                    &writer,
                    &id,
                    ServerEvent::MentionSearchSessionStarted {
                        session_id: json!(session_id),
                        thread_id: json!(thread_id),
                    },
                ),
                Err(error) => write_locked_event(&writer, &id, ServerEvent::error(error)),
            }
        }
        ClientOp::MentionSearchSessionUpdate { session_id, query } => {
            let mut output = writer.lock().map_err(lock_error)?;
            match state.mention_searches.update(session_id, query.clone()) {
                Ok(()) => protocol::write_server_event(
                    &mut *output,
                    &id,
                    ServerEvent::MentionSearchSessionUpdateAccepted {
                        session_id: json!(session_id),
                        query: json!(query),
                    },
                ),
                Err(error) => {
                    protocol::write_server_event(&mut *output, &id, ServerEvent::error(error))
                }
            }
        }
        ClientOp::MentionSearchSessionStop { session_id } => {
            let mut output = writer.lock().map_err(lock_error)?;
            state.mention_searches.stop(session_id);
            protocol::write_server_event(
                &mut *output,
                &id,
                ServerEvent::MentionSearchSessionStopped {
                    session_id: json!(session_id),
                },
            )
        }
        _ => unreachable!("only mention search operations can reach the mention processor"),
    }
}

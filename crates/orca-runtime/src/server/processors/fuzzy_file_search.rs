use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::super::*;

pub(in crate::server::router) fn is_fuzzy_file_search_operation(op: &ClientOp) -> bool {
    matches!(
        op,
        ClientOp::FuzzyFileSearchSessionStart { .. }
            | ClientOp::FuzzyFileSearchSessionUpdate { .. }
            | ClientOp::FuzzyFileSearchSessionStop { .. }
    )
}

pub(in crate::server::router) fn dispatch_fuzzy_file_search_operation<W: Write + Send + 'static>(
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    match op {
        ClientOp::FuzzyFileSearchSessionStart {
            session_id,
            roots,
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
            if roots.is_empty() {
                return write_locked_event(
                    &writer,
                    &id,
                    ServerEvent::error("roots must contain at least one absolute path"),
                );
            }
            match state.fuzzy_file_searches.start(
                session_id.clone(),
                roots.clone(),
                exclude.clone(),
                *respect_gitignore,
                *result_limit,
                id.clone(),
                Arc::clone(&writer),
            ) {
                Ok(()) => write_locked_event(
                    &writer,
                    &id,
                    ServerEvent::FuzzyFileSearchSessionStarted {
                        session_id: json!(session_id),
                    },
                ),
                Err(error) => write_locked_event(&writer, &id, ServerEvent::error(error)),
            }
        }
        ClientOp::FuzzyFileSearchSessionUpdate { session_id, query } => {
            let mut output = writer.lock().map_err(lock_error)?;
            match state.fuzzy_file_searches.update(session_id, query.clone()) {
                Ok(()) => protocol::write_server_event(
                    &mut *output,
                    &id,
                    ServerEvent::FuzzyFileSearchSessionUpdateAccepted {
                        session_id: json!(session_id),
                        query: json!(query),
                    },
                ),
                Err(error) => {
                    protocol::write_server_event(&mut *output, &id, ServerEvent::error(error))
                }
            }
        }
        ClientOp::FuzzyFileSearchSessionStop { session_id } => {
            let mut output = writer.lock().map_err(lock_error)?;
            state.fuzzy_file_searches.stop(session_id);
            protocol::write_server_event(
                &mut *output,
                &id,
                ServerEvent::FuzzyFileSearchSessionStopped {
                    session_id: json!(session_id),
                },
            )
        }
        _ => unreachable!("only fuzzy file search operations can reach the search processor"),
    }
}

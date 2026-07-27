use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use super::lock_error;
use super::opaque_permission_router::{
    JsonlCommittedReplay, JsonlConnectionAdmission, JsonlLiveRequestAdmission,
    JsonlRequestTombstone, JsonlResponseDigest, JsonlRetiredRequestOwner,
    JsonlRetiredRequestSettlement,
};
use crate::unstable_surface::DeferredMutation;
use crate::unstable_surface::{RuntimeSurfaceClientHandle, SurfaceInteractionId};

#[derive(Clone)]
pub(super) enum JsonlDirectInteractionRoute {
    UserInput {
        client: RuntimeSurfaceClientHandle,
        interaction_id: SurfaceInteractionId,
    },
    McpElicitation {
        client: RuntimeSurfaceClientHandle,
        interaction_id: SurfaceInteractionId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JsonlDirectInteractionKind {
    UserInput,
    McpElicitation,
}

#[derive(Clone)]
pub(super) struct JsonlDirectInteractionAdapter<T> {
    admission: JsonlConnectionAdmission,
    routes: Arc<Mutex<HashMap<String, JsonlDirectInteractionEntry<T>>>>,
}

struct JsonlDirectInteractionEntry<T> {
    admission: Option<JsonlLiveRequestAdmission>,
    kind: JsonlDirectInteractionKind,
    publication: JsonlDirectPublicationState,
    state: JsonlDirectInteractionState,
    route: T,
}

#[derive(Clone, Copy)]
enum JsonlDirectPublicationState {
    Registered,
    Writing { frame_digest: JsonlResponseDigest },
    Published { frame_digest: JsonlResponseDigest },
}

#[derive(Clone)]
enum JsonlDirectInteractionState {
    Routed,
    CommittedPending {
        request_id: crate::unstable_surface::SurfaceRequestId,
        commit_id: crate::unstable_surface::SurfaceCommitId,
        response_digest: JsonlResponseDigest,
    },
}

impl<T: Clone> JsonlDirectInteractionAdapter<T> {
    pub(super) fn new(admission: JsonlConnectionAdmission) -> Self {
        Self {
            admission,
            routes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) fn register(
        &self,
        preferred_request_id: String,
        kind: JsonlDirectInteractionKind,
        route: T,
    ) -> io::Result<String> {
        let owner = match kind {
            JsonlDirectInteractionKind::UserInput => JsonlRetiredRequestOwner::DirectUserInput,
            JsonlDirectInteractionKind::McpElicitation => {
                JsonlRetiredRequestOwner::DirectMcpElicitation
            }
        };
        let admission = self
            .admission
            .register(&preferred_request_id, owner)
            .map_err(|reason| {
                io::Error::other(format!(
                    "JSONL direct interaction admission failed: {reason:?}"
                ))
            })?;
        let request_id = admission.opaque_request_id.clone();
        let mut routes = self.routes.lock().map_err(lock_error)?;
        if routes
            .insert(
                request_id.clone(),
                JsonlDirectInteractionEntry {
                    admission: Some(admission),
                    kind,
                    publication: JsonlDirectPublicationState::Registered,
                    state: JsonlDirectInteractionState::Routed,
                    route,
                },
            )
            .is_some()
        {
            return Err(io::Error::other("JSONL direct interaction route collision"));
        }
        Ok(request_id)
    }

    pub(super) fn route(
        &self,
        request_id: &str,
        expected_kind: JsonlDirectInteractionKind,
    ) -> io::Result<Option<T>> {
        if self.admission.tombstone(request_id)?.is_some() {
            return Ok(None);
        }
        Ok(self
            .routes
            .lock()
            .map_err(lock_error)?
            .get(request_id)
            .filter(|entry| entry.kind == expected_kind)
            .map(|entry| entry.route.clone()))
    }

    pub(super) fn published_route(
        &self,
        request_id: &str,
        expected_kind: JsonlDirectInteractionKind,
    ) -> io::Result<Option<T>> {
        if self.admission.tombstone(request_id)?.is_some() {
            return Ok(None);
        }
        Ok(self
            .routes
            .lock()
            .map_err(lock_error)?
            .get(request_id)
            .filter(|entry| {
                entry.kind == expected_kind
                    && matches!(
                        entry.publication,
                        JsonlDirectPublicationState::Published { .. }
                    )
            })
            .map(|entry| entry.route.clone()))
    }

    pub(super) fn mark_writing(
        &self,
        request_id: &str,
        frame_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        let mut routes = self.routes.lock().map_err(lock_error)?;
        let entry = routes
            .get_mut(request_id)
            .ok_or_else(|| io::Error::other("JSONL direct route is no longer live"))?;
        match entry.publication {
            JsonlDirectPublicationState::Registered => {
                entry.publication = JsonlDirectPublicationState::Writing { frame_digest };
                Ok(())
            }
            JsonlDirectPublicationState::Writing {
                frame_digest: existing,
            } if existing == frame_digest => Ok(()),
            JsonlDirectPublicationState::Writing { .. }
            | JsonlDirectPublicationState::Published { .. } => Err(io::Error::other(
                "JSONL direct frame entered writing with a different digest",
            )),
        }
    }

    pub(super) fn mark_published(
        &self,
        request_id: &str,
        frame_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        let mut routes = self.routes.lock().map_err(lock_error)?;
        let entry = routes
            .get_mut(request_id)
            .ok_or_else(|| io::Error::other("JSONL direct route is no longer live"))?;
        match entry.publication {
            JsonlDirectPublicationState::Writing {
                frame_digest: existing,
            } if existing == frame_digest => {
                entry.publication = JsonlDirectPublicationState::Published { frame_digest };
                Ok(())
            }
            JsonlDirectPublicationState::Published {
                frame_digest: existing,
            } if existing == frame_digest => Ok(()),
            JsonlDirectPublicationState::Registered
            | JsonlDirectPublicationState::Writing { .. }
            | JsonlDirectPublicationState::Published { .. } => Err(io::Error::other(
                "JSONL direct frame publication has no matching writing witness",
            )),
        }
    }

    pub(super) fn settle_committed(
        &self,
        request_id: &str,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<Option<JsonlRequestTombstone>> {
        let entry = self.routes.lock().map_err(lock_error)?.remove(request_id);
        let Some(mut entry) = entry else {
            return Ok(self.admission.tombstone(request_id)?);
        };
        let admission = entry
            .admission
            .take()
            .ok_or_else(|| io::Error::other("JSONL direct admission already consumed"))?;
        self.admission
            .retire(
                admission,
                JsonlRetiredRequestSettlement::DirectInteractionCommitted { response_digest },
            )
            .map(Some)
    }

    pub(super) fn mark_committed_pending(
        &self,
        request_id: &str,
        mutation: &DeferredMutation,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        self.mark_committed_pending_witness(
            request_id,
            mutation.request_id.clone(),
            mutation.commit_id.clone(),
            response_digest,
        )
    }

    fn mark_committed_pending_witness(
        &self,
        request_id: &str,
        mutation_request_id: crate::unstable_surface::SurfaceRequestId,
        commit_id: crate::unstable_surface::SurfaceCommitId,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        let mut routes = self.routes.lock().map_err(lock_error)?;
        let entry = routes
            .get_mut(request_id)
            .ok_or_else(|| io::Error::other("JSONL direct interaction route is no longer live"))?;
        match &entry.state {
            JsonlDirectInteractionState::Routed => {
                entry.state = JsonlDirectInteractionState::CommittedPending {
                    request_id: mutation_request_id,
                    commit_id,
                    response_digest,
                };
                Ok(())
            }
            JsonlDirectInteractionState::CommittedPending {
                request_id: existing_request_id,
                commit_id: existing_commit_id,
                response_digest: existing_response_digest,
            } if existing_request_id == &mutation_request_id
                && existing_commit_id == &commit_id
                && existing_response_digest == &response_digest =>
            {
                Ok(())
            }
            JsonlDirectInteractionState::CommittedPending { .. } => Err(io::Error::other(
                "JSONL direct interaction route has a different committed repair witness",
            )),
        }
    }

    pub(super) fn close_routes(
        &self,
        owner_settlement: super::opaque_permission_router::JsonlOwnerSettlement,
    ) -> io::Result<Vec<JsonlRequestTombstone>> {
        let request_ids = self
            .routes
            .lock()
            .map_err(lock_error)?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut tombstones = Vec::with_capacity(request_ids.len());
        for request_id in request_ids {
            let entry = {
                let mut routes = self.routes.lock().map_err(lock_error)?;
                if routes.get(&request_id).is_some_and(|entry| {
                    matches!(
                        entry.state,
                        JsonlDirectInteractionState::CommittedPending { .. }
                    )
                }) {
                    None
                } else {
                    routes.remove(&request_id)
                }
            };
            let Some(mut entry) = entry else {
                continue;
            };
            let admission = entry
                .admission
                .take()
                .ok_or_else(|| io::Error::other("JSONL direct admission already consumed"))?;
            tombstones.push(self.admission.retire(
                admission,
                JsonlRetiredRequestSettlement::TransportRetired {
                    owner_settlement: owner_settlement.clone(),
                },
            )?);
        }
        Ok(tombstones)
    }

    pub(super) fn committed_replay(
        &self,
        request_id: &str,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<JsonlCommittedReplay> {
        let Some(tombstone) = self.admission.tombstone(request_id)? else {
            return Ok(JsonlCommittedReplay::NotCommitted);
        };
        let committed_digest = match tombstone.settlement {
            JsonlRetiredRequestSettlement::DirectInteractionCommitted { response_digest } => {
                response_digest
            }
            JsonlRetiredRequestSettlement::PermissionCommitted { .. }
            | JsonlRetiredRequestSettlement::TransportRetired { .. } => {
                return Ok(JsonlCommittedReplay::NotCommitted);
            }
        };
        Ok(if committed_digest == response_digest {
            JsonlCommittedReplay::SameResponse
        } else {
            JsonlCommittedReplay::ConflictingResponse
        })
    }

    pub(super) fn settle_committed_pending(&self) -> io::Result<Vec<JsonlRequestTombstone>> {
        let request_ids = self
            .routes
            .lock()
            .map_err(lock_error)?
            .iter()
            .filter(|(_, entry)| {
                matches!(
                    entry.state,
                    JsonlDirectInteractionState::CommittedPending { .. }
                )
            })
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>();
        let mut tombstones = Vec::with_capacity(request_ids.len());
        for request_id in request_ids {
            let response_digest = {
                let routes = self.routes.lock().map_err(lock_error)?;
                let entry = routes
                    .get(&request_id)
                    .ok_or_else(|| io::Error::other("JSONL committed direct route disappeared"))?;
                match entry.state {
                    JsonlDirectInteractionState::CommittedPending {
                        response_digest, ..
                    } => response_digest,
                    JsonlDirectInteractionState::Routed => {
                        return Err(io::Error::other(
                            "JSONL routed direct interaction entered committed settlement",
                        ));
                    }
                }
            };
            if let Some(tombstone) = self.settle_committed(&request_id, response_digest)? {
                tombstones.push(tombstone);
            }
        }
        Ok(tombstones)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::opaque_permission_router::JsonlOwnerSettlement;

    fn admission() -> JsonlConnectionAdmission {
        JsonlConnectionAdmission::new(
            crate::unstable_surface::SurfaceConnectionId::try_from_bytes([
                1, 159, 161, 19, 220, 41, 112, 211, 145, 70, 17, 0, 120, 212, 79, 247,
            ])
            .unwrap(),
        )
    }

    #[test]
    fn committed_pending_direct_interaction_is_never_transport_retired() {
        let adapter = JsonlDirectInteractionAdapter::new(admission());
        adapter
            .register(
                "user-input".to_string(),
                JsonlDirectInteractionKind::UserInput,
                "route".to_string(),
            )
            .unwrap();
        adapter
            .mark_committed_pending_witness(
                "user-input",
                crate::unstable_surface::SurfaceRequestId::new(),
                crate::unstable_surface::SurfaceCommitId::try_from_bytes([
                    1, 159, 161, 19, 220, 41, 112, 211, 145, 70, 17, 0, 120, 212, 79, 248,
                ])
                .unwrap(),
                crate::server::opaque_permission_router::jsonl_response_digest(&"cancel").unwrap(),
            )
            .unwrap();

        assert!(
            adapter
                .close_routes(JsonlOwnerSettlement::InteractionRecoveryRetained)
                .unwrap()
                .is_empty()
        );
        let tombstones = adapter.settle_committed_pending().unwrap();
        assert_eq!(tombstones.len(), 1);
        assert_eq!(
            tombstones[0].settlement,
            JsonlRetiredRequestSettlement::DirectInteractionCommitted {
                response_digest: crate::server::opaque_permission_router::jsonl_response_digest(
                    &"cancel"
                )
                .unwrap(),
            }
        );
        assert_eq!(
            adapter
                .committed_replay(
                    "user-input",
                    crate::server::opaque_permission_router::jsonl_response_digest(&"cancel")
                        .unwrap(),
                )
                .unwrap(),
            JsonlCommittedReplay::SameResponse
        );
        assert_eq!(
            adapter
                .committed_replay(
                    "user-input",
                    crate::server::opaque_permission_router::jsonl_response_digest(&"answer")
                        .unwrap(),
                )
                .unwrap(),
            JsonlCommittedReplay::ConflictingResponse
        );
    }

    #[test]
    fn direct_route_is_kind_bound_and_requires_physical_publication() {
        let adapter = JsonlDirectInteractionAdapter::new(admission());
        adapter
            .register(
                "user-input".to_string(),
                JsonlDirectInteractionKind::UserInput,
                "route".to_string(),
            )
            .unwrap();
        let digest =
            crate::server::opaque_permission_router::jsonl_response_digest(&"frame").unwrap();
        assert!(
            adapter
                .published_route("user-input", JsonlDirectInteractionKind::UserInput)
                .unwrap()
                .is_none()
        );
        adapter.mark_writing("user-input", digest).unwrap();
        adapter.mark_published("user-input", digest).unwrap();
        assert_eq!(
            adapter
                .published_route("user-input", JsonlDirectInteractionKind::UserInput)
                .unwrap()
                .as_deref(),
            Some("route")
        );
        assert!(
            adapter
                .published_route("user-input", JsonlDirectInteractionKind::McpElicitation)
                .unwrap()
                .is_none()
        );
    }
}

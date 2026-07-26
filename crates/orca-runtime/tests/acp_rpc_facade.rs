#[path = "../src/acp/rpc_facade.rs"]
mod rpc_facade;

use std::cell::Cell;
use std::future::pending;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use rpc_facade::{
    ACP_INGRESS_BYTE_LIMIT, ACP_INGRESS_MESSAGE_LIMIT, ACP_MAX_INBOUND_LINE_BYTES,
    ACP_MAX_OUTBOUND_FRAME_BYTES, ACP_OUTGOING_BYTE_LIMIT, ACP_OUTGOING_MESSAGE_LIMIT,
    ACP_SUPERVISOR_JOIN_DEADLINE_MS, BoundedLaneBudget, FrameDirection, HandlerCompletion,
    HandlerFuture, InboundFrame, LaneKind, OutboundReservationBarrier, ReaderAdmissionBarrier,
    RpcFacadeConfig, RpcFacadeError, SequenceScope, SequenceSeeds, TransportFrame, bounded_lane,
    spawn_local_rpc_facade, spawn_local_rpc_facade_with_response_session_resolver,
    spawn_rpc_facade, spawn_rpc_facade_with_sequence_seeds,
    spawn_rpc_facade_with_sequence_seeds_and_outbound_reservation_barrier,
    spawn_rpc_facade_with_sequence_seeds_and_reader_admission_barrier,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::oneshot;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn local_facade_sequences_non_send_session_admission_before_cancel() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async {
        let seen = Rc::new(Cell::new(0_u64));
        let admitted = Rc::new(tokio::sync::Notify::new());
        let handler = {
            let seen = seen.clone();
            let admitted = admitted.clone();
            Rc::new(move |frame: InboundFrame| {
                let seen = seen.clone();
                let admitted = admitted.clone();
                Box::pin(async move {
                    if frame.method() == Some("session/prompt") {
                        tokio::task::yield_now().await;
                        assert_eq!(seen.get(), 0);
                        seen.set(1);
                    } else if frame.method() == Some("session/cancel") {
                        assert_eq!(seen.get(), 1);
                        seen.set(2);
                        admitted.notify_one();
                    }
                    Ok(Box::pin(async {}) as rpc_facade::LocalHandlerCompletion)
                }) as rpc_facade::LocalHandlerFuture
            })
        };
        let (mut client, server) = tokio::io::duplex(4096);
        let (server_read, _server_write) = tokio::io::split(server);
        let (_handle, supervisor) = spawn_local_rpc_facade(
            server_read,
            RecordingWriter::short(usize::MAX),
            handler,
            RpcFacadeConfig::default(),
        );
        client
            .write_all(&request("session/prompt", 1, "local-session"))
            .await
            .unwrap();
        client
            .write_all(&request("session/cancel", 2, "local-session"))
            .await
            .unwrap();
        admitted.notified().await;
        client.shutdown().await.unwrap();
        supervisor.wait().await.unwrap();
        assert_eq!(seen.get(), 2);
    });
}

#[test]
fn local_facade_orders_cancel_before_correlated_permission_response() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async {
        let seen = Rc::new(Cell::new(0_u64));
        let response_seen = Rc::new(tokio::sync::Notify::new());
        let handler = {
            let seen = seen.clone();
            let response_seen = response_seen.clone();
            Rc::new(move |frame: InboundFrame| {
                let seen = seen.clone();
                let response_seen = response_seen.clone();
                Box::pin(async move {
                    if frame.method() == Some("session/cancel") {
                        tokio::task::yield_now().await;
                        assert_eq!(seen.get(), 0);
                        seen.set(1);
                    } else if frame.method().is_none() {
                        assert_eq!(
                            seen.get(),
                            1,
                            "permission response overtook earlier session cancel"
                        );
                        seen.set(2);
                        response_seen.notify_one();
                    }
                    Ok(Box::pin(async {}) as rpc_facade::LocalHandlerCompletion)
                }) as rpc_facade::LocalHandlerFuture
            })
        };
        let resolver = Arc::new(|request_id: i64| {
            (request_id == 41).then(|| "permission-session".to_string())
        });
        let (mut client, server) = tokio::io::duplex(4096);
        let (server_read, _server_write) = tokio::io::split(server);
        let (_handle, supervisor) =
            spawn_local_rpc_facade_with_response_session_resolver(
                server_read,
                RecordingWriter::short(usize::MAX),
                handler,
                resolver,
                RpcFacadeConfig::default(),
            );
        client
            .write_all(&request(
                "session/cancel",
                1,
                "permission-session",
            ))
            .await
            .unwrap();
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":41,\"result\":{\"outcome\":{\"outcome\":\"cancelled\"}}}\n")
            .await
            .unwrap();
        response_seen.notified().await;
        client.shutdown().await.unwrap();
        supervisor.wait().await.unwrap();
        assert_eq!(seen.get(), 2);
    });
}

fn request(method: &str, id: u64, session_id: &str) -> Vec<u8> {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{{\"sessionId\":\"{session_id}\"}}}}\n"
    )
    .into_bytes()
}

fn response(id: u64, value: &str) -> Vec<u8> {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":\"{value}\"}}\n").into_bytes()
}

fn request_with_encoded_len(target: usize, id: u64, session_id: &str) -> Vec<u8> {
    let prefix = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{session_id}\",\"padding\":\""
    );
    let suffix = "\"}}\n";
    assert!(prefix.len() + suffix.len() <= target);
    let mut encoded = Vec::with_capacity(target);
    encoded.extend_from_slice(prefix.as_bytes());
    encoded.resize(target - suffix.len(), b'x');
    encoded.extend_from_slice(suffix.as_bytes());
    assert_eq!(encoded.len(), target);
    encoded
}

fn pending_input() -> (DuplexStream, DuplexStream) {
    tokio::io::duplex(1024)
}

fn immediate_handler() -> Arc<dyn Fn(InboundFrame) -> HandlerFuture + Send + Sync> {
    Arc::new(|_| Box::pin(async { Ok(Box::pin(async {}) as HandlerCompletion) }) as HandlerFuture)
}

fn recording_handler(
    seen: Arc<Mutex<Vec<(u64, u64, String, String)>>>,
    expected: usize,
    admissions_done: Arc<tokio::sync::Notify>,
) -> Arc<dyn Fn(InboundFrame) -> HandlerFuture + Send + Sync> {
    Arc::new(move |frame| {
        let seen = seen.clone();
        let admissions_done = admissions_done.clone();
        Box::pin(async move {
            if frame.method() == Some("session/prompt") {
                // Make the later cancel runnable first. The per-session gate must
                // still keep its handler behind this earlier prompt.
                for _ in 0..32 {
                    tokio::task::yield_now().await;
                }
            }
            let mut seen = seen.lock().unwrap();
            seen.push((
                frame.sequence(),
                frame.session_sequence().unwrap(),
                frame.session_id().unwrap().to_owned(),
                frame.method().unwrap().to_owned(),
            ));
            if seen.len() == expected {
                admissions_done.notify_one();
            }
            drop(seen);
            Ok(Box::pin(async {}) as HandlerCompletion)
        })
    })
}

#[tokio::test]
async fn same_session_prompt_precedes_later_cancel_when_handlers_reverse_schedule() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let admissions_done = Arc::new(tokio::sync::Notify::new());
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let (_handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::short(usize::MAX),
        recording_handler(seen.clone(), 2, admissions_done.clone()),
        RpcFacadeConfig::default(),
    );

    client
        .write_all(&request("session/prompt", 1, "session-a"))
        .await
        .unwrap();
    client
        .write_all(&request("session/cancel", 2, "session-a"))
        .await
        .unwrap();
    admissions_done.notified().await;
    client.shutdown().await.unwrap();

    let report = tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
        .await
        .expect("supervisor hung")
        .expect("clean EOF");
    assert!(report.reader_joined && report.scheduler_joined && report.writer_joined);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (0, 0, "session-a".to_owned(), "session/prompt".to_owned()),
            (1, 1, "session-a".to_owned(), "session/cancel".to_owned()),
        ]
    );
}

#[tokio::test]
async fn cancel_admission_runs_after_prompt_admission_while_prompt_completion_is_pending() {
    let prompt_admitted = Arc::new(tokio::sync::Notify::new());
    let cancel_admitted = Arc::new(tokio::sync::Notify::new());
    let release_prompt_completion = Arc::new(tokio::sync::Notify::new());
    let handler = {
        let prompt_admitted = prompt_admitted.clone();
        let cancel_admitted = cancel_admitted.clone();
        let release_prompt_completion = release_prompt_completion.clone();
        Arc::new(move |frame: InboundFrame| {
            let prompt_admitted = prompt_admitted.clone();
            let cancel_admitted = cancel_admitted.clone();
            let release_prompt_completion = release_prompt_completion.clone();
            Box::pin(async move {
                match frame.method() {
                    Some("session/prompt") => {
                        prompt_admitted.notify_one();
                        Ok(Box::pin(async move {
                            release_prompt_completion.notified().await;
                        }) as HandlerCompletion)
                    }
                    Some("session/cancel") => {
                        cancel_admitted.notify_one();
                        Ok(Box::pin(async {}) as HandlerCompletion)
                    }
                    method => panic!("unexpected method: {method:?}"),
                }
            }) as HandlerFuture
        })
    };
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let (_handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::short(usize::MAX),
        handler,
        RpcFacadeConfig::default(),
    );

    client
        .write_all(&request("session/prompt", 50, "session-a"))
        .await
        .unwrap();
    prompt_admitted.notified().await;
    client
        .write_all(&request("session/cancel", 51, "session-a"))
        .await
        .unwrap();
    tokio::time::timeout(TEST_TIMEOUT, cancel_admitted.notified())
        .await
        .expect("cancel admission was blocked by prompt completion");

    release_prompt_completion.notify_waiters();
    client.shutdown().await.unwrap();
    supervisor.wait().await.unwrap();
}

#[tokio::test]
async fn inbound_lane_enforces_message_and_aggregate_byte_limits_and_releases_budget() {
    assert_eq!(ACP_INGRESS_MESSAGE_LIMIT, 64);
    assert_eq!(ACP_INGRESS_BYTE_LIMIT, 16_777_216);

    let (messages, mut message_rx) = bounded_lane(
        LaneKind::Ingress,
        ACP_INGRESS_MESSAGE_LIMIT,
        ACP_INGRESS_BYTE_LIMIT,
    );
    for value in 0..ACP_INGRESS_MESSAGE_LIMIT {
        messages.try_send(value, 1).unwrap();
    }
    assert!(matches!(
        messages.try_send(ACP_INGRESS_MESSAGE_LIMIT, 1),
        Err(RpcFacadeError::Capacity {
            lane: LaneKind::Ingress,
            budget: BoundedLaneBudget::Messages,
        })
    ));
    assert_eq!(message_rx.recv().await.unwrap(), 0);
    messages.try_send(ACP_INGRESS_MESSAGE_LIMIT, 1).unwrap();

    let (bytes, mut byte_rx) = bounded_lane(LaneKind::Ingress, 2, ACP_INGRESS_BYTE_LIMIT);
    bytes.try_send("first", ACP_INGRESS_BYTE_LIMIT).unwrap();
    assert!(matches!(
        bytes.try_send("second", 1),
        Err(RpcFacadeError::Capacity {
            lane: LaneKind::Ingress,
            budget: BoundedLaneBudget::Bytes,
        })
    ));
    assert_eq!(byte_rx.recv().await.unwrap(), "first");
    bytes.try_send("second", 1).unwrap();
}

#[tokio::test]
async fn outbound_lane_enforces_message_and_aggregate_byte_limits_and_releases_budget() {
    assert_eq!(ACP_OUTGOING_MESSAGE_LIMIT, 256);
    assert_eq!(ACP_OUTGOING_BYTE_LIMIT, 33_554_432);

    let (messages, mut message_rx) = bounded_lane(
        LaneKind::Outgoing,
        ACP_OUTGOING_MESSAGE_LIMIT,
        ACP_OUTGOING_BYTE_LIMIT,
    );
    for value in 0..ACP_OUTGOING_MESSAGE_LIMIT {
        messages.try_send(value, 1).unwrap();
    }
    assert!(matches!(
        messages.try_send(ACP_OUTGOING_MESSAGE_LIMIT, 1),
        Err(RpcFacadeError::Capacity {
            lane: LaneKind::Outgoing,
            budget: BoundedLaneBudget::Messages,
        })
    ));
    assert_eq!(message_rx.recv().await.unwrap(), 0);
    messages.try_send(ACP_OUTGOING_MESSAGE_LIMIT, 1).unwrap();

    let (bytes, mut byte_rx) = bounded_lane(LaneKind::Outgoing, 2, ACP_OUTGOING_BYTE_LIMIT);
    bytes.try_send("first", ACP_OUTGOING_BYTE_LIMIT).unwrap();
    assert!(matches!(
        bytes.try_send("second", 1),
        Err(RpcFacadeError::Capacity {
            lane: LaneKind::Outgoing,
            budget: BoundedLaneBudget::Bytes,
        })
    ));
    assert_eq!(byte_rx.recv().await.unwrap(), "first");
    bytes.try_send("second", 1).unwrap();
}

#[tokio::test]
async fn stalled_handler_completions_hold_the_64_message_ingress_budget() {
    let admissions = Arc::new(AtomicUsize::new(0));
    let all_admitted = Arc::new(tokio::sync::Notify::new());
    let handler = {
        let admissions = admissions.clone();
        let all_admitted = all_admitted.clone();
        Arc::new(move |_| {
            let admissions = admissions.clone();
            let all_admitted = all_admitted.clone();
            Box::pin(async move {
                if admissions.fetch_add(1, Ordering::AcqRel) + 1 == ACP_INGRESS_MESSAGE_LIMIT {
                    all_admitted.notify_one();
                }
                Ok(Box::pin(pending()) as HandlerCompletion)
            }) as HandlerFuture
        })
    };
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, _server_write) = tokio::io::split(server);
    let (_handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::short(usize::MAX),
        handler,
        RpcFacadeConfig::default(),
    );
    for id in 0..ACP_INGRESS_MESSAGE_LIMIT {
        client
            .write_all(&request("session/prompt", id as u64, "session-a"))
            .await
            .unwrap();
    }
    all_admitted.notified().await;
    client
        .write_all(&request(
            "session/cancel",
            ACP_INGRESS_MESSAGE_LIMIT as u64,
            "session-a",
        ))
        .await
        .unwrap();

    assert!(matches!(
        tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
            .await
            .expect("65th frame did not seal ingress"),
        Err(RpcFacadeError::Capacity {
            lane: LaneKind::Ingress,
            budget: BoundedLaneBudget::Messages,
        })
    ));
}

#[tokio::test]
async fn stalled_handler_completions_hold_the_16_mib_ingress_byte_budget() {
    let admissions = Arc::new(AtomicUsize::new(0));
    let all_admitted = Arc::new(tokio::sync::Notify::new());
    let handler = {
        let admissions = admissions.clone();
        let all_admitted = all_admitted.clone();
        Arc::new(move |_| {
            let admissions = admissions.clone();
            let all_admitted = all_admitted.clone();
            Box::pin(async move {
                if admissions.fetch_add(1, Ordering::AcqRel) + 1 == 2 {
                    all_admitted.notify_one();
                }
                Ok(Box::pin(pending()) as HandlerCompletion)
            }) as HandlerFuture
        })
    };
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, _server_write) = tokio::io::split(server);
    let (_handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::short(usize::MAX),
        handler,
        RpcFacadeConfig::default(),
    );
    for id in 0..2 {
        client
            .write_all(&request_with_encoded_len(
                ACP_MAX_INBOUND_LINE_BYTES,
                id,
                "session-a",
            ))
            .await
            .unwrap();
    }
    all_admitted.notified().await;
    client
        .write_all(&request("session/cancel", 2, "session-a"))
        .await
        .unwrap();

    assert!(matches!(
        tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
            .await
            .expect("byte-over-limit frame did not seal ingress"),
        Err(RpcFacadeError::Capacity {
            lane: LaneKind::Ingress,
            budget: BoundedLaneBudget::Bytes,
        })
    ));
}

#[tokio::test]
async fn outbound_capacity_rejection_seals_transport_instead_of_dropping_frame() {
    let (_client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let writer = RecordingWriter::pending();
    let writer_state = writer.state.clone();
    let (handle, supervisor) = spawn_rpc_facade(
        server_read,
        writer,
        immediate_handler(),
        RpcFacadeConfig::default(),
    );
    let mut receipts = Vec::new();
    receipts.push(
        handle
            .enqueue(TransportFrame::new(
                FrameDirection::AgentToClient,
                response(0, "pending"),
            ))
            .unwrap(),
    );
    for _ in 0..128 {
        if writer_state.lock().unwrap().write_calls > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(writer_state.lock().unwrap().write_calls > 0);
    for id in 1..ACP_OUTGOING_MESSAGE_LIMIT {
        receipts.push(
            handle
                .enqueue(TransportFrame::new(
                    FrameDirection::AgentToClient,
                    response(id as u64, "queued"),
                ))
                .unwrap(),
        );
    }
    assert!(matches!(
        handle.enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(ACP_OUTGOING_MESSAGE_LIMIT as u64, "over-limit"),
        )),
        Err(RpcFacadeError::Capacity {
            lane: LaneKind::Outgoing,
            budget: BoundedLaneBudget::Messages,
        })
    ));
    assert!(matches!(
        handle.enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(ACP_OUTGOING_MESSAGE_LIMIT as u64 + 1, "sealed"),
        )),
        Err(RpcFacadeError::Sealed)
    ));
    drop(receipts);
    assert!(matches!(
        tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
            .await
            .expect("outbound saturation did not wake supervisor"),
        Err(RpcFacadeError::Capacity {
            lane: LaneKind::Outgoing,
            budget: BoundedLaneBudget::Messages,
        })
    ));
}

#[tokio::test]
async fn writer_uses_write_all_across_short_writes_then_flushes_before_ack() {
    let (client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let writer = RecordingWriter::short(3);
    let state = writer.state.clone();
    let (handle, supervisor) = spawn_rpc_facade(
        server_read,
        writer,
        immediate_handler(),
        RpcFacadeConfig::default(),
    );

    let expected = response(7, "short-write");
    let receipt = handle
        .enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            expected.clone(),
        ))
        .unwrap();
    assert_eq!(receipt.sequence(), 0);
    let ack = tokio::time::timeout(TEST_TIMEOUT, receipt.ack())
        .await
        .expect("writer hung")
        .expect("write acknowledged");
    assert_eq!(ack.sequence, 0);
    assert_eq!(ack.encoded_bytes, expected.len());
    let state = state.lock().unwrap();
    assert_eq!(state.bytes, expected);
    assert!(state.write_calls > 1, "test did not exercise short writes");
    assert_eq!(state.flush_calls, 1);
    drop(state);

    drop(client);
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn write_all_failure_is_returned_to_the_correlated_receipt() {
    let (_client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let (handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::fail_write_after(5),
        immediate_handler(),
        RpcFacadeConfig::default(),
    );

    let receipt = handle
        .enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(9, "write-fails"),
        ))
        .unwrap();
    assert!(matches!(
        receipt.ack().await,
        Err(RpcFacadeError::Write { sequence: 0, .. })
    ));
    assert!(supervisor.wait().await.is_err());
}

#[tokio::test]
async fn flush_failure_is_returned_to_the_correlated_receipt() {
    let (_client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let (handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::fail_flush(),
        immediate_handler(),
        RpcFacadeConfig::default(),
    );

    let receipt = handle
        .enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(10, "flush-fails"),
        ))
        .unwrap();
    assert!(matches!(
        receipt.ack().await,
        Err(RpcFacadeError::Flush { sequence: 0, .. })
    ));
    assert!(supervisor.wait().await.is_err());
}

#[tokio::test]
async fn permanently_pending_writer_is_bounded_by_the_injected_deadline() {
    let (_client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let config = RpcFacadeConfig {
        write_flush_deadline: Duration::from_millis(20),
        supervisor_join_deadline: Duration::from_millis(100),
    };
    let (handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::pending(),
        immediate_handler(),
        config,
    );

    let receipt = handle
        .enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(11, "pending"),
        ))
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(TEST_TIMEOUT, receipt.ack()).await,
        Ok(Err(RpcFacadeError::Timeout {
            sequence: Some(0),
            ..
        }))
    ));
    assert!(
        tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
            .await
            .expect("supervisor hung")
            .is_err()
    );
}

#[tokio::test]
async fn inbound_frame_above_limit_is_rejected_and_seals_ingress() {
    assert_eq!(ACP_MAX_INBOUND_LINE_BYTES, 8_388_608);
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, _server_write) = tokio::io::split(server);
    let (handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::short(usize::MAX),
        immediate_handler(),
        RpcFacadeConfig::default(),
    );
    let write = tokio::spawn(async move {
        client
            .write_all(&vec![b'x'; ACP_MAX_INBOUND_LINE_BYTES + 1])
            .await
            .unwrap();
        client.shutdown().await.unwrap();
    });

    assert!(matches!(
        tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
            .await
            .expect("supervisor hung"),
        Err(RpcFacadeError::Oversize {
            direction: FrameDirection::ClientToAgent,
            ..
        })
    ));
    write.await.unwrap();
    assert!(matches!(
        handle.enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(12, "sealed")
        )),
        Err(RpcFacadeError::Sealed)
    ));
}

#[tokio::test]
async fn inbound_frame_limit_stops_an_unbounded_line_without_waiting_for_eof() {
    let (_handle, supervisor) = spawn_rpc_facade(
        tokio::io::repeat(b'x'),
        RecordingWriter::short(usize::MAX),
        immediate_handler(),
        RpcFacadeConfig::default(),
    );

    assert!(matches!(
        tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
            .await
            .expect("unbounded line was not cut off"),
        Err(RpcFacadeError::Oversize {
            direction: FrameDirection::ClientToAgent,
            ..
        })
    ));
}

#[tokio::test]
async fn outbound_frame_above_limit_seals_transport_instead_of_dropping_frame() {
    assert_eq!(ACP_MAX_OUTBOUND_FRAME_BYTES, 8_388_608);
    let (_client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let (handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::short(usize::MAX),
        immediate_handler(),
        RpcFacadeConfig::default(),
    );

    assert!(matches!(
        handle.enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            vec![b'x'; ACP_MAX_OUTBOUND_FRAME_BYTES + 1]
        )),
        Err(RpcFacadeError::Oversize {
            direction: FrameDirection::AgentToClient,
            ..
        })
    ));
    assert!(matches!(
        handle.enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(13, "sealed")
        )),
        Err(RpcFacadeError::Sealed)
    ));
    assert!(matches!(
        tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
            .await
            .expect("oversize outbound frame did not seal transport"),
        Err(RpcFacadeError::Oversize {
            direction: FrameDirection::AgentToClient,
            ..
        })
    ));
}

#[tokio::test]
async fn outbound_direction_is_validated_before_queueing() {
    let (_client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let (handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::short(usize::MAX),
        immediate_handler(),
        RpcFacadeConfig::default(),
    );

    assert!(matches!(
        handle.enqueue(TransportFrame::new(
            FrameDirection::ClientToAgent,
            request("session/prompt", 13, "session-a")
        )),
        Err(RpcFacadeError::Direction {
            expected: FrameDirection::AgentToClient,
            actual: FrameDirection::ClientToAgent,
        })
    ));
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_inbound_json_is_a_protocol_error() {
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let (_handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::short(usize::MAX),
        immediate_handler(),
        RpcFacadeConfig::default(),
    );
    client.write_all(b"not-json\n").await.unwrap();

    assert!(matches!(
        supervisor.wait().await,
        Err(RpcFacadeError::Protocol { .. })
    ));
}

#[tokio::test]
async fn inbound_sequences_are_global_monotonic_and_session_local_monotonic() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let admissions_done = Arc::new(tokio::sync::Notify::new());
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let (_handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::short(usize::MAX),
        recording_handler(seen.clone(), 3, admissions_done.clone()),
        RpcFacadeConfig::default(),
    );
    client
        .write_all(&request("session/prompt", 1, "session-a"))
        .await
        .unwrap();
    client
        .write_all(&request("session/prompt", 2, "session-b"))
        .await
        .unwrap();
    client
        .write_all(&request("session/cancel", 3, "session-a"))
        .await
        .unwrap();
    admissions_done.notified().await;
    client.shutdown().await.unwrap();

    supervisor.wait().await.unwrap();
    let mut seen = seen.lock().unwrap().clone();
    seen.sort_by_key(|entry| entry.0);
    assert_eq!(
        seen,
        vec![
            (0, 0, "session-a".to_owned(), "session/prompt".to_owned()),
            (1, 0, "session-b".to_owned(), "session/prompt".to_owned()),
            (2, 1, "session-a".to_owned(), "session/cancel".to_owned()),
        ]
    );
}

#[tokio::test]
async fn inbound_global_sequence_exhaustion_at_u64_max_seals_without_duplicates() {
    let admitted = Arc::new(tokio::sync::Notify::new());
    let handler = {
        let admitted = admitted.clone();
        Arc::new(move |_| {
            let admitted = admitted.clone();
            Box::pin(async move {
                admitted.notify_one();
                Ok(Box::pin(async {}) as HandlerCompletion)
            }) as HandlerFuture
        })
    };
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let (_handle, supervisor) = spawn_rpc_facade_with_sequence_seeds(
        server_read,
        RecordingWriter::short(usize::MAX),
        handler,
        RpcFacadeConfig::default(),
        SequenceSeeds {
            inbound_global: u64::MAX,
            ..SequenceSeeds::default()
        },
    );
    client
        .write_all(&request("session/prompt", 70, "session-a"))
        .await
        .unwrap();
    admitted.notified().await;
    client
        .write_all(&request("session/prompt", 71, "session-b"))
        .await
        .unwrap();

    assert!(matches!(
        supervisor.wait().await,
        Err(RpcFacadeError::SequenceExhausted {
            scope: SequenceScope::InboundGlobal,
        })
    ));
}

#[tokio::test]
async fn inbound_session_sequence_exhaustion_at_u64_max_seals_without_duplicates() {
    let admitted = Arc::new(tokio::sync::Notify::new());
    let handler = {
        let admitted = admitted.clone();
        Arc::new(move |_| {
            let admitted = admitted.clone();
            Box::pin(async move {
                admitted.notify_one();
                Ok(Box::pin(async {}) as HandlerCompletion)
            }) as HandlerFuture
        })
    };
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let (_handle, supervisor) = spawn_rpc_facade_with_sequence_seeds(
        server_read,
        RecordingWriter::short(usize::MAX),
        handler,
        RpcFacadeConfig::default(),
        SequenceSeeds {
            inbound_session: u64::MAX,
            ..SequenceSeeds::default()
        },
    );
    client
        .write_all(&request("session/prompt", 72, "session-a"))
        .await
        .unwrap();
    admitted.notified().await;
    client
        .write_all(&request("session/cancel", 73, "session-a"))
        .await
        .unwrap();

    assert!(matches!(
        supervisor.wait().await,
        Err(RpcFacadeError::SequenceExhausted {
            scope: SequenceScope::InboundSession,
        })
    ));
}

#[tokio::test]
async fn outbound_sequence_exhaustion_at_u64_max_seals_without_duplicates() {
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let reader_dropped = Arc::new(AtomicBool::new(false));
    let writer_dropped = Arc::new(AtomicBool::new(false));
    let handler_dropped = Arc::new(AtomicBool::new(false));
    let handler_admitted = Arc::new(tokio::sync::Notify::new());
    let admissions = Arc::new(AtomicUsize::new(0));
    let handler = {
        let handler_dropped = handler_dropped.clone();
        let handler_admitted = handler_admitted.clone();
        let admissions = admissions.clone();
        Arc::new(move |_| {
            let handler_dropped = handler_dropped.clone();
            let handler_admitted = handler_admitted.clone();
            let admissions = admissions.clone();
            Box::pin(async move {
                admissions.fetch_add(1, Ordering::AcqRel);
                handler_admitted.notify_one();
                Ok(Box::pin(PendingDropFuture(handler_dropped)) as HandlerCompletion)
            }) as HandlerFuture
        })
    };
    let barrier = ReaderAdmissionBarrier::new(1);
    let (handle, supervisor) = spawn_rpc_facade_with_sequence_seeds_and_reader_admission_barrier(
        DropTracked::new(server_read, reader_dropped.clone()),
        DropTracked::new(RecordingWriter::short(usize::MAX), writer_dropped.clone()),
        handler,
        RpcFacadeConfig::default(),
        SequenceSeeds {
            outbound: u64::MAX,
            ..SequenceSeeds::default()
        },
        barrier.clone(),
    );
    client
        .write_all(&request("session/prompt", 74, "already-running"))
        .await
        .unwrap();
    handler_admitted.notified().await;
    client
        .write_all(&request("session/prompt", 76, "too-late"))
        .await
        .unwrap();
    tokio::time::timeout(TEST_TIMEOUT, barrier.wait_reached())
        .await
        .expect("second inbound frame did not reach its pre-admission barrier");
    let receipt = handle
        .enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(74, "last"),
        ))
        .unwrap();
    assert_eq!(receipt.sequence(), u64::MAX);
    assert!(matches!(
        handle.enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(75, "exhausted"),
        )),
        Err(RpcFacadeError::SequenceExhausted {
            scope: SequenceScope::Outbound,
        })
    ));
    assert!(matches!(
        handle.enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(76, "sealed"),
        )),
        Err(RpcFacadeError::Sealed)
    ));
    barrier.release();
    drop(receipt);

    assert!(matches!(
        tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "outbound sequence exhaustion did not wake supervisor; observed {} inbound admissions",
                    admissions.load(Ordering::Acquire)
                )
            }),
        Err(RpcFacadeError::SequenceExhausted {
            scope: SequenceScope::Outbound,
        })
    ));
    assert_eq!(admissions.load(Ordering::Acquire), 1);
    assert!(reader_dropped.load(Ordering::Acquire));
    assert!(writer_dropped.load(Ordering::Acquire));
    assert!(handler_dropped.load(Ordering::Acquire));
    drop(client);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_outbound_enqueues_preserve_sequence_and_physical_order() {
    let (_client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let writer = RecordingWriter::short(usize::MAX);
    let writer_state = writer.state.clone();
    let barrier = OutboundReservationBarrier::new(0);
    let release_guard = OutboundBarrierReleaseGuard::new(barrier.clone());
    let (handle, supervisor) =
        spawn_rpc_facade_with_sequence_seeds_and_outbound_reservation_barrier(
            server_read,
            writer,
            immediate_handler(),
            RpcFacadeConfig::default(),
            SequenceSeeds::default(),
            barrier.clone(),
        );
    let first_encoded = response(90, "first");
    let second_encoded = response(91, "second");

    let first_handle = handle.clone();
    let first_frame = first_encoded.clone();
    let first = tokio::task::spawn_blocking(move || {
        first_handle.enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            first_frame,
        ))
    });
    tokio::time::timeout(TEST_TIMEOUT, barrier.wait_reached())
        .await
        .expect("first enqueue did not reach its post-reservation barrier");

    let second_handle = handle.clone();
    let second_frame = second_encoded.clone();
    let (gate_observed, gate_observed_rx) = oneshot::channel();
    let second = tokio::task::spawn_blocking(move || {
        let _ = gate_observed.send(second_handle.outgoing_admission_gate_is_held_for_test());
        second_handle.enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            second_frame,
        ))
    });
    let gate_observed = tokio::time::timeout(TEST_TIMEOUT, gate_observed_rx).await;
    release_guard.release();
    let gate_observed = gate_observed
        .expect("second enqueue did not probe the outgoing admission gate")
        .expect("second enqueue dropped its outgoing admission gate probe");
    assert!(
        gate_observed,
        "second enqueue did not observe the first holding admission"
    );
    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap().unwrap();
    assert_eq!((first.sequence(), second.sequence()), (0, 1));
    assert_eq!(first.ack().await.unwrap().sequence, 0);
    assert_eq!(second.ack().await.unwrap().sequence, 1);

    let mut expected = first_encoded;
    expected.extend_from_slice(&second_encoded);
    assert_eq!(writer_state.lock().unwrap().bytes, expected);
    supervisor.shutdown().await.unwrap();
}

struct OutboundBarrierReleaseGuard(Option<OutboundReservationBarrier>);

impl OutboundBarrierReleaseGuard {
    fn new(barrier: OutboundReservationBarrier) -> Self {
        Self(Some(barrier))
    }

    fn release(mut self) {
        if let Some(barrier) = self.0.take() {
            barrier.release();
        }
    }
}

impl Drop for OutboundBarrierReleaseGuard {
    fn drop(&mut self) {
        if let Some(barrier) = self.0.take() {
            barrier.release();
        }
    }
}

#[tokio::test]
async fn acknowledgements_are_correlated_and_only_resolve_after_physical_flush() {
    let (_client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let writer = RecordingWriter::gated_flush();
    let gate = writer.flush_gate.clone();
    let (handle, supervisor) = spawn_rpc_facade(
        server_read,
        writer,
        immediate_handler(),
        RpcFacadeConfig::default(),
    );
    let first = handle
        .enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(21, "first"),
        ))
        .unwrap();
    let second = handle
        .enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(22, "second"),
        ))
        .unwrap();
    assert_eq!((first.sequence(), second.sequence()), (0, 1));
    let first_ack = tokio::spawn(first.ack());
    let second_ack = tokio::spawn(second.ack());
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(!first_ack.is_finished());
    assert!(!second_ack.is_finished());

    gate.add_permits(1);
    assert_eq!(first_ack.await.unwrap().unwrap().sequence, 0);
    assert!(!second_ack.is_finished());
    gate.add_permits(1);
    assert_eq!(second_ack.await.unwrap().unwrap().sequence, 1);
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn eof_seals_ingress_and_cleanly_joins_all_owned_tasks() {
    assert_eq!(ACP_SUPERVISOR_JOIN_DEADLINE_MS, 5_000);
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let (handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::short(usize::MAX),
        immediate_handler(),
        RpcFacadeConfig::default(),
    );
    client.shutdown().await.unwrap();

    let report = supervisor.wait().await.unwrap();
    assert!(report.eof);
    assert!(report.reader_joined && report.scheduler_joined && report.writer_joined);
    assert!(matches!(
        handle.enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(30, "after-eof")
        )),
        Err(RpcFacadeError::Sealed)
    ));
}

#[tokio::test]
async fn shutdown_cancels_and_joins_reader_scheduler_and_pending_writer() {
    let (_client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let config = RpcFacadeConfig {
        write_flush_deadline: Duration::from_secs(60),
        supervisor_join_deadline: Duration::from_millis(100),
    };
    let (handle, supervisor) = spawn_rpc_facade(
        server_read,
        RecordingWriter::pending(),
        Arc::new(|_| {
            Box::pin(async { Ok(Box::pin(pending()) as HandlerCompletion) }) as HandlerFuture
        }),
        config,
    );
    let receipt = handle
        .enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(31, "never-written"),
        ))
        .unwrap();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    let report = tokio::time::timeout(TEST_TIMEOUT, supervisor.shutdown())
        .await
        .expect("shutdown hung")
        .expect("joined shutdown");
    assert!(report.reader_joined && report.scheduler_joined && report.writer_joined);
    assert!(matches!(receipt.ack().await, Err(RpcFacadeError::Sealed)));
}

#[tokio::test]
async fn live_handler_panic_wakes_supervisor_and_cleans_all_owned_work() {
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let reader_dropped = Arc::new(AtomicBool::new(false));
    let writer_dropped = Arc::new(AtomicBool::new(false));
    let survivor_admitted = Arc::new(tokio::sync::Notify::new());
    let survivor_dropped = Arc::new(AtomicBool::new(false));
    let handler = {
        let survivor_admitted = survivor_admitted.clone();
        let survivor_dropped = survivor_dropped.clone();
        Arc::new(move |frame: InboundFrame| {
            let survivor_admitted = survivor_admitted.clone();
            let survivor_dropped = survivor_dropped.clone();
            Box::pin(async move {
                if frame.session_id() == Some("panic") {
                    panic!("injected handler panic");
                }
                survivor_admitted.notify_one();
                Ok(Box::pin(PendingDropFuture(survivor_dropped)) as HandlerCompletion)
            }) as HandlerFuture
        })
    };
    let (_handle, supervisor) = spawn_rpc_facade(
        DropTracked::new(server_read, reader_dropped.clone()),
        DropTracked::new(RecordingWriter::short(usize::MAX), writer_dropped.clone()),
        handler,
        RpcFacadeConfig::default(),
    );
    client
        .write_all(&request("session/prompt", 40, "survivor"))
        .await
        .unwrap();
    survivor_admitted.notified().await;
    client
        .write_all(&request("session/prompt", 41, "panic"))
        .await
        .unwrap();

    assert!(matches!(
        tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
            .await
            .expect("handler panic did not wake supervisor"),
        Err(RpcFacadeError::Task {
            task: "scheduler",
            ..
        })
    ));
    assert!(reader_dropped.load(Ordering::Acquire));
    assert!(writer_dropped.load(Ordering::Acquire));
    assert!(survivor_dropped.load(Ordering::Acquire));
    drop(client);
}

#[tokio::test]
async fn live_writer_panic_wakes_supervisor_and_cleans_all_owned_work() {
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let reader_dropped = Arc::new(AtomicBool::new(false));
    let writer_dropped = Arc::new(AtomicBool::new(false));
    let handler_admitted = Arc::new(tokio::sync::Notify::new());
    let handler_dropped = Arc::new(AtomicBool::new(false));
    let handler = {
        let handler_admitted = handler_admitted.clone();
        let handler_dropped = handler_dropped.clone();
        Arc::new(move |_| {
            let handler_admitted = handler_admitted.clone();
            let handler_dropped = handler_dropped.clone();
            Box::pin(async move {
                handler_admitted.notify_one();
                Ok(Box::pin(PendingDropFuture(handler_dropped)) as HandlerCompletion)
            }) as HandlerFuture
        })
    };
    let (handle, supervisor) = spawn_rpc_facade(
        DropTracked::new(server_read, reader_dropped.clone()),
        DropTracked::new(RecordingWriter::panic_write(), writer_dropped.clone()),
        handler,
        RpcFacadeConfig::default(),
    );
    client
        .write_all(&request("session/prompt", 42, "session-a"))
        .await
        .unwrap();
    handler_admitted.notified().await;
    let _receipt = handle
        .enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(42, "panic"),
        ))
        .unwrap();

    assert!(matches!(
        tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
            .await
            .expect("writer panic did not wake supervisor"),
        Err(RpcFacadeError::Task { task: "writer", .. })
    ));
    assert!(reader_dropped.load(Ordering::Acquire));
    assert!(writer_dropped.load(Ordering::Acquire));
    assert!(handler_dropped.load(Ordering::Acquire));
    drop(client);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_join_timeout_still_drops_reader_scheduler_writer_and_handlers() {
    let reader_dropped = Arc::new(AtomicBool::new(false));
    let writer_dropped = Arc::new(AtomicBool::new(false));
    let handler_dropped = Arc::new(AtomicBool::new(false));
    let handler_admitted = Arc::new(tokio::sync::Notify::new());
    let handler_polling = Arc::new(tokio::sync::Notify::new());
    let handler = {
        let handler_dropped = handler_dropped.clone();
        let handler_admitted = handler_admitted.clone();
        let handler_polling = handler_polling.clone();
        Arc::new(move |_| {
            let handler_dropped = handler_dropped.clone();
            let handler_admitted = handler_admitted.clone();
            let handler_polling = handler_polling.clone();
            Box::pin(async move {
                handler_admitted.notify_one();
                Ok(Box::pin(BlockingPendingDropFuture {
                    dropped: handler_dropped,
                    polling: handler_polling,
                }) as HandlerCompletion)
            }) as HandlerFuture
        })
    };
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let config = RpcFacadeConfig {
        write_flush_deadline: Duration::from_secs(60),
        supervisor_join_deadline: Duration::from_millis(10),
    };
    let (_handle, supervisor) = spawn_rpc_facade(
        DropTracked::new(server_read, reader_dropped.clone()),
        DropTracked::new(RecordingWriter::pending(), writer_dropped.clone()),
        handler,
        config,
    );
    client
        .write_all(&request("session/prompt", 60, "session-a"))
        .await
        .unwrap();
    handler_admitted.notified().await;
    handler_polling.notified().await;
    client.shutdown().await.unwrap();

    assert!(matches!(
        tokio::time::timeout(TEST_TIMEOUT, supervisor.wait())
            .await
            .expect("cleanup exceeded outer timeout"),
        Err(RpcFacadeError::Timeout {
            phase: rpc_facade::TimeoutPhase::SupervisorJoin,
            ..
        })
    ));
    assert!(reader_dropped.load(Ordering::Acquire));
    assert!(writer_dropped.load(Ordering::Acquire));
    assert!(handler_dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn dropping_supervisor_completes_deadline_bounded_detached_cleanup() {
    let (mut client, server) = pending_input();
    let (server_read, _server_write) = tokio::io::split(server);
    let reader_dropped = Arc::new(AtomicBool::new(false));
    let writer_dropped = Arc::new(AtomicBool::new(false));
    let handler_admitted = Arc::new(tokio::sync::Notify::new());
    let handler_dropped = Arc::new(AtomicBool::new(false));
    let handler = {
        let handler_admitted = handler_admitted.clone();
        let handler_dropped = handler_dropped.clone();
        Arc::new(move |_| {
            let handler_admitted = handler_admitted.clone();
            let handler_dropped = handler_dropped.clone();
            Box::pin(async move {
                handler_admitted.notify_one();
                Ok(Box::pin(PendingDropFuture(handler_dropped)) as HandlerCompletion)
            }) as HandlerFuture
        })
    };
    let (handle, supervisor) = spawn_rpc_facade(
        DropTracked::new(server_read, reader_dropped.clone()),
        DropTracked::new(RecordingWriter::pending(), writer_dropped.clone()),
        handler,
        RpcFacadeConfig {
            write_flush_deadline: Duration::from_secs(60),
            supervisor_join_deadline: Duration::from_millis(100),
        },
    );
    client
        .write_all(&request("session/prompt", 80, "session-a"))
        .await
        .unwrap();
    handler_admitted.notified().await;

    drop(supervisor);
    tokio::time::timeout(TEST_TIMEOUT, handle.wait_closed())
        .await
        .expect("detached coordinator did not complete cleanup");
    assert!(reader_dropped.load(Ordering::Acquire));
    assert!(writer_dropped.load(Ordering::Acquire));
    assert!(handler_dropped.load(Ordering::Acquire));
    assert!(matches!(
        handle.enqueue(TransportFrame::new(
            FrameDirection::AgentToClient,
            response(80, "closed"),
        )),
        Err(RpcFacadeError::Sealed)
    ));
    drop(client);
}

struct BlockingPendingDropFuture {
    dropped: Arc<AtomicBool>,
    polling: Arc<tokio::sync::Notify>,
}

struct PendingDropFuture(Arc<AtomicBool>);

impl Future for PendingDropFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

impl Drop for PendingDropFuture {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Future for BlockingPendingDropFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polling.notify_one();
        std::thread::sleep(Duration::from_millis(50));
        Poll::Pending
    }
}

impl Drop for BlockingPendingDropFuture {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

struct DropTracked<T> {
    inner: T,
    dropped: Arc<AtomicBool>,
}

impl<T> DropTracked<T> {
    fn new(inner: T, dropped: Arc<AtomicBool>) -> Self {
        Self { inner, dropped }
    }
}

impl<T> Drop for DropTracked<T> {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

impl<T> AsyncRead for DropTracked<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buffer)
    }
}

impl<T> AsyncWrite for DropTracked<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[derive(Default)]
struct WriterState {
    bytes: Vec<u8>,
    write_calls: usize,
    flush_calls: usize,
}

enum WriterMode {
    Short(usize),
    FailWriteAfter(usize),
    FailFlush,
    Pending,
    GatedFlush,
    PanicWrite,
}

struct RecordingWriter {
    state: Arc<Mutex<WriterState>>,
    mode: WriterMode,
    flush_gate: Arc<tokio::sync::Semaphore>,
    flush_wait: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl RecordingWriter {
    fn short(max_write: usize) -> Self {
        Self::new(WriterMode::Short(max_write))
    }

    fn fail_write_after(bytes: usize) -> Self {
        Self::new(WriterMode::FailWriteAfter(bytes))
    }

    fn fail_flush() -> Self {
        Self::new(WriterMode::FailFlush)
    }

    fn pending() -> Self {
        Self::new(WriterMode::Pending)
    }

    fn gated_flush() -> Self {
        Self::new(WriterMode::GatedFlush)
    }

    fn panic_write() -> Self {
        Self::new(WriterMode::PanicWrite)
    }

    fn new(mode: WriterMode) -> Self {
        Self {
            state: Arc::new(Mutex::new(WriterState::default())),
            mode,
            flush_gate: Arc::new(tokio::sync::Semaphore::new(0)),
            flush_wait: None,
        }
    }
}

impl AsyncWrite for RecordingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let mut state = this.state.lock().unwrap();
        state.write_calls += 1;
        let count = match this.mode {
            WriterMode::Short(max) => bytes.len().min(max),
            WriterMode::FailWriteAfter(limit) => {
                if state.bytes.len() >= limit {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "injected write failure",
                    )));
                }
                bytes.len().min(limit - state.bytes.len())
            }
            WriterMode::FailFlush | WriterMode::GatedFlush => bytes.len(),
            WriterMode::Pending => return Poll::Pending,
            WriterMode::PanicWrite => panic!("injected writer panic"),
        };
        state.bytes.extend_from_slice(&bytes[..count]);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.mode {
            WriterMode::FailFlush => Poll::Ready(Err(io::Error::other("injected flush failure"))),
            WriterMode::GatedFlush => {
                if this.flush_wait.is_none() {
                    let gate = this.flush_gate.clone();
                    this.flush_wait = Some(Box::pin(async move {
                        let permit = gate.acquire_owned().await.unwrap();
                        permit.forget();
                    }));
                }
                match this.flush_wait.as_mut().unwrap().as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        this.flush_wait = None;
                        this.state.lock().unwrap().flush_calls += 1;
                        Poll::Ready(Ok(()))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            WriterMode::Pending => Poll::Pending,
            WriterMode::Short(_) | WriterMode::FailWriteAfter(_) | WriterMode::PanicWrite => {
                this.state.lock().unwrap().flush_calls += 1;
                Poll::Ready(Ok(()))
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

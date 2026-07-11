use std::sync::LazyLock;
use std::thread;
use std::time::Duration;

use orca_core::cancel::CancelToken;
use reqwest::blocking::{
    Client as BlockingClient, RequestBuilder as BlockingRequestBuilder,
    Response as BlockingResponse,
};
use reqwest::{Client, RequestBuilder, Response};

const CONNECT_TIMEOUT_SECS: u64 = 30;
const REQUEST_TIMEOUT_SECS: u64 = 120;
const STREAMING_IDLE_READ_TIMEOUT_SECS: u64 = 300;
pub(crate) const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 1000;
const MAX_BACKOFF_MS: u64 = 60_000;
const BACKOFF_FACTOR: f64 = 2.0;
const JITTER_FACTOR: f64 = 0.1;

const RETRYABLE_STATUS_CODES: &[u16] = &[429, 500, 502, 503, 504];

static CLIENT: LazyLock<BlockingClient> = LazyLock::new(|| {
    BlockingClient::builder()
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .expect("failed to build HTTP client")
});

pub(crate) fn streaming_client() -> Result<Client, String> {
    Client::builder()
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .hickory_dns(true)
        .build()
        .map_err(|error| format!("failed to build streaming HTTP client: {error}"))
}

pub(crate) fn streaming_idle_read_timeout() -> Duration {
    Duration::from_secs(STREAMING_IDLE_READ_TIMEOUT_SECS)
}

pub fn client() -> &'static BlockingClient {
    &CLIENT
}

pub fn execute_with_retry(
    build_request: impl Fn(&BlockingClient) -> BlockingRequestBuilder,
) -> Result<BlockingResponse, String> {
    let client = client();
    let mut attempt: u32 = 0;

    loop {
        let result = build_request(client).send();

        match result {
            Ok(resp) => {
                let status = resp.status();
                if !RETRYABLE_STATUS_CODES.contains(&status.as_u16()) {
                    return resp
                        .error_for_status()
                        .map_err(|e| format!("request error: {e}"));
                }
                if attempt >= MAX_RETRIES {
                    return Err(format!("max retries exceeded (last status: {status})"));
                }
                let delay =
                    retry_after_header(resp.headers()).unwrap_or_else(|| compute_backoff(attempt));
                thread::sleep(delay);
            }
            Err(err) => {
                if attempt >= MAX_RETRIES || !is_retryable_error(&err) {
                    return Err(format!(
                        "request failed after {} attempts: {err}",
                        attempt + 1
                    ));
                }
                let delay = compute_backoff(attempt);
                thread::sleep(delay);
            }
        }

        attempt += 1;
    }
}

pub async fn execute_streaming_with_retry(
    client: &Client,
    build_request: impl Fn(&Client) -> RequestBuilder,
    cancel: &CancelToken,
) -> Result<Response, String> {
    let mut attempt: u32 = 0;

    loop {
        let result = send_streaming_request(build_request(client), cancel).await?;

        match result {
            Ok(resp) => {
                let status = resp.status();
                if !RETRYABLE_STATUS_CODES.contains(&status.as_u16()) {
                    if status.is_client_error() || status.is_server_error() {
                        let body = response_text_with_cancel(resp, cancel).await?;
                        return Err(format!("request error ({status}): {body}"));
                    }
                    return Ok(resp);
                }
                if attempt >= MAX_RETRIES {
                    return Err(format!("max retries exceeded (last status: {status})"));
                }
                let delay =
                    retry_after_header(resp.headers()).unwrap_or_else(|| compute_backoff(attempt));
                sleep_with_cancel(delay, cancel).await?;
            }
            Err(err) => {
                if attempt >= MAX_RETRIES || !is_retryable_error(&err) {
                    return Err(format!(
                        "request failed after {} attempts: {err}",
                        attempt + 1
                    ));
                }
                let delay = compute_backoff(attempt);
                sleep_with_cancel(delay, cancel).await?;
            }
        }

        attempt += 1;
    }
}

async fn send_streaming_request(
    request: RequestBuilder,
    cancel: &CancelToken,
) -> Result<Result<Response, reqwest::Error>, String> {
    if cancel.is_cancelled() {
        return Err("request cancelled".to_string());
    }

    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => Err("request cancelled".to_string()),
        result = request.send() => Ok(result),
    }
}

async fn response_text_with_cancel(
    response: Response,
    cancel: &CancelToken,
) -> Result<String, String> {
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => Err("request cancelled".to_string()),
        result = tokio::time::timeout(streaming_idle_read_timeout(), response.text()) => {
            match result {
                Ok(Ok(body)) => Ok(body),
                Ok(Err(error)) => Err(format!("response body read failed: {error}")),
                Err(_) => Err(format!(
                    "response body idle read timed out after {:?}",
                    streaming_idle_read_timeout()
                )),
            }
        }
    }
}

async fn sleep_with_cancel(delay: Duration, cancel: &CancelToken) -> Result<(), String> {
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => Err("request cancelled".to_string()),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

pub(crate) async fn wait_for_cancel(cancel: &CancelToken) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(CANCELLATION_POLL_INTERVAL).await;
    }
}

fn compute_backoff(attempt: u32) -> Duration {
    let base_ms = INITIAL_BACKOFF_MS as f64 * BACKOFF_FACTOR.powi(attempt as i32);
    let capped_ms = base_ms.min(MAX_BACKOFF_MS as f64);
    let jitter = 1.0 + (jitter_value() * 2.0 - 1.0) * JITTER_FACTOR;
    Duration::from_millis((capped_ms * jitter) as u64)
}

fn retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn is_retryable_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || err.is_request()
}

fn jitter_value() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (nanos as f64) / (u32::MAX as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Instant;

    #[test]
    fn backoff_increases_exponentially() {
        let d0 = compute_backoff(0);
        let d1 = compute_backoff(1);
        let d2 = compute_backoff(2);

        assert!(d0.as_millis() >= 900 && d0.as_millis() <= 1100);
        assert!(d1.as_millis() >= 1800 && d1.as_millis() <= 2200);
        assert!(d2.as_millis() >= 3600 && d2.as_millis() <= 4400);
    }

    #[test]
    fn backoff_caps_at_max() {
        let d10 = compute_backoff(10);
        assert!(d10.as_millis() <= 66_000);
    }

    #[test]
    fn jitter_value_is_in_range() {
        let v = jitter_value();
        assert!((0.0..=1.0).contains(&v));
    }

    #[test]
    fn streaming_client_uses_idle_read_timeout() {
        assert_eq!(
            streaming_idle_read_timeout(),
            Duration::from_secs(300),
            "streaming responses should allow long active generations but fail idle reads"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_retry_backoff_stops_when_cancelled() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind retry endpoint");
        listener
            .set_nonblocking(true)
            .expect("set retry endpoint nonblocking");
        let address = listener.local_addr().expect("retry endpoint address");
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let (response_tx, response_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        server_requests.fetch_add(1, Ordering::SeqCst);
                        let mut request = [0u8; 4 * 1024];
                        let _ = stream.read(&mut request);
                        write!(
                            stream,
                            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .expect("write retry response");
                        response_tx.send(()).expect("announce retry response");
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "retry request was not received");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept retry request: {error}"),
                }
            }
        });
        let cancel = CancelToken::new();
        let cancel_after_response = cancel.clone();
        let canceller = thread::spawn(move || {
            response_rx.recv().expect("wait for retry response");
            cancel_after_response.cancel();
        });
        let url = format!("http://{address}/retry");

        let started = Instant::now();
        let client = streaming_client().expect("streaming client");
        let result =
            execute_streaming_with_retry(&client, |client| client.get(&url), &cancel).await;
        let elapsed = started.elapsed();

        canceller.join().expect("retry canceller");
        server.join().expect("retry server");
        assert_eq!(result.unwrap_err(), "request cancelled");
        assert!(
            elapsed < Duration::from_millis(250),
            "cancelled retry slept for {elapsed:?}"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_streaming_request_closes_in_flight_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled endpoint");
        let address = listener.local_addr().expect("stalled endpoint address");
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept stalled request");
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("set request read timeout");
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).expect("read request headers");
                assert!(read > 0, "client closed before request headers completed");
                request.extend_from_slice(&chunk[..read]);
            }
            accepted_tx.send(()).expect("announce accepted request");
            stream
                .set_read_timeout(Some(Duration::from_millis(400)))
                .expect("set peer close timeout");
            let mut byte = [0_u8; 1];
            let closed = match stream.read(&mut byte) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    true
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    false
                }
                Err(error) => panic!("read client close: {error}"),
            };
            closed_tx.send(closed).expect("report client close");
        });
        let cancel = CancelToken::new();
        let cancel_after_accept = cancel.clone();
        let canceller = thread::spawn(move || {
            accepted_rx.recv().expect("wait for accepted request");
            cancel_after_accept.cancel();
        });
        let url = format!("http://{address}/stall");

        let started = Instant::now();
        let client = streaming_client().expect("streaming client");
        let result =
            execute_streaming_with_retry(&client, |client| client.get(&url), &cancel).await;
        let elapsed = started.elapsed();
        let connection_closed = closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("wait for peer close result");

        canceller.join().expect("stalled request canceller");
        server.join().expect("stalled request server");
        assert_eq!(result.unwrap_err(), "request cancelled");
        assert!(
            elapsed < Duration::from_millis(500),
            "cancelled request returned after {elapsed:?}"
        );
        assert!(
            connection_closed,
            "cancelled request left the TCP connection owned by a detached worker"
        );
    }
}

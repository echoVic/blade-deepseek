use std::sync::LazyLock;
use std::thread;
use std::time::Duration;

use reqwest::blocking::{Client, RequestBuilder, Response};

const CONNECT_TIMEOUT_SECS: u64 = 30;
const REQUEST_TIMEOUT_SECS: u64 = 120;
const STREAMING_IDLE_READ_TIMEOUT_SECS: u64 = 300;

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 1000;
const MAX_BACKOFF_MS: u64 = 60_000;
const BACKOFF_FACTOR: f64 = 2.0;
const JITTER_FACTOR: f64 = 0.1;

const RETRYABLE_STATUS_CODES: &[u16] = &[429, 500, 502, 503, 504];

static CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .expect("failed to build HTTP client")
});

static STREAMING_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(None)
        .build()
        .expect("failed to build streaming HTTP client")
});

pub(crate) fn streaming_idle_read_timeout() -> Duration {
    Duration::from_secs(STREAMING_IDLE_READ_TIMEOUT_SECS)
}

pub fn client() -> &'static Client {
    &CLIENT
}

pub fn streaming_client() -> &'static Client {
    &STREAMING_CLIENT
}

pub fn execute_with_retry(
    build_request: impl Fn(&Client) -> RequestBuilder,
) -> Result<Response, String> {
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
                let delay = retry_after_header(&resp).unwrap_or_else(|| compute_backoff(attempt));
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

pub fn execute_streaming_with_retry(
    build_request: impl Fn(&Client) -> RequestBuilder,
) -> Result<Response, String> {
    let client = streaming_client();
    let mut attempt: u32 = 0;

    loop {
        let result = build_request(client).send();

        match result {
            Ok(resp) => {
                let status = resp.status();
                if !RETRYABLE_STATUS_CODES.contains(&status.as_u16()) {
                    if status.is_client_error() || status.is_server_error() {
                        let body = resp.text().unwrap_or_default();
                        return Err(format!("request error ({status}): {body}"));
                    }
                    return Ok(resp);
                }
                if attempt >= MAX_RETRIES {
                    return Err(format!("max retries exceeded (last status: {status})"));
                }
                let delay = retry_after_header(&resp).unwrap_or_else(|| compute_backoff(attempt));
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

fn compute_backoff(attempt: u32) -> Duration {
    let base_ms = INITIAL_BACKOFF_MS as f64 * BACKOFF_FACTOR.powi(attempt as i32);
    let capped_ms = base_ms.min(MAX_BACKOFF_MS as f64);
    let jitter = 1.0 + (jitter_value() * 2.0 - 1.0) * JITTER_FACTOR;
    Duration::from_millis((capped_ms * jitter) as u64)
}

fn retry_after_header(resp: &Response) -> Option<Duration> {
    resp.headers()
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
}

//! Shared transport defaults for discovery, metadata, and artifact clients.
use std::{
    error::Error as _,
    fmt, io, thread,
    time::{Duration, SystemTime},
};

use anyhow::{Error, Result};
use rand::Rng;
use reqwest::{
    StatusCode,
    blocking::{Client, ClientBuilder, Response},
    header::{HeaderValue, RETRY_AFTER},
};

pub(crate) const DEFAULT_ATTEMPTS: u32 = 3;

pub(crate) fn client_builder() -> ClientBuilder {
    // Keep reqwest's automatic environment-proxy discovery enabled. Installing
    // an explicit Proxy or calling no_proxy() here would override its handling
    // of HTTP_PROXY, HTTPS_PROXY, ALL_PROXY, NO_PROXY and lowercase equivalents.
    Client::builder()
        // Keep a single attempt budget rather than stacking reqwest retries
        // underneath our status/body-aware application policy.
        .retry(reqwest::retry::never())
        .user_agent(concat!("snatchrs/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
}

#[derive(Debug)]
struct TransientFailure {
    source: Error,
    retry_after: Option<Duration>,
}
impl fmt::Display for TransientFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}
impl std::error::Error for TransientFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn transient(error: Error, retry_after: Option<Duration>) -> Error {
    Error::new(TransientFailure {
        source: error,
        retry_after,
    })
}

fn transient_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

fn retry_after(value: &HeaderValue, now: SystemTime) -> Option<Duration> {
    let value = value.to_str().ok()?.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or_default())
}

pub(crate) fn check_response(response: Response) -> Result<Response> {
    if let Err(error) = response.error_for_status_ref() {
        let delay = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| retry_after(value, SystemTime::now()));
        return Err(if transient_status(response.status()) {
            transient(error.into(), delay)
        } else {
            error.into()
        });
    }
    Ok(response)
}

fn transient_io(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

fn transient_request(error: &reqwest::Error) -> bool {
    // Blocking response-read timeouts are also tagged as decode errors.
    if error.is_timeout() {
        return true;
    }
    if error.is_builder() || error.is_redirect() || error.is_decode() || error.is_status() {
        return false;
    }
    // TLS validation and invalid client configuration can surface as connection
    // errors, but their underlying I/O errors are permanent.
    let mut source = error.source();
    while let Some(cause) = source {
        if cause.downcast_ref::<io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::InvalidInput
                    | io::ErrorKind::InvalidData
                    | io::ErrorKind::PermissionDenied
                    | io::ErrorKind::Unsupported
            )
        }) {
            return false;
        }
        source = cause.source();
    }
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

pub(crate) fn request_error(error: reqwest::Error) -> Error {
    if transient_request(&error) {
        transient(error.into(), None)
    } else {
        error.into()
    }
}

/// Called only for response reads, never for filesystem writes. Local disk
/// errors remain unmarked and therefore cannot trigger network retries.
pub(crate) fn read_error(error: io::Error) -> Error {
    metadata_error(error.into())
}

pub(crate) fn json_error(error: serde_json::Error) -> Error {
    // serde_json's source() skips the wrapped io::Error. Unwrap it so a
    // reqwest read timeout remains identifiable rather than looking like an
    // ordinary JSON decode error.
    if error.is_io() {
        io::Error::from(error).into()
    } else {
        error.into()
    }
}

fn transient_read(error: &io::Error) -> bool {
    transient_io(error.kind())
        || error
            .get_ref()
            .and_then(|source| source.downcast_ref::<reqwest::Error>())
            .is_some_and(transient_request)
}

pub(crate) fn metadata_error(error: Error) -> Error {
    let retry = error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(transient_request)
            || cause
                .downcast_ref::<io::Error>()
                .is_some_and(transient_read)
            || cause
                .downcast_ref::<serde_json::Error>()
                .and_then(|error| error.io_error_kind())
                .is_some_and(transient_io)
    });
    if retry { transient(error, None) } else { error }
}

fn backoff(attempt: u32, rng: &mut impl Rng) -> Duration {
    let cap_ms = 500u64
        .saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)))
        .min(30_000);
    Duration::from_millis(rng.random_range(cap_ms / 2..=cap_ms))
}

pub(crate) fn retry_delay(attempt: u32, attempts: u32, error: &Error) -> Option<Duration> {
    if attempt >= attempts {
        return None;
    }
    let failure = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TransientFailure>())?;
    // Server delays are minimums, not caps: add jitter after Retry-After rather
    // than clamping a long server delay or retrying all workers at its deadline.
    Some(
        failure
            .retry_after
            .unwrap_or_default()
            .saturating_add(backoff(attempt, &mut rand::rng())),
    )
}

pub(crate) fn retry<T>(attempts: u32, mut operation: impl FnMut() -> Result<T>) -> Result<T> {
    let mut attempt = 1;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) => {
                let Some(delay) = retry_delay(attempt, attempts, &error) else {
                    return Err(
                        error.context(format!("HTTP operation failed after {attempt} attempt(s)"))
                    );
                };
                thread::sleep(delay);
                attempt += 1;
            }
        }
    }
}

pub(crate) fn get<T>(
    client: &Client,
    url: &url::Url,
    accept: &str,
    attempts: u32,
    mut decode: impl FnMut(Response) -> Result<T>,
) -> Result<T> {
    retry(attempts, || {
        let response = client
            .get(url.clone())
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .map_err(request_error)?;
        decode(check_response(response)?).map_err(metadata_error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};

    #[test]
    fn statuses_and_local_failures_are_classified_without_retrying_permanent_errors() {
        for code in [408, 429, 500, 502, 503, 504] {
            assert!(transient_status(StatusCode::from_u16(code).unwrap()));
        }
        for code in [400, 401, 403, 404, 410, 501, 505, 507] {
            assert!(!transient_status(StatusCode::from_u16(code).unwrap()));
        }
        assert!(
            retry_delay(
                1,
                3,
                &io::Error::from(io::ErrorKind::PermissionDenied).into()
            )
            .is_none()
        );
        assert!(retry_delay(1, 3, &anyhow::anyhow!("Not enough disk space")).is_none());
        assert!(retry_delay(3, 3, &transient(anyhow::anyhow!("timeout"), None)).is_none());
    }

    #[test]
    fn retry_after_supports_seconds_dates_and_invalid_values() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(
            retry_after(&HeaderValue::from_static("12"), now),
            Some(Duration::from_secs(12))
        );
        let future =
            HeaderValue::from_str(&httpdate::fmt_http_date(now + Duration::from_secs(60))).unwrap();
        assert_eq!(retry_after(&future, now), Some(Duration::from_secs(60)));
        let past =
            HeaderValue::from_str(&httpdate::fmt_http_date(now - Duration::from_secs(60))).unwrap();
        assert_eq!(retry_after(&past, now), Some(Duration::ZERO));
        for value in ["", "-1", "+1", "1.5", "invalid"] {
            assert_eq!(
                retry_after(&HeaderValue::from_str(value).unwrap(), now),
                None
            );
        }
    }

    #[test]
    fn jitter_is_exponential_capped_and_does_not_shorten_server_delay() {
        let mut rng = StdRng::seed_from_u64(42);
        let values: std::collections::BTreeSet<_> = (0..32).map(|_| backoff(1, &mut rng)).collect();
        assert!(values.len() > 1);
        assert!(values.iter().all(|delay| {
            (Duration::from_millis(250)..=Duration::from_millis(500)).contains(delay)
        }));
        assert!(
            (Duration::from_millis(500)..=Duration::from_secs(1)).contains(&backoff(2, &mut rng))
        );
        assert!(
            (Duration::from_secs(15)..=Duration::from_secs(30)).contains(&backoff(1000, &mut rng))
        );
        let error = transient(
            anyhow::anyhow!("rate limited"),
            Some(Duration::from_secs(120)),
        );
        assert!(retry_delay(1, 3, &error).unwrap() >= Duration::from_secs(120));
    }

    #[test]
    fn json_response_read_timeouts_are_retried() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/simple/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let server = thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0; 1024];
                    let size = stream.read(&mut bytes).unwrap();
                    assert!(size > 0);
                    request.extend_from_slice(&bytes[..size]);
                    if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        break;
                    }
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n")
                    .unwrap();
                if attempt == 0 {
                    thread::sleep(Duration::from_millis(500));
                }
                let _ = stream.write_all(b"[]");
            }
        });
        let client = client_builder()
            .timeout(Duration::from_millis(250))
            .build()
            .unwrap();
        let result: serde_json::Value = get(&client, &url, "application/json", 2, |response| {
            serde_json::from_reader(response).map_err(json_error)
        })
        .unwrap();
        server.join().unwrap();
        assert_eq!(result, serde_json::json!([]));
    }
}

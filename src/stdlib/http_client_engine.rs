//! Shared synchronous HTTP transport. No Ku Value, AST, interpreter or scheduler.
use std::{
    collections::HashMap,
    io::Read,
    sync::OnceLock,
    time::{Duration, Instant},
};
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const MAX_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_MAX_BODY_BYTES: usize = 1_000_000;
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
static DEFAULT_AGENT: OnceLock<ureq::Agent> = OnceLock::new();

#[derive(Debug)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    pub timeout_ms: u64,
    pub max_body_bytes: usize,
}

#[derive(Debug)]
pub struct HttpResponse {
    pub status: i64,
    pub headers: HashMap<String, String>,
    pub body: String,
}

#[derive(Debug)]
pub struct HttpError {
    pub code: String,
    pub message: String,
}

pub fn http_request(config: HttpRequest) -> Result<HttpResponse, HttpError> {
    http_request_with_deadline(config, None)
}

pub fn http_request_with_deadline(
    config: HttpRequest,
    caller_deadline: Option<Instant>,
) -> Result<HttpResponse, HttpError> {
    let method = config.method.to_ascii_uppercase();
    if method != "GET" && method != "POST" {
        return Err(http_error(
            "invalid_method",
            format!("http method '{method}' is not supported yet"),
        ));
    }
    if config.url.bytes().any(|byte| byte <= 0x20 || byte == 0x7f) {
        return Err(http_error(
            "invalid_url",
            "http url must not contain whitespace or control characters",
        ));
    }
    let parsed_url = url::Url::parse(&config.url)
        .map_err(|err| http_error("invalid_url", format!("invalid http url: {err}")))?;
    if !matches!(parsed_url.scheme(), "http" | "https") || parsed_url.host().is_none() {
        return Err(http_error(
            "invalid_url",
            "http url must be an absolute http:// or https:// URL",
        ));
    }

    let mut request = default_agent().request(&method, &config.url);
    for (name, value) in &config.headers {
        request = request.set(name, value);
    }
    // The native caller may carry an earlier, already running frame budget.
    // Compute the remaining allowance after preparation, without restarting it.
    let mut timeout = Duration::from_millis(config.timeout_ms);
    if let Some(deadline) = caller_deadline {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| http_error("transport", "http caller deadline expired before I/O"))?;
        timeout = timeout.min(remaining);
    }
    request = request.timeout(timeout);
    let response = match config.body {
        Some(body) => request.send_string(&body),
        None => request.call(),
    };
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => response,
        Err(ureq::Error::Transport(err)) => {
            return Err(http_error(
                "transport",
                format!("http transport failed: {err}"),
            ));
        }
    };
    response_to_value(response, config.max_body_bytes)
}

fn default_agent() -> &'static ureq::Agent {
    DEFAULT_AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(DEFAULT_TIMEOUT_MS))
            .timeout_read(Duration::from_millis(DEFAULT_TIMEOUT_MS))
            .timeout_write(Duration::from_millis(DEFAULT_TIMEOUT_MS))
            .build()
    })
}

fn response_to_value(
    response: ureq::Response,
    max_body_bytes: usize,
) -> Result<HttpResponse, HttpError> {
    let status = i64::from(response.status());
    let mut headers = HashMap::new();
    for name in response.headers_names() {
        if let Some(value) = response.header(&name) {
            headers.insert(name.to_ascii_lowercase(), value.to_string());
        }
    }
    let mut bytes = Vec::new();
    let mut reader = response.into_reader().take((max_body_bytes + 1) as u64);
    reader
        .read_to_end(&mut bytes)
        .map_err(|err| http_error("read_failed", format!("http response failed: {err}")))?;
    if bytes.len() > max_body_bytes {
        return Err(http_error(
            "body_too_large",
            format!("http response too large: exceeds {max_body_bytes} bytes"),
        ));
    }
    let body = String::from_utf8(bytes)
        .map_err(|err| http_error("invalid_utf8", format!("http response is not utf-8: {err}")))?;
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn http_error(code: &str, message: impl Into<String>) -> HttpError {
    HttpError {
        code: code.to_string(),
        message: message.into(),
    }
}

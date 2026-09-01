use std::{collections::HashMap, io::Read, sync::OnceLock, time::Duration};

use crate::{
    error::{KuError, KuResult},
    span::Span,
    stdlib::{
        core::{expect_arg_count, expected_type},
        json,
    },
    value::Value,
};

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const MAX_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_MAX_BODY_BYTES: usize = 1_000_000;
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_HEADER_BYTES: i64 = 16 * 1024;
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_CONNECTIONS: i64 = 1024;
pub const MAX_CONNECTIONS: usize = 4096;
const DEFAULT_READ_HEADER_TIMEOUT_MS: i64 = 5_000;
const DEFAULT_READ_BODY_TIMEOUT_MS: i64 = 10_000;
const DEFAULT_WRITE_TIMEOUT_MS: i64 = 10_000;
const DEFAULT_IDLE_TIMEOUT_MS: i64 = 5_000;
const DEFAULT_HANDLER_TIMEOUT_MS: i64 = 15_000;
const DEFAULT_MAX_ACTIVE_REQUESTS: i64 = 256;
pub const MAX_ACTIVE_REQUESTS: usize = 1024;
const DEFAULT_MAX_PENDING_REQUESTS: i64 = 1024;
pub const MAX_PENDING_REQUESTS: usize = 8192;
pub const MAX_CLIENT_IDLE_CONNECTIONS: usize = 1024;
const CLIENT_CONFIG_FIELDS: [&str; 3] = ["timeout_ms", "max_body_bytes", "max_idle_connections"];
const REQUEST_CONFIG_FIELDS: [&str; 6] = [
    "method",
    "url",
    "headers",
    "body",
    "timeout_ms",
    "max_body_bytes",
];
const SERVER_CONFIG_FIELDS: [&str; 10] = [
    "read_header_timeout_ms",
    "read_body_timeout_ms",
    "write_timeout_ms",
    "idle_timeout_ms",
    "handler_timeout_ms",
    "max_body_bytes",
    "max_header_bytes",
    "max_connections",
    "max_active_requests",
    "max_pending_requests",
];
/// Longest accepted request target (the `/path?query` token of the request line).
/// A target over this is answered with 414 before it is copied or routed -- it is
/// never truncated, because truncating would let two distinct long paths collide
/// on the same route. Deliberately a fixed limit, not a service config field.
/// The native backend pins the same value (`KU_HTTP_MAX_TARGET`).
pub const MAX_REQUEST_TARGET_BYTES: usize = 8192;
/// Maximum non-empty path segments accepted by both HTTP runtimes. Native uses a
/// fixed routing scratch array; requests beyond the bound are rejected, never
/// truncated into a shorter route.
pub const MAX_REQUEST_PATH_SEGMENTS: usize = 64;

static DEFAULT_AGENT: OnceLock<ureq::Agent> = OnceLock::new();

pub fn eval(function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    match function {
        "get" => {
            expect_arg_count("http.get", args.len(), 1, span)?;
            let Value::String(url) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            Ok(Some(result_from_http(http_request(HttpRequest {
                method: "GET".to_string(),
                url: url.clone(),
                headers: HashMap::new(),
                body: None,
                timeout_ms: DEFAULT_TIMEOUT_MS,
                max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            }))))
        }
        "post" => {
            expect_arg_count("http.post", args.len(), 2, span)?;
            let Value::String(url) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            let Value::String(body) = &args[1] else {
                return Err(expected_type("str", &args[1], span));
            };
            Ok(Some(result_from_http(http_request(HttpRequest {
                method: "POST".to_string(),
                url: url.clone(),
                headers: HashMap::new(),
                body: Some(body.clone()),
                timeout_ms: DEFAULT_TIMEOUT_MS,
                max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            }))))
        }
        "request" => {
            expect_arg_count("http.request", args.len(), 1, span)?;
            let request = request_from_value(&args[0], span)?;
            Ok(Some(result_from_http(http_request(request))))
        }
        "client" => {
            if args.len() > 1 {
                return Err(KuError::runtime(
                    format!(
                        "http.client expects 0 or 1 arguments but got {}",
                        args.len()
                    ),
                    span,
                ));
            }
            let config = args.first();
            Ok(Some(client_value(config, span)?))
        }
        "text" => {
            if args.is_empty() || args.len() > 2 {
                return Err(KuError::runtime(
                    format!("http.text expects 1 or 2 arguments but got {}", args.len()),
                    span,
                ));
            }
            let (status, body) = text_response_args("http.text", args, span)?;
            Ok(Some(response_helper_value(
                status,
                "text/plain; charset=utf-8",
                body,
            )))
        }
        "html" => {
            if args.is_empty() || args.len() > 2 {
                return Err(KuError::runtime(
                    format!("http.html expects 1 or 2 arguments but got {}", args.len()),
                    span,
                ));
            }
            let (status, body) = text_response_args("http.html", args, span)?;
            Ok(Some(response_helper_value(
                status,
                "text/html; charset=utf-8",
                body,
            )))
        }
        "json" => {
            if args.is_empty() || args.len() > 2 {
                return Err(KuError::runtime(
                    format!("http.json expects 1 or 2 arguments but got {}", args.len()),
                    span,
                ));
            }
            let (status, value) = json_response_args(args, span)?;
            let body = json::stringify_value(value, span)?;
            Ok(Some(response_helper_value(
                status,
                "application/json; charset=utf-8",
                body,
            )))
        }
        "empty" => {
            if args.len() > 1 {
                return Err(KuError::runtime(
                    format!("http.empty expects 0 or 1 arguments but got {}", args.len()),
                    span,
                ));
            }
            let status = optional_status(args.first(), 204, span)?;
            Ok(Some(response_helper_value(status, "", String::new())))
        }
        "redirect" => {
            if args.is_empty() || args.len() > 2 {
                return Err(KuError::runtime(
                    format!(
                        "http.redirect expects 1 or 2 arguments but got {}",
                        args.len()
                    ),
                    span,
                ));
            }
            let (status, location) = redirect_response_args(args, span)?;
            Ok(Some(redirect_response_value(status, location)))
        }
        "statusText" => {
            expect_arg_count("http.statusText", args.len(), 1, span)?;
            let Value::Int(status) = &args[0] else {
                return Err(expected_type("int", &args[0], span));
            };
            Ok(Some(Value::String(status_text(*status).to_string())))
        }
        "service" | "server" => {
            if args.len() > 1 {
                return Err(KuError::runtime(
                    format!(
                        "http.{function} expects 0 or 1 arguments but got {}",
                        args.len()
                    ),
                    span,
                ));
            }
            let config = args.first();
            Ok(Some(server_config_value(config, span)?))
        }
        _ => Ok(None),
    }
}

pub fn status_object_value() -> Value {
    Value::Object(
        HTTP_STATUS_CODES
            .iter()
            .map(|(name, code, _)| ((*name).to_string(), Value::Int(*code)))
            .collect(),
    )
}

pub fn code_object_value() -> Value {
    Value::Object(HashMap::from([
        ("SUCCESS".to_string(), Value::Int(200)),
        ("CREATED".to_string(), Value::Int(201)),
        ("ACCEPTED".to_string(), Value::Int(202)),
        ("NO_CONTENT".to_string(), Value::Int(204)),
        ("BAD_REQUEST".to_string(), Value::Int(400)),
        ("UNAUTHORIZED".to_string(), Value::Int(401)),
        ("FORBIDDEN".to_string(), Value::Int(403)),
        ("NOT_FOUND".to_string(), Value::Int(404)),
        ("VALIDATION_FAILED".to_string(), Value::Int(422)),
        ("INTERNAL_ERROR".to_string(), Value::Int(500)),
    ]))
}

pub fn status_text(status: i64) -> &'static str {
    HTTP_STATUS_CODES
        .iter()
        .find(|(_, code, _)| *code == status)
        .map(|(_, _, text)| *text)
        .unwrap_or("Unknown")
}

const HTTP_STATUS_CODES: &[(&str, i64, &str)] = &[
    ("ok", 200, "OK"),
    ("created", 201, "Created"),
    ("accepted", 202, "Accepted"),
    ("noContent", 204, "No Content"),
    ("movedPermanently", 301, "Moved Permanently"),
    ("found", 302, "Found"),
    ("seeOther", 303, "See Other"),
    ("notModified", 304, "Not Modified"),
    ("temporaryRedirect", 307, "Temporary Redirect"),
    ("permanentRedirect", 308, "Permanent Redirect"),
    ("badRequest", 400, "Bad Request"),
    ("unauthorized", 401, "Unauthorized"),
    ("forbidden", 403, "Forbidden"),
    ("notFound", 404, "Not Found"),
    ("methodNotAllowed", 405, "Method Not Allowed"),
    ("notAcceptable", 406, "Not Acceptable"),
    ("requestTimeout", 408, "Request Timeout"),
    ("conflict", 409, "Conflict"),
    ("gone", 410, "Gone"),
    ("contentTooLarge", 413, "Content Too Large"),
    ("uriTooLong", 414, "URI Too Long"),
    ("unsupportedMedia", 415, "Unsupported Media Type"),
    ("rangeNotSatisfiable", 416, "Range Not Satisfiable"),
    ("expectationFailed", 417, "Expectation Failed"),
    ("unprocessable", 422, "Unprocessable Content"),
    ("tooManyRequests", 429, "Too Many Requests"),
    ("headerTooLarge", 431, "Request Header Fields Too Large"),
    ("internalError", 500, "Internal Server Error"),
    ("notImplemented", 501, "Not Implemented"),
    ("badGateway", 502, "Bad Gateway"),
    ("serviceUnavailable", 503, "Service Unavailable"),
    ("gatewayTimeout", 504, "Gateway Timeout"),
];

#[derive(Debug)]
struct HttpRequest {
    method: String,
    url: String,
    headers: HashMap<String, String>,
    body: Option<String>,
    timeout_ms: u64,
    max_body_bytes: usize,
}

#[derive(Debug)]
struct HttpResponse {
    status: i64,
    headers: HashMap<String, String>,
    body: String,
}

#[derive(Debug)]
struct HttpError {
    code: String,
    message: String,
}

fn http_request(config: HttpRequest) -> Result<HttpResponse, HttpError> {
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

    let mut request = default_agent()
        .request(&method, &config.url)
        .timeout(Duration::from_millis(config.timeout_ms));
    for (name, value) in &config.headers {
        request = request.set(name, value);
    }
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

fn request_from_value(value: &Value, span: Span) -> KuResult<HttpRequest> {
    let Value::Object(fields) = value else {
        return Err(expected_type("object", value, span));
    };
    reject_unknown_config_fields(fields, &REQUEST_CONFIG_FIELDS, span)?;
    let url = required_string(fields, "url", span)?;
    let method = optional_string(fields, "method", "GET", span)?;
    let body = optional_string_value(fields, "body", span)?;
    let headers = optional_headers(fields, span)?;
    let timeout_ms = optional_bounded_int(
        fields,
        "timeout_ms",
        DEFAULT_TIMEOUT_MS as i64,
        MAX_TIMEOUT_MS as i64,
        span,
    )? as u64;
    let max_body_bytes = optional_bounded_int(
        fields,
        "max_body_bytes",
        DEFAULT_MAX_BODY_BYTES as i64,
        MAX_BODY_BYTES as i64,
        span,
    )? as usize;
    Ok(HttpRequest {
        method,
        url,
        headers,
        body,
        timeout_ms,
        max_body_bytes,
    })
}

fn required_string(fields: &HashMap<String, Value>, name: &str, span: Span) -> KuResult<String> {
    match fields.get(name) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(other) => Err(KuError::runtime(
            format!(
                "type error: http.request field '{name}' must be str but got {}",
                other.type_name()
            ),
            span,
        )),
        None => Err(KuError::runtime(
            format!("http.request missing required field '{name}'"),
            span,
        )),
    }
}

fn optional_string(
    fields: &HashMap<String, Value>,
    name: &str,
    default: &str,
    span: Span,
) -> KuResult<String> {
    match optional_string_value(fields, name, span)? {
        Some(value) => Ok(value),
        None => Ok(default.to_string()),
    }
}

fn optional_string_value(
    fields: &HashMap<String, Value>,
    name: &str,
    span: Span,
) -> KuResult<Option<String>> {
    match fields.get(name) {
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(other) => Err(KuError::runtime(
            format!(
                "type error: http.request field '{name}' must be str but got {}",
                other.type_name()
            ),
            span,
        )),
    }
}

fn optional_int(
    fields: &HashMap<String, Value>,
    name: &str,
    default: i64,
    span: Span,
) -> KuResult<i64> {
    match fields.get(name) {
        Some(Value::Int(value)) if *value > 0 => Ok(*value),
        Some(Value::Int(_)) => Err(KuError::runtime(
            format!("http config field '{name}' must be a positive int"),
            span,
        )),
        Some(Value::Null) | None => Ok(default),
        Some(other) => Err(KuError::runtime(
            format!(
                "type error: http.request field '{name}' must be int but got {}",
                other.type_name()
            ),
            span,
        )),
    }
}

fn optional_headers(
    fields: &HashMap<String, Value>,
    span: Span,
) -> KuResult<HashMap<String, String>> {
    let Some(value) = fields.get("headers") else {
        return Ok(HashMap::new());
    };
    let Value::Object(headers) = value else {
        return Err(KuError::runtime(
            format!(
                "type error: http.request field 'headers' must be object but got {}",
                value.type_name()
            ),
            span,
        ));
    };
    let mut output = HashMap::new();
    for (name, value) in headers {
        let Value::String(value) = value else {
            return Err(KuError::runtime(
                format!(
                    "type error: http.request header '{name}' must be str but got {}",
                    value.type_name()
                ),
                span,
            ));
        };
        if name.is_empty() || !name.bytes().all(is_http_token_byte) {
            return Err(KuError::runtime(
                format!("http.request header name '{name}' is not a valid HTTP token"),
                span,
            ));
        }
        if !is_safe_http_field_value(value) {
            return Err(KuError::runtime(
                format!("http.request header '{name}' contains control characters"),
                span,
            ));
        }
        let normalized = name.to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "host"
                | "content-length"
                | "transfer-encoding"
                | "connection"
                | "proxy-connection"
                | "keep-alive"
                | "te"
                | "trailer"
                | "upgrade"
                | "expect"
        ) {
            return Err(KuError::runtime(
                format!("http.request header '{name}' is managed by the HTTP transport"),
                span,
            ));
        }
        if output.insert(normalized, value.clone()).is_some() {
            return Err(KuError::runtime(
                format!("duplicate http.request header '{name}'"),
                span,
            ));
        }
    }
    Ok(output)
}

fn result_from_http(response: Result<HttpResponse, HttpError>) -> Value {
    match response {
        Ok(response) => ok_value(response_value(response)),
        Err(error) => err_value(error_value("http", &error.code, &error.message)),
    }
}

fn text_response_args(name: &str, args: &[Value], span: Span) -> KuResult<(i64, String)> {
    match args {
        [Value::String(body)] => Ok((200, body.clone())),
        [Value::Int(status), Value::String(body)] => {
            Ok((validate_status(*status, span)?, body.clone()))
        }
        [first, _] => Err(expected_type(
            "int status as the first argument",
            first,
            span,
        )),
        [other] => Err(expected_type("str", other, span)),
        _ => Err(KuError::runtime(
            format!("{name} expects 1 or 2 arguments but got {}", args.len()),
            span,
        )),
    }
}

fn json_response_args(args: &[Value], span: Span) -> KuResult<(i64, &Value)> {
    match args {
        [value] => Ok((200, value)),
        [Value::Int(status), value] => Ok((validate_status(*status, span)?, value)),
        [first, _] => Err(expected_type(
            "int status as the first argument",
            first,
            span,
        )),
        _ => Err(KuError::runtime(
            format!("http.json expects 1 or 2 arguments but got {}", args.len()),
            span,
        )),
    }
}

fn redirect_response_args(args: &[Value], span: Span) -> KuResult<(i64, String)> {
    let (status, location) = match args {
        [Value::String(location)] => (302, location.clone()),
        [Value::Int(status), Value::String(location)] => {
            let status = validate_status(*status, span)?;
            if !matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Err(KuError::runtime(
                    "http redirect status must be 301, 302, 303, 307, or 308",
                    span,
                ));
            }
            (status, location.clone())
        }
        [first, _] => {
            return Err(expected_type(
                "int redirect status as the first argument",
                first,
                span,
            ))
        }
        [other] => return Err(expected_type("str", other, span)),
        _ => {
            return Err(KuError::runtime(
                format!(
                    "http.redirect expects 1 or 2 arguments but got {}",
                    args.len()
                ),
                span,
            ))
        }
    };
    if !is_safe_http_field_value(&location) {
        return Err(KuError::runtime(
            "http redirect location must not contain control characters",
            span,
        ));
    }
    Ok((status, location))
}

fn optional_bounded_int(
    fields: &HashMap<String, Value>,
    name: &str,
    default: i64,
    maximum: i64,
    span: Span,
) -> KuResult<i64> {
    let value = optional_int(fields, name, default, span)?;
    if value > maximum {
        return Err(KuError::runtime(
            format!("http config field '{name}' must be at most {maximum}"),
            span,
        ));
    }
    Ok(value)
}

fn is_safe_http_field_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (byte >= 0x20 && byte != 0x7f))
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn optional_status(value: Option<&Value>, default: i64, span: Span) -> KuResult<i64> {
    match value {
        Some(Value::Int(status)) => validate_status(*status, span),
        Some(other) => Err(expected_type("int", other, span)),
        None => Ok(default),
    }
}

fn validate_status(status: i64, span: Span) -> KuResult<i64> {
    if (100..=599).contains(&status) {
        Ok(status)
    } else {
        Err(KuError::runtime(
            "http status must be between 100 and 599",
            span,
        ))
    }
}

fn response_helper_value(status: i64, content_type: &str, body: String) -> Value {
    let headers = if content_type.is_empty() {
        HashMap::new()
    } else {
        HashMap::from([(
            "content-type".to_string(),
            Value::String(content_type.to_string()),
        )])
    };
    Value::Object(HashMap::from([
        ("status".to_string(), Value::Int(status)),
        ("headers".to_string(), Value::Object(headers)),
        ("body".to_string(), Value::String(body)),
    ]))
}

fn redirect_response_value(status: i64, location: String) -> Value {
    Value::Object(HashMap::from([
        ("status".to_string(), Value::Int(status)),
        (
            "headers".to_string(),
            Value::Object(HashMap::from([(
                "location".to_string(),
                Value::String(location),
            )])),
        ),
        ("body".to_string(), Value::String(String::new())),
    ]))
}

fn client_value(config: Option<&Value>, span: Span) -> KuResult<Value> {
    let mut timeout_ms = DEFAULT_TIMEOUT_MS as i64;
    let mut max_body_bytes = DEFAULT_MAX_BODY_BYTES as i64;
    let mut max_idle_connections = 128_i64;
    if let Some(config) = config {
        let Value::Object(fields) = config else {
            return Err(expected_type("object", config, span));
        };
        reject_unknown_config_fields(fields, &CLIENT_CONFIG_FIELDS, span)?;
        timeout_ms = optional_bounded_int(
            fields,
            "timeout_ms",
            timeout_ms,
            MAX_TIMEOUT_MS as i64,
            span,
        )?;
        max_body_bytes = optional_bounded_int(
            fields,
            "max_body_bytes",
            max_body_bytes,
            MAX_BODY_BYTES as i64,
            span,
        )?;
        max_idle_connections = optional_bounded_int(
            fields,
            "max_idle_connections",
            max_idle_connections,
            MAX_CLIENT_IDLE_CONNECTIONS as i64,
            span,
        )?;
    }
    Ok(Value::Object(HashMap::from([
        ("kind".to_string(), Value::String("http.client".to_string())),
        ("timeout_ms".to_string(), Value::Int(timeout_ms)),
        ("max_body_bytes".to_string(), Value::Int(max_body_bytes)),
        (
            "max_idle_connections".to_string(),
            Value::Int(max_idle_connections),
        ),
    ])))
}

fn server_config_value(config: Option<&Value>, span: Span) -> KuResult<Value> {
    let mut read_header_timeout_ms = DEFAULT_READ_HEADER_TIMEOUT_MS;
    let mut read_body_timeout_ms = DEFAULT_READ_BODY_TIMEOUT_MS;
    let mut write_timeout_ms = DEFAULT_WRITE_TIMEOUT_MS;
    let mut idle_timeout_ms = DEFAULT_IDLE_TIMEOUT_MS;
    let mut handler_timeout_ms = DEFAULT_HANDLER_TIMEOUT_MS;
    let mut max_body_bytes = DEFAULT_MAX_BODY_BYTES as i64;
    let mut max_header_bytes = DEFAULT_MAX_HEADER_BYTES;
    let mut max_connections = DEFAULT_MAX_CONNECTIONS;
    let mut max_active_requests = DEFAULT_MAX_ACTIVE_REQUESTS;
    let mut max_pending_requests = DEFAULT_MAX_PENDING_REQUESTS;
    if let Some(config) = config {
        let Value::Object(fields) = config else {
            return Err(expected_type("object", config, span));
        };
        reject_unknown_config_fields(fields, &SERVER_CONFIG_FIELDS, span)?;
        read_header_timeout_ms = optional_bounded_int(
            fields,
            "read_header_timeout_ms",
            read_header_timeout_ms,
            MAX_TIMEOUT_MS as i64,
            span,
        )?;
        read_body_timeout_ms = optional_bounded_int(
            fields,
            "read_body_timeout_ms",
            read_body_timeout_ms,
            MAX_TIMEOUT_MS as i64,
            span,
        )?;
        write_timeout_ms = optional_bounded_int(
            fields,
            "write_timeout_ms",
            write_timeout_ms,
            MAX_TIMEOUT_MS as i64,
            span,
        )?;
        idle_timeout_ms = optional_bounded_int(
            fields,
            "idle_timeout_ms",
            idle_timeout_ms,
            MAX_TIMEOUT_MS as i64,
            span,
        )?;
        handler_timeout_ms = optional_bounded_int(
            fields,
            "handler_timeout_ms",
            handler_timeout_ms,
            MAX_TIMEOUT_MS as i64,
            span,
        )?;
        max_body_bytes = optional_bounded_int(
            fields,
            "max_body_bytes",
            max_body_bytes,
            MAX_BODY_BYTES as i64,
            span,
        )?;
        max_header_bytes = optional_bounded_int(
            fields,
            "max_header_bytes",
            max_header_bytes,
            MAX_HEADER_BYTES as i64,
            span,
        )?;
        max_connections = optional_bounded_int(
            fields,
            "max_connections",
            max_connections,
            MAX_CONNECTIONS as i64,
            span,
        )?;
        max_active_requests = optional_bounded_int(
            fields,
            "max_active_requests",
            max_active_requests,
            MAX_ACTIVE_REQUESTS as i64,
            span,
        )?;
        max_pending_requests = optional_bounded_int(
            fields,
            "max_pending_requests",
            max_pending_requests,
            MAX_PENDING_REQUESTS as i64,
            span,
        )?;
    }
    Ok(Value::Object(HashMap::from([
        (
            "kind".to_string(),
            Value::String("http.service".to_string()),
        ),
        (
            "read_header_timeout_ms".to_string(),
            Value::Int(read_header_timeout_ms),
        ),
        (
            "read_body_timeout_ms".to_string(),
            Value::Int(read_body_timeout_ms),
        ),
        ("write_timeout_ms".to_string(), Value::Int(write_timeout_ms)),
        ("idle_timeout_ms".to_string(), Value::Int(idle_timeout_ms)),
        (
            "handler_timeout_ms".to_string(),
            Value::Int(handler_timeout_ms),
        ),
        ("max_body_bytes".to_string(), Value::Int(max_body_bytes)),
        ("max_header_bytes".to_string(), Value::Int(max_header_bytes)),
        ("max_connections".to_string(), Value::Int(max_connections)),
        (
            "max_active_requests".to_string(),
            Value::Int(max_active_requests),
        ),
        (
            "max_pending_requests".to_string(),
            Value::Int(max_pending_requests),
        ),
        ("routes".to_string(), Value::Array(Vec::new())),
    ])))
}

fn reject_unknown_config_fields(
    fields: &HashMap<String, Value>,
    allowed: &[&str],
    span: Span,
) -> KuResult<()> {
    let mut unknown = fields
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .collect::<Vec<_>>();
    unknown.sort();
    if let Some(key) = unknown.first() {
        return Err(KuError::runtime(
            format!("unknown http config field '{key}'"),
            span,
        ));
    }
    Ok(())
}

fn response_value(response: HttpResponse) -> Value {
    let headers = response
        .headers
        .into_iter()
        .map(|(name, value)| (name, Value::String(value)))
        .collect::<HashMap<_, _>>();
    Value::Object(HashMap::from([
        ("status".to_string(), Value::Int(response.status)),
        ("headers".to_string(), Value::Object(headers)),
        ("body".to_string(), Value::String(response.body)),
    ]))
}

fn error_value(domain: &str, code: &str, message: &str) -> Value {
    Value::Object(HashMap::from([
        ("domain".to_string(), Value::String(domain.to_string())),
        ("code".to_string(), Value::String(code.to_string())),
        ("message".to_string(), Value::String(message.to_string())),
    ]))
}

fn http_error(code: &str, message: impl Into<String>) -> HttpError {
    HttpError {
        code: code.to_string(),
        message: message.into(),
    }
}

fn ok_value(value: Value) -> Value {
    Value::Result {
        ok: true,
        value: Box::new(value),
    }
}

fn err_value(value: Value) -> Value {
    Value::Result {
        ok: false,
        value: Box::new(value),
    }
}

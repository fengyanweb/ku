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
const MIN_TIMEOUT_MS: u64 = 1;
const MAX_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_BODY_BYTES: usize = 1_000_000;
const DEFAULT_MAX_HEADER_BYTES: i64 = 16 * 1024;
const DEFAULT_MAX_CONNECTIONS: i64 = 1024;
const DEFAULT_MAX_CONCURRENCY: i64 = 256;

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
            let Value::String(body) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            let status = optional_status(args.get(1), span)?;
            Ok(Some(response_helper_value(
                status,
                "text/plain; charset=utf-8",
                body.clone(),
            )))
        }
        "json" => {
            if args.is_empty() || args.len() > 2 {
                return Err(KuError::runtime(
                    format!("http.json expects 1 or 2 arguments but got {}", args.len()),
                    span,
                ));
            }
            let status = optional_status(args.get(1), span)?;
            let body = json::stringify_value(&args[0], span)?;
            Ok(Some(response_helper_value(
                status,
                "application/json",
                body,
            )))
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

pub fn default_server_value(span: Span) -> KuResult<Value> {
    server_config_value(None, span)
}

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
    if !config.url.starts_with("http://") && !config.url.starts_with("https://") {
        return Err(http_error(
            "invalid_url",
            "http url must start with http:// or https://",
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
    let url = required_string(fields, "url", span)?;
    let method = optional_string(fields, "method", "GET", span)?;
    let body = optional_string_value(fields, "body", span)?;
    let headers = optional_headers(fields, span)?;
    let timeout_ms = optional_int(fields, "timeout_ms", DEFAULT_TIMEOUT_MS as i64, span)?;
    let timeout_ms = u64::try_from(timeout_ms)
        .ok()
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
    let max_body_bytes = optional_int(
        fields,
        "max_body_bytes",
        DEFAULT_MAX_BODY_BYTES as i64,
        span,
    )?;
    let max_body_bytes = usize::try_from(max_body_bytes)
        .ok()
        .filter(|value| *value > 0 && *value <= DEFAULT_MAX_BODY_BYTES)
        .unwrap_or(DEFAULT_MAX_BODY_BYTES);
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
        Some(Value::Int(value)) => Ok(*value),
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
        output.insert(name.clone(), value.clone());
    }
    Ok(output)
}

fn result_from_http(response: Result<HttpResponse, HttpError>) -> Value {
    match response {
        Ok(response) => ok_value(response_value(response)),
        Err(error) => err_value(error_value("http", &error.code, &error.message)),
    }
}

fn optional_status(value: Option<&Value>, span: Span) -> KuResult<i64> {
    match value {
        Some(Value::Int(status)) if (100..=599).contains(status) => Ok(*status),
        Some(Value::Int(_)) => Err(KuError::runtime(
            "http status must be between 100 and 599",
            span,
        )),
        Some(other) => Err(expected_type("int", other, span)),
        None => Ok(200),
    }
}

fn response_helper_value(status: i64, content_type: &str, body: String) -> Value {
    Value::Object(HashMap::from([
        ("status".to_string(), Value::Int(status)),
        (
            "headers".to_string(),
            Value::Object(HashMap::from([(
                "content-type".to_string(),
                Value::String(content_type.to_string()),
            )])),
        ),
        ("body".to_string(), Value::String(body)),
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
        timeout_ms = optional_int(fields, "timeout_ms", timeout_ms, span)?;
        max_body_bytes = optional_int(fields, "max_body_bytes", max_body_bytes, span)?;
        max_idle_connections =
            optional_int(fields, "max_idle_connections", max_idle_connections, span)?;
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
    let mut read_timeout_ms = DEFAULT_TIMEOUT_MS as i64;
    let mut write_timeout_ms = DEFAULT_TIMEOUT_MS as i64;
    let mut max_body_bytes = DEFAULT_MAX_BODY_BYTES as i64;
    let mut max_header_bytes = DEFAULT_MAX_HEADER_BYTES;
    let mut max_connections = DEFAULT_MAX_CONNECTIONS;
    let mut max_concurrency = DEFAULT_MAX_CONCURRENCY;
    if let Some(config) = config {
        let Value::Object(fields) = config else {
            return Err(expected_type("object", config, span));
        };
        read_timeout_ms = optional_int(fields, "read_timeout_ms", read_timeout_ms, span)?;
        write_timeout_ms = optional_int(fields, "write_timeout_ms", write_timeout_ms, span)?;
        max_body_bytes = optional_int(fields, "max_body_bytes", max_body_bytes, span)?;
        max_header_bytes = optional_int(fields, "max_header_bytes", max_header_bytes, span)?;
        max_connections = optional_int(fields, "max_connections", max_connections, span)?;
        max_concurrency = optional_int(fields, "max_concurrency", max_concurrency, span)?;
    }
    Ok(Value::Object(HashMap::from([
        (
            "kind".to_string(),
            Value::String("http.service".to_string()),
        ),
        ("read_timeout_ms".to_string(), Value::Int(read_timeout_ms)),
        ("write_timeout_ms".to_string(), Value::Int(write_timeout_ms)),
        ("max_body_bytes".to_string(), Value::Int(max_body_bytes)),
        ("max_header_bytes".to_string(), Value::Int(max_header_bytes)),
        ("max_connections".to_string(), Value::Int(max_connections)),
        ("max_concurrency".to_string(), Value::Int(max_concurrency)),
        ("routes".to_string(), Value::Array(Vec::new())),
    ])))
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

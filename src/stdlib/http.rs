use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

use crate::{
    error::{KuError, KuResult},
    span::Span,
    value::Value,
};

const MAX_HTTP_BODY_BYTES: usize = 1_000_000;
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

pub fn eval(function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    match function {
        "try_get" => {
            let url = expect_str(args, 0, span)?;
            let result = match http_get(url) {
                Ok(body) => result_value(Value::String(body), true),
                Err(message) => result_value(Value::String(message), false),
            };
            Ok(Some(result))
        }
        _ => Ok(None),
    }
}

fn http_get(url: &str) -> Result<String, String> {
    let target = parse_http_url(url)?;
    let mut stream = TcpStream::connect((target.host.as_str(), target.port))
        .map_err(|err| format!("http connection failed: {err}"))?;
    stream
        .set_read_timeout(Some(HTTP_TIMEOUT))
        .map_err(|err| format!("http read timeout setup failed: {err}"))?;
    stream
        .set_write_timeout(Some(HTTP_TIMEOUT))
        .map_err(|err| format!("http write timeout setup failed: {err}"))?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: text/plain, application/json, */*\r\n\r\n",
        target.path, target.host
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("http request failed: {err}"))?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|err| format!("http response failed: {err}"))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_HTTP_BODY_BYTES {
            return Err(format!(
                "http response too large: exceeds {MAX_HTTP_BODY_BYTES} bytes"
            ));
        }
    }
    let response =
        String::from_utf8(bytes).map_err(|err| format!("http response is not utf-8: {err}"))?;
    let Some((head, body)) = response.split_once("\r\n\r\n") else {
        return Err("http response is missing headers".to_string());
    };
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200 ") {
        return Err(format!("http request failed with status '{status}'"));
    }
    Ok(body.to_string())
}

struct HttpTarget {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<HttpTarget, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "http.try_get currently supports only http:// URLs".to_string())?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() {
        return Err("http url is missing host".to_string());
    }
    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        let port = port
            .parse::<u16>()
            .map_err(|_| format!("invalid http port '{port}'"))?;
        (host, port)
    } else {
        (authority, 80)
    };
    if host.is_empty() {
        return Err("http url is missing host".to_string());
    }
    Ok(HttpTarget {
        host: host.to_string(),
        port,
        path: format!("/{path}"),
    })
}

fn expect_str(args: &[Value], index: usize, span: Span) -> KuResult<&str> {
    match args.get(index) {
        Some(Value::String(value)) => Ok(value),
        Some(other) => Err(KuError::runtime(
            format!("type error: expected str but got {}", other.type_name()),
            span,
        )),
        None => Err(KuError::runtime("missing argument", span)),
    }
}

fn result_value(value: Value, ok: bool) -> Value {
    Value::Result {
        ok,
        value: Box::new(value),
    }
}

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    error::{KuError, KuResult},
    span::Span,
    stdlib::core::{expect_arg_count, expected_type},
    value::Value,
};

const MAX_CONFIG_BYTES: u64 = 1_000_000;

pub fn eval(
    function: &str,
    args: &[Value],
    span: Span,
    base_dir: &Path,
) -> KuResult<Option<Value>> {
    match function {
        "env" => {
            expect_arg_count("config.env", args.len(), 0, span)?;
            Ok(Some(read_env_file(&base_dir.join(".env"), false, span)?))
        }
        "env_file" => {
            expect_arg_count("config.env_file", args.len(), 1, span)?;
            let Value::String(path) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            Ok(Some(read_env_file(
                &resolve_path(base_dir, path),
                true,
                span,
            )?))
        }
        "yaml" => {
            expect_arg_count("config.yaml", args.len(), 1, span)?;
            let Value::String(path) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            Ok(Some(result_from_config(read_yaml_file(
                &resolve_path(base_dir, path),
                span,
            ))))
        }
        _ => Ok(None),
    }
}

fn read_env_file(path: &Path, explicit: bool, span: Span) -> KuResult<Value> {
    if !explicit && !path.exists() {
        return Ok(Value::Object(HashMap::new()));
    }
    match read_limited(path, span) {
        Ok(text) => parse_env(&text, span),
        Err(err) => Err(err),
    }
}

fn read_yaml_file(path: &Path, span: Span) -> Result<Value, Value> {
    read_limited(path, span)
        .and_then(|text| parse_yaml(&text, span))
        .map_err(|err| error_value("config", "read_failed", &err.message))
}

fn read_limited(path: &Path, span: Span) -> KuResult<String> {
    let meta = fs::metadata(path).map_err(|err| {
        KuError::runtime(
            format!("config file '{}' cannot be read: {err}", path.display()),
            span,
        )
    })?;
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(KuError::runtime(
            format!(
                "config file '{}' exceeds {} bytes",
                path.display(),
                MAX_CONFIG_BYTES
            ),
            span,
        ));
    }
    fs::read_to_string(path).map_err(|err| {
        KuError::runtime(
            format!("config file '{}' cannot be read: {err}", path.display()),
            span,
        )
    })
}

fn parse_env(text: &str, span: Span) -> KuResult<Value> {
    let mut fields = HashMap::new();
    for (line_index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(KuError::runtime(
                format!("invalid .env line {}: expected KEY=value", line_index + 1),
                span,
            ));
        };
        let name = name.trim();
        if !is_config_key(name) {
            return Err(KuError::runtime(
                format!("invalid .env key '{}' on line {}", name, line_index + 1),
                span,
            ));
        }
        fields.insert(
            name.to_string(),
            Value::String(unquote(value.trim(), span)?),
        );
    }
    Ok(Value::Object(fields))
}

fn parse_yaml(text: &str, span: Span) -> KuResult<Value> {
    let mut fields = HashMap::new();
    for (line_index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if raw.starts_with(' ') || raw.starts_with('\t') {
            return Err(KuError::runtime(
                format!(
                    "config.yaml line {} uses nesting; only flat key: value is supported now",
                    line_index + 1
                ),
                span,
            ));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(KuError::runtime(
                format!("invalid yaml line {}: expected key: value", line_index + 1),
                span,
            ));
        };
        let name = name.trim();
        if !is_config_key(name) {
            return Err(KuError::runtime(
                format!("invalid yaml key '{}' on line {}", name, line_index + 1),
                span,
            ));
        }
        fields.insert(name.to_string(), parse_scalar(value.trim(), span)?);
    }
    Ok(Value::Object(fields))
}

fn parse_scalar(value: &str, span: Span) -> KuResult<Value> {
    if value == "null" {
        return Ok(Value::Null);
    }
    if value == "true" {
        return Ok(Value::Bool(true));
    }
    if value == "false" {
        return Ok(Value::Bool(false));
    }
    if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        return Ok(Value::String(unquote(value, span)?));
    }
    if let Ok(value) = value.parse::<i64>() {
        return Ok(Value::Int(value));
    }
    if let Ok(value) = value.parse::<f64>() {
        return Ok(Value::Float(value));
    }
    Ok(Value::String(value.to_string()))
}

fn unquote(value: &str, span: Span) -> KuResult<String> {
    if value.len() < 2 {
        return Ok(value.to_string());
    }
    let quote = value.as_bytes()[0] as char;
    if (quote != '"' && quote != '\'') || !value.ends_with(quote) {
        return Ok(value.to_string());
    }
    let inner = &value[1..value.len() - 1];
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(next) = chars.next() else {
            return Err(KuError::runtime(
                "invalid trailing escape in config string",
                span,
            ));
        };
        match next {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '\'' => out.push('\''),
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Ok(out)
}

fn is_config_key(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn resolve_path(base_dir: &Path, input: &str) -> PathBuf {
    let path = PathBuf::from(input);
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}

fn result_from_config(value: Result<Value, Value>) -> Value {
    match value {
        Ok(value) => Value::Result {
            ok: true,
            value: Box::new(value),
        },
        Err(value) => Value::Result {
            ok: false,
            value: Box::new(value),
        },
    }
}

fn error_value(domain: &str, code: &str, message: &str) -> Value {
    Value::Object(HashMap::from([
        ("domain".to_string(), Value::String(domain.to_string())),
        ("code".to_string(), Value::String(code.to_string())),
        ("message".to_string(), Value::String(message.to_string())),
    ]))
}

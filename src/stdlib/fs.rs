use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    error::{KuError, KuResult},
    span::Span,
    stdlib::{
        core::{expect_arg_count, expected_type},
        errors,
    },
    value::Value,
};

const MAX_READ_BYTES: u64 = 1_000_000;
const MAX_WRITE_BYTES: usize = 1_000_000;

pub fn eval(
    function: &str,
    args: &[Value],
    span: Span,
    base_dir: &Path,
) -> KuResult<Option<Value>> {
    match function {
        "read" => read(args, span, base_dir).map(Some),
        "try_read" => try_read(args, span, base_dir).map(Some),
        "write" => write(args, span, base_dir).map(Some),
        "try_write" => try_write(args, span, base_dir).map(Some),
        _ => Ok(None),
    }
}

fn read(args: &[Value], span: Span, base_dir: &Path) -> KuResult<Value> {
    expect_arg_count("fs.read", args.len(), 1, span)?;
    let Value::String(path) = &args[0] else {
        return Err(expected_type("str", &args[0], span));
    };
    let resolved = resolve_read_path(base_dir, path);
    let display_path = resolved.display().to_string();
    let metadata = fs::metadata(&resolved)
        .map_err(|err| KuError::runtime(format!("failed to read '{display_path}': {err}"), span))?;
    if metadata.len() > MAX_READ_BYTES {
        return Err(KuError::runtime(
            format!("failed to read '{display_path}': file is too large"),
            span,
        ));
    }
    let text = fs::read_to_string(&resolved)
        .map_err(|err| KuError::runtime(format!("failed to read '{display_path}': {err}"), span))?;
    Ok(Value::String(text))
}

fn try_read(args: &[Value], span: Span, base_dir: &Path) -> KuResult<Value> {
    expect_arg_count("fs.try_read", args.len(), 1, span)?;
    let Value::String(path) = &args[0] else {
        return Err(expected_type("str", &args[0], span));
    };
    let resolved = resolve_read_path(base_dir, path);
    let display_path = resolved.display().to_string();
    let metadata = match fs::metadata(&resolved) {
        Ok(metadata) => metadata,
        Err(err) => {
            return Ok(errors::err(
                "fs",
                "read_failed",
                format!("failed to read '{display_path}': {err}"),
            ));
        }
    };
    if metadata.len() > MAX_READ_BYTES {
        return Ok(errors::err(
            "fs",
            "file_too_large",
            format!("failed to read '{display_path}': file is too large"),
        ));
    }
    match fs::read_to_string(&resolved) {
        Ok(text) => Ok(errors::ok(Value::String(text))),
        Err(err) => Ok(errors::err(
            "fs",
            "read_failed",
            format!("failed to read '{display_path}': {err}"),
        )),
    }
}

fn write(args: &[Value], span: Span, base_dir: &Path) -> KuResult<Value> {
    expect_arg_count("fs.write", args.len(), 2, span)?;
    let (path, text) = expect_write_args(args, span)?;
    let resolved = resolve_path(base_dir, path);
    let display_path = resolved.display().to_string();
    if text.len() > MAX_WRITE_BYTES {
        return Err(KuError::runtime(
            format!("failed to write '{display_path}': content is too large"),
            span,
        ));
    }
    fs::write(&resolved, text).map_err(|err| {
        KuError::runtime(format!("failed to write '{display_path}': {err}"), span)
    })?;
    Ok(Value::Null)
}

fn try_write(args: &[Value], span: Span, base_dir: &Path) -> KuResult<Value> {
    expect_arg_count("fs.try_write", args.len(), 2, span)?;
    let (path, text) = expect_write_args(args, span)?;
    let resolved = resolve_path(base_dir, path);
    let display_path = resolved.display().to_string();
    if text.len() > MAX_WRITE_BYTES {
        return Ok(errors::err(
            "fs",
            "content_too_large",
            format!("failed to write '{display_path}': content is too large"),
        ));
    }
    match fs::write(&resolved, text) {
        Ok(()) => Ok(errors::ok(Value::Null)),
        Err(err) => Ok(errors::err(
            "fs",
            "write_failed",
            format!("failed to write '{display_path}': {err}"),
        )),
    }
}

fn expect_write_args(args: &[Value], span: Span) -> KuResult<(&str, &str)> {
    let Value::String(path) = &args[0] else {
        return Err(expected_type("str", &args[0], span));
    };
    let Value::String(text) = &args[1] else {
        return Err(expected_type("str", &args[1], span));
    };
    Ok((path, text))
}

fn resolve_read_path(base_dir: &Path, path: &str) -> PathBuf {
    resolve_path(base_dir, path)
}

fn resolve_path(base_dir: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

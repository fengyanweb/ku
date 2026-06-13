use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    error::{KuError, KuResult},
    span::Span,
    stdlib::core::{expect_arg_count, expected_type},
    value::Value,
};

const MAX_READ_BYTES: u64 = 1_000_000;

pub fn eval(
    function: &str,
    args: &[Value],
    span: Span,
    base_dir: &Path,
) -> KuResult<Option<Value>> {
    match function {
        "read" => read(args, span, base_dir).map(Some),
        "try_read" => try_read(args, span, base_dir).map(Some),
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
            return Ok(Value::Result {
                ok: false,
                value: Box::new(Value::String(format!(
                    "failed to read '{display_path}': {err}"
                ))),
            });
        }
    };
    if metadata.len() > MAX_READ_BYTES {
        return Err(KuError::runtime(
            format!("failed to read '{display_path}': file is too large"),
            span,
        ));
    }
    match fs::read_to_string(&resolved) {
        Ok(text) => Ok(Value::Result {
            ok: true,
            value: Box::new(Value::String(text)),
        }),
        Err(err) => Ok(Value::Result {
            ok: false,
            value: Box::new(Value::String(format!(
                "failed to read '{display_path}': {err}"
            ))),
        }),
    }
}

fn resolve_read_path(base_dir: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

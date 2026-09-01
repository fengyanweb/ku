use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    error::KuResult,
    span::Span,
    stdlib::{
        core::{expect_arg_count, expected_type},
        errors,
    },
    value::Value,
};

const MAX_READ_BYTES: u64 = 1_000_000;
const MAX_WRITE_BYTES: usize = 1_000_000;
const MAX_PATH_BYTES: usize = 32 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_DIRECTORY_OUTPUT_BYTES: usize = 1_000_000;

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
        "exists" => exists(args, span, base_dir).map(Some),
        "read_dir" => read_dir(args, span, base_dir).map(Some),
        _ => Ok(None),
    }
}

fn exists(args: &[Value], span: Span, base_dir: &Path) -> KuResult<Value> {
    expect_arg_count("fs.exists", args.len(), 1, span)?;
    let Value::String(path) = &args[0] else {
        return Err(expected_type("str", &args[0], span));
    };
    if path.len() > MAX_PATH_BYTES {
        return Ok(Value::Bool(false));
    }
    let resolved = resolve_path(base_dir, path);
    if resolved.to_string_lossy().len() > MAX_PATH_BYTES {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(fs::metadata(resolved).is_ok()))
}

fn read_dir(args: &[Value], span: Span, base_dir: &Path) -> KuResult<Value> {
    expect_arg_count("fs.read_dir", args.len(), 1, span)?;
    let Value::String(path) = &args[0] else {
        return Err(expected_type("str", &args[0], span));
    };
    let resolved = resolve_path(base_dir, path);
    let display_path = resolved.display().to_string();
    if path.len() > MAX_PATH_BYTES || display_path.len() > MAX_PATH_BYTES {
        return Ok(read_dir_error(&display_path, "directory path is too long"));
    }
    let entries = match fs::read_dir(&resolved) {
        Ok(entries) => entries,
        Err(err) => return Ok(read_dir_error(&display_path, err)),
    };
    let mut paths = Vec::new();
    let mut output_bytes = 0_usize;
    for entry in entries {
        if paths.len() >= MAX_DIRECTORY_ENTRIES {
            return Ok(read_dir_error(
                &display_path,
                "directory has too many entries",
            ));
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => return Ok(read_dir_error(&display_path, err)),
        };
        let entry_display = match entry.path().into_os_string().into_string() {
            Ok(path) => path,
            Err(_) => {
                return Ok(read_dir_error(
                    &display_path,
                    "directory entry path is not valid UTF-8",
                ));
            }
        };
        if entry_display.len() > MAX_PATH_BYTES {
            return Ok(read_dir_error(
                &display_path,
                "directory entry path is too long",
            ));
        }
        output_bytes = match output_bytes.checked_add(entry_display.len()) {
            Some(bytes) if bytes <= MAX_DIRECTORY_OUTPUT_BYTES => bytes,
            _ => {
                return Ok(read_dir_error(
                    &display_path,
                    "directory listing is too large",
                ));
            }
        };
        paths.push(entry_display);
    }
    paths.sort();
    Ok(errors::ok(Value::Array(
        paths.into_iter().map(Value::String).collect(),
    )))
}

fn read_dir_error(path: &str, cause: impl std::fmt::Display) -> Value {
    errors::err(
        "fs",
        "read_dir_failed",
        format!("failed to read directory '{path}': {cause}"),
    )
}

fn read(args: &[Value], span: Span, base_dir: &Path) -> KuResult<Value> {
    read_result("fs.read", args, span, base_dir)
}

fn try_read(args: &[Value], span: Span, base_dir: &Path) -> KuResult<Value> {
    read_result("fs.try_read", args, span, base_dir)
}

fn read_result(operation: &str, args: &[Value], span: Span, base_dir: &Path) -> KuResult<Value> {
    expect_arg_count(operation, args.len(), 1, span)?;
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
    write_result("fs.write", args, span, base_dir)
}

fn try_write(args: &[Value], span: Span, base_dir: &Path) -> KuResult<Value> {
    write_result("fs.try_write", args, span, base_dir)
}

fn write_result(operation: &str, args: &[Value], span: Span, base_dir: &Path) -> KuResult<Value> {
    expect_arg_count(operation, args.len(), 2, span)?;
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

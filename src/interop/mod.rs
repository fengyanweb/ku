//! Shared typed boundary for bridges generated from installed package sources.

use crate::{
    ast::{Expr, ExprKind, Literal},
    error::{KuError, KuResult},
    span::Span,
};

pub(crate) mod c_ffi;
pub(crate) mod python;
mod runtime;

pub(crate) use runtime::invoke;

pub(crate) const BOUND_PREFIX: &str = "__ku_interop_bound_";

pub(crate) const MAX_ARGUMENTS: usize = 32;
pub(crate) const MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const RELATIVE_RUNTIME_ROOT: &str = "ku-runtime.d/interop";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReturnKind {
    Null,
    Int,
    Str,
    Bool,
}

impl ReturnKind {
    pub(crate) fn for_builtin(name: &str) -> Option<Self> {
        Some(match name.strip_prefix("__ku_interop_")? {
            "null" | "bound_null" => Self::Null,
            "int" | "bound_int" => Self::Int,
            "str" | "bound_str" => Self::Str,
            "bool" | "bound_bool" => Self::Bool,
            _ => return None,
        })
    }

    pub(crate) fn for_bound(name: &str) -> Option<Self> {
        if name.starts_with(BOUND_PREFIX) {
            Self::for_builtin(name)
        } else {
            None
        }
    }

    pub(crate) fn intrinsic(self) -> &'static str {
        match self {
            Self::Null => "__ku_interop_null",
            Self::Int => "__ku_interop_int",
            Self::Str => "__ku_interop_str",
            Self::Bool => "__ku_interop_bool",
        }
    }

    pub(crate) fn tag(self) -> u32 {
        match self {
            Self::Null => 0,
            Self::Int => 1,
            Self::Str => 2,
            Self::Bool => 3,
        }
    }
}

pub(crate) fn binding_arguments(args: &[Expr], span: Span) -> KuResult<(&str, &str)> {
    let [path, operation, arguments @ ..] = args else {
        return Err(KuError::runtime(
            "interop requires a library and an operation",
            span,
        ));
    };
    let (ExprKind::Literal(Literal::String(path)), ExprKind::Literal(Literal::String(operation))) =
        (&path.kind, &operation.kind)
    else {
        return Err(KuError::runtime(
            "interop binding and operation must be static strings",
            span,
        ));
    };
    if path.is_empty() || path.contains('\0') {
        return Err(KuError::runtime("invalid interop library binding", span));
    }
    if operation.is_empty()
        || operation.len() > 200
        || !operation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
    {
        return Err(KuError::runtime("invalid interop operation name", span));
    }
    if arguments.len() > MAX_ARGUMENTS {
        return Err(KuError::runtime("interop call exceeds 32 arguments", span));
    }
    Ok((path, operation))
}

pub(crate) fn prepare_install(
    project: &std::path::Path,
    source: Option<&str>,
    python: Option<&std::path::Path>,
    module: Option<&str>,
) -> KuResult<Option<String>> {
    use std::path::Path;
    let Some(source) = source else {
        return Ok(None);
    };
    let path = Path::new(source);
    let is_python = source.starts_with("pypi:")
        || path.extension().is_some_and(|extension| extension == "py") && path.is_file();
    if !is_python && (python.is_some() || module.is_some()) {
        return Err(KuError::message(
            "--python and --module require a Python source",
        ));
    }
    let generated = if is_python {
        self::python::prepare_install(project, source, python, module)?
    } else if path.is_dir() && !path.join("ku.mod").is_file() {
        self::c_ffi::prepare_install(project, path)?
    } else {
        if path.is_relative() && (source.starts_with("./") || source.starts_with("../")) {
            let absolute = std::env::current_dir()
                .map_err(|error| {
                    KuError::message(format!("cannot read current directory: {error}"))
                })?
                .join(path);
            let text = absolute
                .to_str()
                .ok_or_else(|| KuError::message("package source path must be UTF-8"))?;
            return Ok(Some(text.to_owned()));
        }
        return Ok(Some(source.to_owned()));
    };
    let relative = generated.strip_prefix(project).map_err(|_| {
        KuError::message("generated interop package must be inside the consuming project")
    })?;
    let relative = relative
        .to_str()
        .ok_or_else(|| KuError::message("generated interop package path must be UTF-8"))?;
    Ok(Some(format!("./{}", relative.replace('\\', "/"))))
}

pub(crate) fn implementation_digest() -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for unit in [
        include_bytes!("abi.h").as_slice(),
        include_bytes!("c_ffi.rs").as_slice(),
        include_bytes!("native/signatures.rs").as_slice(),
        include_bytes!("native/codegen.rs").as_slice(),
        include_bytes!("python.rs").as_slice(),
        include_bytes!("python_portable.rs").as_slice(),
        include_bytes!("python_prepare.py").as_slice(),
        include_bytes!("python_bridge.c").as_slice(),
    ] {
        hash.update((unit.len() as u64).to_le_bytes());
        hash.update(unit);
    }
    format!("{:x}", hash.finalize())
}

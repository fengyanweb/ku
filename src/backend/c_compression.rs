//! Optional, bounded native compression. No runtime is emitted or linked unless
//! checked IR actually calls one of these modules. Public byte inputs are views;
//! the returned byte allocation belongs to the caller's ordinary owned value.
use super::{c_expr, output::COutput, unsupported, IrExpr, KuResult};

pub(super) fn emit(out: &mut COutput, zstd: bool, zlib: bool) {
    if out.failed() || (!zstd && !zlib) {
        return;
    }
    if zstd {
        out.push_str("#define KU_FEATURE_ZSTD 1\n");
    }
    if zlib {
        out.push_str("#define KU_FEATURE_ZLIB 1\n");
    }
    out.push_str(include_str!("c_compression_runtime.c"));
}

pub(super) fn intrinsic_expr(module: &str, method: &str, args: &[IrExpr]) -> KuResult<String> {
    if !matches!(module, "zstd" | "gzip" | "deflate")
        || !matches!(method, "compress" | "decompress")
    {
        return Err(unsupported(format!(
            "native C {module}.{method}() is not implemented"
        )));
    }
    if args.len() != 2 {
        return Err(unsupported(format!(
            "{module}.{method} expects bytes and an explicit max_output_bytes"
        )));
    }
    Ok(format!(
        "ku_{module}_{method}({}, {})",
        c_expr(&args[0])?,
        c_expr(&args[1])?
    ))
}

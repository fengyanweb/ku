//! C emission for the verified interop companion ABI.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use super::{
    c_expr, c_result_type, expr_children, output::COutput, unsupported, walk_inst_exprs,
    walk_terminator_exprs,
};
use crate::{
    error::KuResult,
    ir::{IrCallKind, IrExpr, IrExprKind, IrProgram, IrType},
};

const PREFIX: &str = "__ku_interop_";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WireType {
    Null,
    Int,
    Str,
    Bool,
}

impl WireType {
    fn from_name(suffix: &str) -> Option<Self> {
        match suffix {
            "null" => Some(Self::Null),
            "int" => Some(Self::Int),
            "str" => Some(Self::Str),
            "bool" => Some(Self::Bool),
            _ => None,
        }
    }

    fn from_ir(ty: &IrType) -> Option<Self> {
        match ty {
            IrType::Null => Some(Self::Null),
            IrType::Int => Some(Self::Int),
            IrType::Str => Some(Self::Str),
            IrType::Bool => Some(Self::Bool),
            _ => None,
        }
    }

    fn names(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Null => ("null", "KU_INTEROP_NULL", "0"),
            Self::Int => ("int", "KU_INTEROP_INT", "outcome.integer"),
            Self::Str => ("str", "KU_INTEROP_STR", "outcome.text"),
            Self::Bool => ("bool", "KU_INTEROP_BOOL", "outcome.integer != 0"),
        }
    }

    fn ir(self) -> IrType {
        match self {
            Self::Null => IrType::Null,
            Self::Int => IrType::Int,
            Self::Str => IrType::Str,
            Self::Bool => IrType::Bool,
        }
    }
}

fn wire_intrinsic(name: &str) -> Option<WireType> {
    WireType::from_name(name.strip_prefix(PREFIX)?)
}

fn locator_literal(expr: &IrExpr) -> Option<&str> {
    if expr.ty != IrType::Str {
        return None;
    }
    match &expr.kind {
        IrExprKind::Literal(literal) => Some(literal),
        IrExprKind::Borrow(inner) => locator_literal(inner),
        _ => None,
    }
}

fn plugin_symbol(literal: &str) -> String {
    format!(
        "__ku_interop_host_plugin_{:x}",
        Sha256::digest(literal.as_bytes())
    )
}

fn collect(expr: &IrExpr, signatures: &mut BTreeSet<WireType>, plugins: &mut BTreeSet<String>) {
    if let IrExprKind::Call {
        args,
        kind: IrCallKind::Intrinsic(name),
        ..
    } = &expr.kind
    {
        if let Some(ty) = wire_intrinsic(name) {
            signatures.insert(ty);
            if let Some(path) = args.first().and_then(locator_literal) {
                plugins.insert(plugin_symbol(path));
            }
        }
    }
    for child in expr_children(expr) {
        collect(child, signatures, plugins);
    }
}

pub(super) fn emit(out: &mut COutput, program: &IrProgram) -> KuResult<()> {
    let mut signatures = BTreeSet::new();
    let mut plugins = BTreeSet::new();
    for function in &program.functions {
        for block in &function.blocks {
            let mut visit = |expr: &IrExpr| collect(expr, &mut signatures, &mut plugins);
            for inst in &block.instructions {
                walk_inst_exprs(inst, &mut visit);
            }
            walk_terminator_exprs(&block.terminator, &mut visit);
        }
    }
    if signatures.is_empty() {
        return Ok(());
    }

    out.push_str(concat!(
        "\n#define KU_INTEROP_HOST 1\n",
        include_str!("../interop/abi.h"),
        "\n#undef KU_INTEROP_HOST\n",
        include_str!("../interop/native_host.c"),
    ));
    for plugin in plugins {
        out.push_str(&format!("static __KuInteropPlugin {plugin};\n"));
    }
    for ty in signatures {
        let (suffix, tag, extract) = ty.names();
        let result = c_result_type(&ty.ir())?;
        out.push_str(&format!(
            "static {result} __ku_interop_host_{suffix}(\n\
             \x20   __KuInteropPlugin* owner, KuString path, KuString operation,\n\
             \x20   const KuInteropValue* args, size_t nargs) {{\n\
             \x20 __KuInteropHostResult outcome = __ku_interop_perform(owner, path, operation, args, nargs, {tag});\n\
             \x20 return ({result}){{ outcome.ok, {extract}, outcome.error }};\n}}\n"
        ));
    }
    out.check()
}

pub(super) fn expression(name: &str, args: &[IrExpr], ty: &IrType) -> KuResult<Option<String>> {
    let Some(return_type) = wire_intrinsic(name) else {
        return Ok(None);
    };
    let [path, op, operands @ ..] = args else {
        return Err(unsupported(
            "interop requires a companion path and operation",
        ));
    };
    if ty != &IrType::Result(Box::new(return_type.ir())) || operands.len() > 32 {
        return Err(unsupported("invalid typed interop call"));
    }
    let (Some(locator), Some(_)) = (locator_literal(path), locator_literal(op)) else {
        return Err(unsupported(
            "interop companion path and operation must be literals",
        ));
    };
    let arguments = if operands.is_empty() {
        "NULL".to_owned()
    } else {
        let values = operands
            .iter()
            .map(|expr| {
                let (kind, _, _) = WireType::from_ir(&expr.ty)
                    .ok_or_else(|| unsupported("invalid interop argument type"))?
                    .names();
                if kind == "null" {
                    return Ok("__ku_interop_arg_null()".to_string());
                }
                Ok(format!("__ku_interop_arg_{kind}({})", c_expr(expr)?))
            })
            .collect::<KuResult<Vec<_>>>()?;
        format!("(const KuInteropValue[]){{ {} }}", values.join(", "))
    };
    let (suffix, _, _) = return_type.names();
    Ok(Some(format!(
        "__ku_interop_host_{suffix}(&{}, {}, {}, {arguments}, {})",
        plugin_symbol(locator),
        c_expr(path)?,
        c_expr(op)?,
        operands.len()
    )))
}

//! Typed, independently owned array results and the flat `T | null` ABI.
//!
//! Fallible clones are private to these operations: a partially built result is
//! destroyed before publishing an allocation failure to the existing sync guard.
//! Source arrays are views and are never moved or modified here.

use super::{
    c_array_type, c_clone_value, c_drop_value, c_expr, c_ident, c_object_type_name,
    c_time_scalar_name, c_type, c_type_suffix, c_zero_value, enum_type_name, expr_children,
    output::COutput, unsupported, walk_inst_types, walk_terminator_exprs,
};
use crate::{
    ast::BinaryOp,
    error::KuResult,
    ir::{IrCallKind, IrExpr, IrExprKind, IrProgram, IrType},
};

fn equality_payload(inner: &IrType) -> KuResult<String> {
    Ok(match inner {
        IrType::Int | IrType::Float | IrType::Bool => "left.value == right.value".into(),
        IrType::Null => "true".into(),
        IrType::Str => "ku_string_equal(left.value, right.value)".into(),
        IrType::Named(name) if c_time_scalar_name(name) => format!(
            "{}_equal(left.value, right.value)",
            super::c_time_prefix(name)
        ),
        IrType::Named(name) if c_object_type_name(name) => {
            "ku_value_equal(ku_v_object(left.value), ku_v_object(right.value))".into()
        }
        IrType::Named(name) if name == "__ku_value" => {
            "ku_value_equal(left.value, right.value)".into()
        }
        _ => {
            return Err(unsupported(format!(
                "native C equality does not support nullable {inner}"
            )))
        }
    })
}

fn equality_types(expr: &IrExpr, types: &mut Vec<IrType>) {
    if let IrExprKind::Binary {
        left,
        op: BinaryOp::Equal | BinaryOp::NotEqual,
        right,
    } = &expr.kind
    {
        if left.ty != IrType::Null && right.ty != IrType::Null {
            if let IrType::Nullable(inner) = &left.ty {
                if !types.contains(inner) {
                    types.push((**inner).clone());
                }
            } else if let IrType::Nullable(inner) = &right.ty {
                if !types.contains(inner) {
                    types.push((**inner).clone());
                }
            }
        }
    }
    for child in expr_children(expr) {
        equality_types(child, types);
    }
}

pub(super) fn equality_expr(
    left: &IrExpr,
    op: BinaryOp,
    right: &IrExpr,
) -> KuResult<Option<String>> {
    if !matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) {
        return Ok(None);
    }
    let inner = match (&left.ty, &right.ty) {
        (IrType::Nullable(inner), _) | (_, IrType::Nullable(inner)) => inner,
        _ => return Ok(None),
    };
    let suffix = c_type_suffix(inner)?;
    let nullable = nullable_type(inner)?;
    let expression = if right.ty == IrType::Null {
        format!("__ku_nullable_is_null_{suffix}({})", c_expr(left)?)
    } else if left.ty == IrType::Null {
        format!("__ku_nullable_is_null_{suffix}({})", c_expr(right)?)
    } else {
        let argument = |value: &IrExpr| -> KuResult<String> {
            Ok(if matches!(value.ty, IrType::Nullable(_)) {
                c_expr(value)?
            } else {
                format!("({nullable}){{ false, {} }}", c_expr(value)?)
            })
        };
        format!(
            "__ku_nullable_equal_{suffix}({}, {})",
            argument(left)?,
            argument(right)?
        )
    };
    Ok(Some(if op == BinaryOp::NotEqual {
        format!("(!({expression}))")
    } else {
        expression
    }))
}

pub(super) fn nullable_type(inner: &IrType) -> KuResult<String> {
    Ok(format!("__ku_nullable_type_{}", c_type_suffix(inner)?))
}

pub(super) fn emit_typedef(out: &mut COutput, inner: &IrType) -> KuResult<()> {
    out.check()?;
    out.push_str(&format!(
        "struct {} {{ bool is_null; {} value; }};\n",
        nullable_type(inner)?,
        c_type(inner)?
    ));
    Ok(())
}

pub(super) fn emit_prototypes(out: &mut COutput, inner: &IrType) -> KuResult<()> {
    let suffix = c_type_suffix(inner)?;
    let nullable = nullable_type(inner)?;
    out.push_str(&format!(
        "static {nullable} __ku_nullable_clone_{suffix}({nullable} value);\n\
         static {nullable} __ku_nullable_move_{suffix}({nullable}* value);\n\
         static void __ku_nullable_drop_{suffix}({nullable}* value);\n\
         static {ty} __ku_nullable_take_{suffix}({nullable}* value);\n\
         static bool __ku_nullable_is_null_{suffix}({nullable} value);\n",
        ty = c_type(inner)?
    ));
    if dynamic_value(inner) {
        out.push_str(&format!(
            "static {nullable} __ku_nullable_some_{suffix}(KuValue value);\n"
        ));
    }
    out.check()
}

fn dynamic_value(ty: &IrType) -> bool {
    matches!(ty, IrType::Named(name) if name == "__ku_value")
}

fn null_condition(inner: &IrType, place: &str, pointer: bool) -> String {
    let access = if pointer { "->" } else { "." };
    if dynamic_value(inner) {
        format!("({place}{access}is_null || {place}{access}value.tag == KU_NULL)")
    } else {
        format!("{place}{access}is_null")
    }
}

fn append_type(ty: &IrType, types: &mut Vec<IrType>) {
    if !types.contains(ty) {
        types.push(ty.clone());
    }
    match ty {
        IrType::Array(inner) | IrType::Result(inner) | IrType::Nullable(inner) => {
            append_type(inner, types)
        }
        IrType::Closure { params, ret, .. } => {
            for param in params {
                append_type(param, types);
            }
            append_type(ret, types);
        }
        _ => {}
    }
}

fn collect_ops(expr: &IrExpr, ops: &mut Vec<(String, IrType)>) {
    if let IrExprKind::Call {
        kind: IrCallKind::Intrinsic(name),
        args,
        ..
    } = &expr.kind
    {
        if matches!(name.as_str(), "array.concat" | "array.first" | "array.last") {
            if let Some(IrExpr {
                ty: IrType::Array(element),
                ..
            }) = args.first()
            {
                let op = (name.clone(), (**element).clone());
                if !ops.contains(&op) {
                    ops.push(op);
                }
            }
        }
    }
    for child in expr_children(expr) {
        collect_ops(child, ops);
    }
}

fn clone_types(ty: &IrType, program: &IrProgram, types: &mut Vec<IrType>) {
    if types.iter().any(|existing| existing == ty || matches!((c_type_suffix(existing), c_type_suffix(ty)), (Ok(left), Ok(right)) if left == right)) {
        return;
    }
    types.push(ty.clone());
    match ty {
        IrType::Array(inner) | IrType::Result(inner) | IrType::Nullable(inner) => {
            clone_types(inner, program, types);
        }
        IrType::Named(name) => {
            if let Some(name) = enum_type_name(name) {
                if let Some(layout) = program.layouts.enums.iter().find(|item| item.name == name) {
                    for variant in &layout.variants {
                        for field in &variant.fields {
                            clone_types(&field.ty, program, types);
                        }
                    }
                }
            } else if let Some(layout) = program
                .layouts
                .structs
                .iter()
                .find(|item| item.name == *name)
            {
                for field in &layout.fields {
                    clone_types(&field.ty, program, types);
                }
            }
        }
        _ => {}
    }
}

fn try_clone(ty: &IrType, value: &str, output: &str) -> KuResult<String> {
    Ok(format!(
        "__ku_array_ops_try_clone_{}({value}, {output}, failure)",
        c_type_suffix(ty)?
    ))
}

fn fail_partial(ty: &IrType) -> KuResult<String> {
    Ok(format!("{} return false;", c_drop_value(ty, "result")?))
}

fn emit_try_clone(out: &mut COutput, ty: &IrType, program: &IrProgram) -> KuResult<()> {
    let suffix = c_type_suffix(ty)?;
    let cty = c_type(ty)?;
    let zero = c_zero_value(ty)?;
    out.push_str(&format!(
        "static bool __ku_array_ops_try_clone_{suffix}({cty} value, {cty}* out, int* failure) {{\n  *out = {zero};\n"
    ));
    match ty {
        IrType::Int | IrType::Float | IrType::Bool | IrType::Null => {
            out.push_str("  (void)failure; *out = value; return true;\n");
        }
        IrType::Str => out.push_str(
            "  if ((value.len && !value.ptr) || value.storage > KU_STRING_OWNED\n\
             \x20     || (value.storage == KU_STRING_OWNED && value.len > value.capacity)\n\
             \x20     || (value.storage == KU_STRING_STATIC && value.capacity)) { *failure = 1; return false; }\n\
             \x20 if (value.storage == KU_STRING_STATIC) { *out = value; return true; }\n\
             \x20 uint8_t* data = (uint8_t*)malloc(value.len ? value.len : 1);\n\
             \x20 if (!data) { *failure = 1; return false; }\n\
             \x20 if (value.len) memcpy(data, value.ptr, value.len);\n\
             \x20 *out = (KuString){ data, value.len, value.len ? value.len : 1, KU_STRING_OWNED };\n\
             \x20 return true;\n",
        ),
        IrType::Array(inner) => {
            let inner_type = c_type(inner)?;
            out.push_str(&format!(
                "  if (value.len > SIZE_MAX / sizeof({inner_type}) || (value.len && !value.data) || (value.capacity && value.len > value.capacity)) {{ *failure = 1; return false; }}\n\
                 \x20 if (!value.len) return true;\n\
                 \x20 {cty} result = {{0}};\n\
                 \x20 result.data = ({inner_type}*)malloc(value.len * sizeof({inner_type}));\n\
                 \x20 if (!result.data) {{ *failure = 1; return false; }}\n\
                 \x20 result.capacity = value.len;\n\
                 \x20 for (size_t i = 0; i < value.len; i++) {{\n\
                 \x20   if ((i & 127u) == 0u && __ku_handler_timeout_poll()) {{ *failure = 2; {cleanup} }}\n\
                 \x20   if (!{clone}) {{ {cleanup} }}\n\
                 \x20   result.len++;\n\
                 \x20 }}\n\
                 \x20 *out = result; return true;\n",
                cleanup = fail_partial(ty)?,
                clone = try_clone(inner, "value.data[i]", "&result.data[i]")?
            ));
        }
        IrType::Nullable(inner) => {
            out.push_str(&format!(
                "  if ({null}) return true;\n\
                 \x20 {cty} result = {zero};\n\
                 \x20 if (!{clone}) return false;\n\
                 \x20 result.is_null = false; *out = result; return true;\n",
                clone = try_clone(inner, "value.value", "&result.value")?,
                null = null_condition(inner,"value",false)
            ));
        }
        IrType::Result(inner) => {
            out.push_str(&format!(
                "  {cty} result = {zero};\n\
                 \x20 if (value.ok) {{\n\
                 \x20   if (!{clone}) return false;\n\
                 \x20   result.ok = true;\n\
                 \x20 }} else if (!__ku_array_ops_try_clone_error(value.error, &result.error, failure)) return false;\n\
                 \x20 *out = result; return true;\n",
                clone = try_clone(inner, "value.value", "&result.value")?
            ));
        }
        IrType::Named(name) if name == "__ku_error_type" => {
            out.push_str("  return __ku_array_ops_try_clone_error(value, out, failure);\n");
        }
        IrType::Named(name) if c_object_type_name(name) => {
            out.push_str("  if (!ku_object_try_clone(value, out)) { *failure = 1; return false; } return true;\n");
        }
        IrType::Named(name) if name == "__ku_value" => {
            out.push_str("  if (!ku_value_try_clone(value, out)) { *failure = 1; return false; } return true;\n");
        }
        IrType::Named(name) if name == "__ku_bytes" => out.push_str(
            "  if (value.len && !value.ptr) { *failure = 1; return false; }\n\
             \x20 if (value.storage != KU_BYTES_OWNED) { *out = value; return true; }\n\
             \x20 if (ku_bytes_try_copy(value.ptr, value.len, out) != 1) { *failure = 1; return false; } return true;\n",
        ),
        IrType::Named(name) if c_time_scalar_name(name) || name == "__ku_http_server" => {
            out.push_str("  (void)failure; *out = value; return true;\n");
        }
        IrType::Closure { .. } => {
            out.push_str(&format!(
                "  (void)failure; *out = {}; return true;\n",
                c_clone_value(ty, "value")?
            ));
        }
        IrType::Named(name) => {
            if let Some(enum_name) = enum_type_name(name) {
                let layout = program
                    .layouts
                    .enums
                    .iter()
                    .find(|layout| layout.name == enum_name)
                    .ok_or_else(|| unsupported(format!("unknown array enum {enum_name}")))?;
                out.push_str(&format!("  {cty} result = {zero}; result.tag = value.tag;\n  switch (value.tag) {{\n"));
                for variant in &layout.variants {
                    out.push_str(&format!("    case {}:\n", variant.tag));
                    for field in &variant.fields {
                        let place = format!("payload.{}.{}", c_ident(&variant.name), c_ident(&field.name));
                        out.push_str(&format!("      result.{place} = {};\n", c_zero_value(&field.ty)?));
                    }
                    for field in &variant.fields {
                        let place = format!("payload.{}.{}", c_ident(&variant.name), c_ident(&field.name));
                        out.push_str(&format!(
                            "      if (!{}) {{ {} }}\n",
                            try_clone(&field.ty, &format!("value.{place}"), &format!("&result.{place}"))?,
                            fail_partial(ty)?
                        ));
                    }
                    out.push_str("      break;\n");
                }
                out.push_str("    default: *failure = 1; return false;\n  }\n  *out = result; return true;\n");
            } else {
                let layout = program
                    .layouts
                    .structs
                    .iter()
                    .find(|layout| layout.name == *name)
                    .ok_or_else(|| unsupported(format!("native array cannot clone opaque type {name}")))?;
                out.push_str(&format!("  {cty} result = {zero};\n"));
                for field in &layout.fields {
                    out.push_str(&format!(
                        "  result.{} = {};\n",
                        c_ident(&field.name),
                        c_zero_value(&field.ty)?
                    ));
                }
                for field in &layout.fields {
                    let field_name = c_ident(&field.name);
                    out.push_str(&format!(
                        "  if (!{}) {{ {} }}\n",
                        try_clone(&field.ty, &format!("value.{field_name}"), &format!("&result.{field_name}"))?,
                        fail_partial(ty)?
                    ));
                }
                out.push_str("  *out = result; return true;\n");
            }
        }
        _ => return Err(unsupported(format!("native array cannot clone {ty}"))),
    }
    out.push_str("}\n");
    out.check()
}

fn emit_nullable_helpers(
    out: &mut COutput,
    inner: &IrType,
    synchronous_clone: bool,
) -> KuResult<()> {
    let suffix = c_type_suffix(inner)?;
    let nullable = nullable_type(inner)?;
    let wrapped = IrType::Nullable(Box::new(inner.clone()));
    let zero = c_zero_value(&wrapped)?;
    if dynamic_value(inner) {
        out.push_str(&format!("static {nullable} __ku_nullable_some_{suffix}(KuValue value) {{\n  if (value.tag == KU_NULL) return {zero};\n  return ({nullable}){{ false, value }};\n}}\n"));
    }
    if synchronous_clone {
        out.push_str(&format!(
        "static {nullable} __ku_nullable_clone_{suffix}({nullable} value) {{\n\
         \x20 {nullable} result = {zero}; int failure = 0;\n\
         \x20 if (!__ku_array_ops_try_clone_nullable_{suffix}(value, &result, &failure)) __ku_array_ops_fail(failure);\n\
         \x20 return result;\n\
         }}\n"
    ));
    }
    out.push_str(&format!(
        "static {nullable} __ku_nullable_move_{suffix}({nullable}* value) {{\n\
         \x20 if (!value) return {zero};\n\
         \x20 {nullable} result = *value; *value = {zero};\n\
         \x20 if ({result_null}) return {zero}; return result;\n\
         }}\n\
         static void __ku_nullable_drop_{suffix}({nullable}* value) {{\n\
         \x20 if (!value) return;\n\
         \x20 if (!value->is_null) {{ {drop} }}\n\
         \x20 *value = {zero};\n\
         }}\n\
         static {cty} __ku_nullable_take_{suffix}({nullable}* value) {{\n\
         \x20 if (!value) return {inner_zero};\n\
         \x20 if ({pointer_null}) {{ *value = {zero}; return {inner_zero}; }}\n\
         \x20 {cty} result = value->value; *value = {zero}; return result;\n\
         }}\n\
         static bool __ku_nullable_is_null_{suffix}({nullable} value) {{ return {value_null}; }}\n",
        cty = c_type(inner)?,
        inner_zero = c_zero_value(inner)?,
        drop = c_drop_value(inner, "value->value")?,
        value_null = null_condition(inner, "value", false),
        result_null = null_condition(inner, "result", false),
        pointer_null = null_condition(inner, "value", true)
    ));
    out.check()
}

fn emit_array_op(out: &mut COutput, name: &str, inner: &IrType) -> KuResult<()> {
    let suffix = c_type_suffix(inner)?;
    let array = c_array_type(inner)?;
    let element = c_type(inner)?;
    let method = name.trim_start_matches("array.");
    if method == "concat" {
        out.push_str(&format!(
            "static {array} __ku_array_concat_{suffix}({array} left, {array} right) {{\n\
             \x20 {array} result = {{0}}; int failure = 0;\n\
             \x20 if (left.len > SIZE_MAX - right.len || left.len + right.len > SIZE_MAX / sizeof({element})\n\
             \x20     || (left.len && !left.data) || (right.len && !right.data)) {{ __ku_array_ops_fail(1); return result; }}\n\
             \x20 size_t count = left.len + right.len;\n\
             \x20 if (!count) return result;\n\
             \x20 result.data = ({element}*)malloc(count * sizeof({element}));\n\
             \x20 if (!result.data) {{ __ku_array_ops_fail(1); return result; }}\n\
             \x20 result.capacity = count;\n\
             \x20 for (size_t i = 0; i < count; i++) {{\n\
             \x20   if ((i & 127u) == 0u && __ku_handler_timeout_poll()) {{ failure = 2; break; }}\n\
             \x20   {element} item = i < left.len ? left.data[i] : right.data[i - left.len];\n\
             \x20   if (!__ku_array_ops_try_clone_{suffix}(item, &result.data[i], &failure)) break;\n\
             \x20   result.len++;\n\
             \x20 }}\n\
             \x20 if (failure) {{ {drop} __ku_array_ops_fail(failure); return ({array}){{0}}; }}\n\
             \x20 return result;\n\
             }}\n",
            drop = c_drop_value(&IrType::Array(Box::new(inner.clone())), "result")?
        ));
    } else if *inner == IrType::Null {
        out.push_str(&format!(
            "static int64_t __ku_array_{method}_{suffix}({array} value) {{ (void)value; return 0; }}\n"
        ));
    } else {
        let result_ty = if matches!(inner, IrType::Nullable(_)) {
            inner.clone()
        } else {
            IrType::Nullable(Box::new(inner.clone()))
        };
        let result_c = c_type(&result_ty)?;
        let zero = c_zero_value(&result_ty)?;
        let index = if method == "first" {
            "0"
        } else {
            "value.len - 1"
        };
        let destination = if matches!(inner, IrType::Nullable(_)) {
            "&result"
        } else {
            "&result.value"
        };
        out.push_str(&format!(
            "static {result_c} __ku_array_{method}_{suffix}({array} value) {{\n\
             \x20 {result_c} result = {zero}; int failure = 0;\n\
             \x20 if (!value.len) return result;\n\
             \x20 if (!value.data || value.len > SIZE_MAX / sizeof({element})) {{ __ku_array_ops_fail(1); return result; }}\n\
             \x20 if (!__ku_array_ops_try_clone_{suffix}(value.data[{index}], {destination}, &failure)) {{ __ku_array_ops_fail(failure); return result; }}\n"
        ));
        if dynamic_value(inner) {
            out.push_str(&format!(
                "  return __ku_nullable_some_{suffix}(result.value);\n}}\n"
            ));
            return out.check();
        } else if !matches!(inner, IrType::Nullable(_)) {
            out.push_str("  result.is_null = false;\n");
        }
        out.push_str("  return result;\n}\n");
    }
    out.check()
}

pub(super) fn nullable_elements(program: &IrProgram) -> Vec<IrType> {
    let mut all_types = Vec::new();
    for function in &program.functions {
        append_type(&function.return_type, &mut all_types);
        for param in &function.params {
            append_type(&param.ty, &mut all_types);
        }
        for block in &function.blocks {
            for inst in &block.instructions {
                walk_inst_types(inst, &mut |ty| append_type(ty, &mut all_types));
            }
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                append_type(&expr.ty, &mut all_types);
                super::walk_expr_types(expr, &mut |ty| append_type(ty, &mut all_types));
            });
        }
    }
    for layout in &program.layouts.structs {
        for field in &layout.fields {
            append_type(&field.ty, &mut all_types);
        }
    }
    for layout in &program.layouts.enums {
        for variant in &layout.variants {
            for field in &variant.fields {
                append_type(&field.ty, &mut all_types);
            }
        }
    }
    let mut nullables = all_types
        .iter()
        .filter_map(|ty| match ty {
            IrType::Nullable(inner) => Some((**inner).clone()),
            _ => None,
        })
        .collect();
    super::dedup_c_types(&mut nullables);
    nullables
}

pub(super) fn emit_helpers(
    out: &mut COutput,
    program: &IrProgram,
    synchronous_calls: bool,
) -> KuResult<()> {
    let nullables = nullable_elements(program);
    let mut ops = Vec::new();
    let mut equalities = Vec::new();
    for function in &program.functions {
        for block in &function.blocks {
            for inst in &block.instructions {
                super::walk_inst_exprs(inst, &mut |expr| collect_ops(expr, &mut ops));
                super::walk_inst_exprs(inst, &mut |expr| equality_types(expr, &mut equalities));
            }
            walk_terminator_exprs(&block.terminator, &mut |expr| collect_ops(expr, &mut ops));
            walk_terminator_exprs(&block.terminator, &mut |expr| {
                equality_types(expr, &mut equalities)
            });
        }
    }
    if ops.is_empty() && nullables.is_empty() {
        return Ok(());
    }
    if !synchronous_calls {
        // Task collection operations use their allocation host, error result
        // and frame cleanup. Only ownership/ABI leaves are shared with sync;
        // do not emit an unused sync fatal transport into a Task-only unit.
        for inner in &nullables {
            emit_nullable_helpers(out, inner, false)?;
        }
        return out.check();
    }
    let mut clones = Vec::new();
    let mut seen_ops = std::collections::HashSet::new();
    ops.retain(|(name, inner)| seen_ops.insert((name.clone(), c_type_suffix(inner).ok())));
    super::dedup_c_types(&mut equalities);
    for (_, inner) in &ops {
        clone_types(inner, program, &mut clones);
    }
    for inner in &nullables {
        clone_types(
            &IrType::Nullable(Box::new(inner.clone())),
            program,
            &mut clones,
        );
    }
    // Errors contain strings even when the successful Result payload does not.
    let needs_error_clone = clones.iter().any(|ty| {
        matches!(ty, IrType::Result(_))
            || matches!(ty, IrType::Named(name) if name == "__ku_error_type")
    });
    if needs_error_clone {
        clone_types(&IrType::Str, program, &mut clones);
    }
    out.push_str(
        "/* failure=2 is the existing deadline unwind; all other failures use the sync guard. */\n\
         static void __ku_array_ops_fail(int failure) {\n\
         \x20 if (failure != 2) {\n\
         \x20   static const uint8_t message[] = \"array result allocation failed or length overflowed\";\n\
         \x20   __ku_sync_raise_http_json(message, sizeof(message) - 1);\n\
         \x20 }\n\
         }\n",
    );
    for ty in &clones {
        out.push_str(&format!(
            "static bool __ku_array_ops_try_clone_{}({cty} value, {cty}* out, int* failure);\n",
            c_type_suffix(ty)?,
            cty = c_type(ty)?
        ));
    }
    if needs_error_clone {
        out.push_str(
            "static bool __ku_array_ops_try_clone_error(KuError value, KuError* out, int* failure) {\n\
             \x20 KuError result = {0}; *out = (KuError){0};\n\
             \x20 if (!__ku_array_ops_try_clone_str(value.domain, &result.domain, failure)\n\
             \x20     || !__ku_array_ops_try_clone_str(value.code, &result.code, failure)\n\
             \x20     || !__ku_array_ops_try_clone_str(value.message, &result.message, failure)) { ku_error_drop(&result); return false; }\n\
             \x20 *out = result; return true;\n\
             }\n",
        );
    }
    for ty in &clones {
        emit_try_clone(out, ty, program)?;
    }
    for inner in &nullables {
        emit_nullable_helpers(out, inner, true)?;
    }
    for inner in &equalities {
        let suffix = c_type_suffix(inner)?;
        let nullable = nullable_type(inner)?;
        out.push_str(&format!("static bool __ku_nullable_equal_{suffix}({nullable} left, {nullable} right) {{\n  bool left_null={},right_null={};\n  if (left_null || right_null) return left_null == right_null;\n  return {};\n}}\n", null_condition(inner,"left",false), null_condition(inner,"right",false), equality_payload(inner)?));
    }
    for (name, inner) in &ops {
        emit_array_op(out, name, inner)?;
    }
    out.check()
}

pub(super) fn intrinsic_expr(
    name: &str,
    args: &[IrExpr],
    _result_ty: &IrType,
) -> KuResult<Option<String>> {
    if !matches!(name, "array.concat" | "array.first" | "array.last") {
        return Ok(None);
    }
    let expected = if name == "array.concat" { 2 } else { 1 };
    if args.len() != expected {
        return Err(unsupported(format!("{name} requires {expected} arguments")));
    }
    let IrType::Array(inner) = &args[0].ty else {
        return Err(unsupported(format!("{name} requires a typed array")));
    };
    let args = args.iter().map(c_expr).collect::<KuResult<Vec<_>>>()?;
    Ok(Some(format!(
        "__ku_array_{}_{}({})",
        name.trim_start_matches("array."),
        c_type_suffix(inner)?,
        args.join(", ")
    )))
}

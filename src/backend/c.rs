use std::collections::{HashMap, VecDeque};

use crate::{
    ast::{BinaryOp, UnaryOp},
    error::{KuError, KuResult},
    ir::{
        IrBlock, IrCallKind, IrEnumLayout, IrExpr, IrExprKind, IrFunction, IrInst, IrLValue,
        IrProgram, IrStructLayout, IrTerminator, IrType,
    },
    span::Span,
};

struct OwnedLocal {
    source_name: String,
    name: String,
    ty: IrType,
    is_param: bool,
}

pub fn generate_c_source(program: &IrProgram) -> KuResult<String> {
    validate_layouts(program)?;
    let mut out = String::from(
        "#include <stdbool.h>\n#include <stdint.h>\n#include <stdio.h>\n#include <stdlib.h>\n#include <string.h>\n\n\
         typedef struct KuError {\n  const char* domain;\n  const char* code;\n  const char* message;\n} KuError;\n\
         static KuError ku_error_message(const char* message) {\n  return (KuError){ \"ku\", \"error\", message ? message : \"error\" };\n}\n\n",
    );
    emit_struct_layouts(&mut out, program)?;
    emit_enum_layouts(&mut out, program)?;
    emit_named_ownership_helpers(&mut out, program)?;
    emit_array_abi(&mut out, program)?;
    emit_result_abi(&mut out, program)?;
    for function in &program.functions {
        emit_function(&mut out, function)?;
        out.push('\n');
    }
    emit_main_wrapper(&mut out, program)?;
    Ok(out)
}

fn validate_layouts(program: &IrProgram) -> KuResult<()> {
    let indexes = program
        .layouts
        .structs
        .iter()
        .enumerate()
        .map(|(index, layout)| (layout.name.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut dependency_count = vec![0usize; program.layouts.structs.len()];
    let mut dependents = vec![Vec::new(); program.layouts.structs.len()];

    for (index, layout) in program.layouts.structs.iter().enumerate() {
        for field in &layout.fields {
            match &field.ty {
                IrType::Int | IrType::Bool | IrType::Str => {}
                IrType::Named(name) if enum_type_name(name).is_none() => {
                    let Some(&dependency) = indexes.get(name.as_str()) else {
                        return Err(unsupported(format!(
                            "native C struct '{}.{}' references unknown struct '{name}'",
                            layout.name, field.name
                        )));
                    };
                    dependency_count[index] += 1;
                    dependents[dependency].push(index);
                }
                IrType::Named(_) => {
                    return Err(unsupported(format!(
                        "native C struct '{}.{}' cannot contain an enum value before enum layouts are emitted",
                        layout.name, field.name
                    )));
                }
                other => {
                    return Err(unsupported(format!(
                        "native C struct '{}.{}' does not support field type {other}; supported field types are int, bool, str, and non-recursive named structs",
                        layout.name, field.name
                    )));
                }
            }
        }
    }

    let mut ready = dependency_count
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect::<VecDeque<_>>();
    let mut visited = 0usize;
    while let Some(index) = ready.pop_front() {
        visited += 1;
        for dependent in &dependents[index] {
            dependency_count[*dependent] -= 1;
            if dependency_count[*dependent] == 0 {
                ready.push_back(*dependent);
            }
        }
    }
    if visited != program.layouts.structs.len() {
        return Err(unsupported(
            "native C prototype does not support recursive value struct layouts",
        ));
    }

    for (index, layout) in program.layouts.structs.iter().enumerate() {
        for field in &layout.fields {
            if let IrType::Named(name) = &field.ty {
                let dependency = indexes[name.as_str()];
                if dependency >= index {
                    return Err(unsupported(format!(
                        "native C struct '{}.{}' must reference a struct declared earlier than '{}'; declaration-order value layouts cannot use a later struct",
                        layout.name, field.name, layout.name
                    )));
                }
            }
        }
    }

    let enum_indexes = program
        .layouts
        .enums
        .iter()
        .enumerate()
        .map(|(index, layout)| (layout.name.as_str(), index))
        .collect::<HashMap<_, _>>();
    for (index, layout) in program.layouts.enums.iter().enumerate() {
        for variant in &layout.variants {
            for field in &variant.fields {
                match &field.ty {
                    IrType::Int | IrType::Bool | IrType::Str => {}
                    IrType::Named(name) if enum_type_name(name).is_none() => {
                        if !indexes.contains_key(name.as_str()) {
                            return Err(unsupported(format!(
                                "native C enum '{}.{}.{}' references unknown struct '{name}'",
                                layout.name, variant.name, field.name
                            )));
                        }
                    }
                    IrType::Named(name) => {
                        let enum_name = enum_type_name(name).expect("checked enum marker");
                        let Some(&dependency) = enum_indexes.get(enum_name) else {
                            return Err(unsupported(format!(
                                "native C enum '{}.{}.{}' references unknown enum '{enum_name}'",
                                layout.name, variant.name, field.name
                            )));
                        };
                        if dependency >= index {
                            return Err(unsupported(format!(
                                "native C enum '{}.{}.{}' must reference an enum declared earlier; recursive enum value layouts are not supported",
                                layout.name, variant.name, field.name
                            )));
                        }
                    }
                    other => {
                        return Err(unsupported(format!(
                            "native C enum '{}.{}.{}' does not support payload type {other}; supported payloads are int, bool, str, structs, and earlier enums",
                            layout.name, variant.name, field.name
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

fn emit_struct_layouts(out: &mut String, program: &IrProgram) -> KuResult<()> {
    for layout in &program.layouts.structs {
        emit_struct_layout(out, layout)?;
    }
    if !program.layouts.structs.is_empty() {
        out.push('\n');
    }
    Ok(())
}

fn emit_struct_layout(out: &mut String, layout: &IrStructLayout) -> KuResult<()> {
    let name = c_struct_type(&layout.name);
    out.push_str(&format!("typedef struct {name} {{\n"));
    for field in &layout.fields {
        out.push_str(&format!(
            "  {} {};\n",
            c_type(&field.ty)?,
            c_ident(&field.name)
        ));
    }
    out.push_str(&format!("}} {name};\n"));
    Ok(())
}

fn emit_enum_layouts(out: &mut String, program: &IrProgram) -> KuResult<()> {
    for layout in &program.layouts.enums {
        emit_enum_layout(out, layout)?;
    }
    if !program.layouts.enums.is_empty() {
        out.push('\n');
    }
    Ok(())
}

fn emit_enum_layout(out: &mut String, layout: &IrEnumLayout) -> KuResult<()> {
    let name = c_enum_type(&layout.name);
    out.push_str(&format!(
        "typedef struct {name} {{\n  int32_t tag;\n  union {{\n"
    ));
    let mut emitted_payload = false;
    for variant in &layout.variants {
        if variant.fields.is_empty() {
            continue;
        }
        emitted_payload = true;
        out.push_str("    struct {\n");
        for field in &variant.fields {
            out.push_str(&format!(
                "      {} {};\n",
                c_type(&field.ty)?,
                c_ident(&field.name)
            ));
        }
        out.push_str(&format!("    }} {};\n", c_ident(&variant.name)));
    }
    if !emitted_payload {
        out.push_str("    unsigned char empty;\n");
    }
    out.push_str(&format!("  }} payload;\n}} {name};\n"));
    Ok(())
}

fn emit_array_abi(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut element_types = Vec::new();
    for function in &program.functions {
        collect_array_element_type(&function.return_type, &mut element_types);
        for param in &function.params {
            collect_array_element_type(&param.ty, &mut element_types);
        }
        for block in &function.blocks {
            for inst in &block.instructions {
                match inst {
                    IrInst::Temp { ty, value, .. } | IrInst::Let { ty, value, .. } => {
                        collect_array_element_type(ty, &mut element_types);
                        collect_array_expr_types(value, &mut element_types);
                    }
                    IrInst::BindOk { ty, result, .. } => {
                        collect_array_element_type(ty, &mut element_types);
                        collect_array_expr_types(result, &mut element_types);
                    }
                    IrInst::Store { target, value } => {
                        collect_array_lvalue_types(target, &mut element_types);
                        collect_array_expr_types(value, &mut element_types);
                    }
                    IrInst::Print(value)
                    | IrInst::Expr(value)
                    | IrInst::Fail(value)
                    | IrInst::Panic(value) => {
                        collect_array_expr_types(value, &mut element_types);
                    }
                    IrInst::BeginTry { .. }
                    | IrInst::EndTry
                    | IrInst::BindError { .. }
                    | IrInst::DefineClosure { .. }
                    | IrInst::Unsupported { .. } => {}
                }
            }
        }
    }
    if element_types.is_empty() {
        return Ok(());
    }

    out.push_str(
        "static void ku_array_bounds_fail(int64_t index, size_t len) {\n  fprintf(stderr, \"array index %lld out of bounds for length %zu\\n\", (long long)index, len);\n  exit(1);\n}\n\n",
    );
    for element in &element_types {
        let array_type = c_array_type(element)?;
        let suffix = c_type_suffix(element)?;
        let element_type = c_type(element)?;
        let clone_element = c_clone_value(element, "array.data[index]")?;
        let drop_element = c_drop_value(element, "array->data[index]")?;
        out.push_str(&format!(
            "typedef struct {{ size_t len; {element_type}* data; }} {array_type};\n\
             static {array_type} ku_array_make_{suffix}(size_t len, const {element_type}* values) {{\n\
             \x20 {array_type} result = {{ len, NULL }};\n\
             \x20 if (len == 0) return result;\n\
             \x20 if (len > SIZE_MAX / sizeof({element_type})) {{ fprintf(stderr, \"array allocation is too large\\n\"); exit(1); }}\n\
             \x20 result.data = ({element_type}*)malloc(len * sizeof({element_type}));\n\
             \x20 if (!result.data) {{ fprintf(stderr, \"array allocation failed\\n\"); exit(1); }}\n\
             \x20 memcpy(result.data, values, len * sizeof({element_type}));\n\
             \x20 return result;\n\
             }}\n\
             static {array_type} ku_array_clone_{suffix}({array_type} array) {{\n\
             \x20 {array_type} result = ku_array_make_{suffix}(array.len, array.data);\n\
             \x20 for (size_t index = 0; index < result.len; index++) result.data[index] = {clone_element};\n\
             \x20 return result;\n\
             }}\n\
             static {array_type} ku_array_move_{suffix}({array_type}* array) {{\n\
             \x20 {array_type} result = *array;\n\
             \x20 array->len = 0;\n\
             \x20 array->data = NULL;\n\
             \x20 return result;\n\
             }}\n\
             static void ku_array_drop_{suffix}({array_type}* array) {{\n\
             \x20 if (!array || !array->data) return;\n\
             \x20 for (size_t index = 0; index < array->len; index++) {{ {drop_element} }}\n\
             \x20 free(array->data);\n\
             \x20 array->data = NULL;\n\
             \x20 array->len = 0;\n\
             }}\n\
             static {element_type} ku_array_get_{suffix}({array_type} array, int64_t index) {{\n\
             \x20 if (index < 0 || (uint64_t)index >= array.len) ku_array_bounds_fail(index, array.len);\n\
             \x20 return array.data[index];\n\
             }}\n\
             static {element_type}* ku_array_at_{suffix}({array_type}* array, int64_t index) {{\n\
             \x20 if (index < 0 || (uint64_t)index >= array->len) ku_array_bounds_fail(index, array->len);\n\
             \x20 return &array->data[index];\n\
             }}\n\n"
        ));
    }
    Ok(())
}

fn collect_array_element_type(ty: &IrType, output: &mut Vec<IrType>) {
    match ty {
        IrType::Array(inner) => {
            collect_array_element_type(inner, output);
            if !output.contains(inner.as_ref()) {
                output.push(*inner.clone());
            }
        }
        IrType::Result(inner) => collect_array_element_type(inner, output),
        _ => {}
    }
}

fn collect_array_expr_types(expr: &IrExpr, output: &mut Vec<IrType>) {
    collect_array_element_type(&expr.ty, output);
    match &expr.kind {
        IrExprKind::Unary { expr, .. } | IrExprKind::TryUnwrap(expr) => {
            collect_array_expr_types(expr, output)
        }
        IrExprKind::Binary { left, right, .. } => {
            collect_array_expr_types(left, output);
            collect_array_expr_types(right, output);
        }
        IrExprKind::Call { callee, args, .. } => {
            collect_array_expr_types(callee, output);
            for arg in args {
                collect_array_expr_types(arg, output);
            }
        }
        IrExprKind::Array(values) => {
            for value in values {
                collect_array_expr_types(value, output);
            }
        }
        IrExprKind::Index { target, index } => {
            collect_array_expr_types(target, output);
            collect_array_expr_types(index, output);
        }
        IrExprKind::Field { target, .. } => collect_array_expr_types(target, output),
        IrExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                collect_array_expr_types(value, output);
            }
        }
        IrExprKind::Literal(_) | IrExprKind::Local(_) | IrExprKind::Temp(_) => {}
    }
}

fn collect_array_lvalue_types(target: &IrLValue, output: &mut Vec<IrType>) {
    match target {
        IrLValue::Local(_) => {}
        IrLValue::Index { target, index } => {
            collect_array_expr_types(target, output);
            collect_array_expr_types(index, output);
        }
        IrLValue::Field { target, .. } => collect_array_expr_types(target, output),
    }
}

fn emit_function(out: &mut String, function: &IrFunction) -> KuResult<()> {
    let owned_locals = collect_owned_locals(function);
    out.push_str(&format!(
        "{} {}(",
        c_type(&function.return_type)?,
        c_symbol(&function.name)
    ));
    for (index, param) in function.params.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("{} {}", c_type(&param.ty)?, c_ident(&param.name)));
    }
    out.push_str(") {\n");
    for local in &owned_locals {
        if local.is_param {
            continue;
        }
        out.push_str(&format!(
            "  {} {} = {};\n",
            c_type(&local.ty)?,
            local.name,
            c_zero_initializer(&local.ty)?
        ));
    }
    for block in &function.blocks {
        emit_block(out, block, &function.return_type, &owned_locals)?;
    }
    if function.return_type == IrType::Void {
        emit_owned_cleanup(out, &owned_locals)?;
        out.push_str("  return;\n");
    }
    out.push_str("}\n");
    Ok(())
}

fn emit_block(
    out: &mut String,
    block: &IrBlock,
    return_type: &IrType,
    owned_locals: &[OwnedLocal],
) -> KuResult<()> {
    if block.id.0 != 0 {
        out.push_str(&format!("block{}:;\n", block.id.0));
    }
    for inst in &block.instructions {
        emit_inst(out, inst, return_type, owned_locals)?;
    }
    if block.terminator == IrTerminator::Unreachable
        && matches!(
            block.instructions.last(),
            Some(IrInst::Fail(_) | IrInst::Panic(_))
        )
    {
        return Ok(());
    }
    emit_terminator(out, &block.terminator, return_type, owned_locals)
}

fn emit_inst(
    out: &mut String,
    inst: &IrInst,
    return_type: &IrType,
    owned_locals: &[OwnedLocal],
) -> KuResult<()> {
    match inst {
        IrInst::Temp { id, ty, value } => {
            if is_c_owned_type(ty) {
                out.push_str(&format!("  t{} = {};\n", id.0, c_value_expr(value)?));
            } else {
                out.push_str(&format!(
                    "  {} t{} = {};\n",
                    c_type(ty)?,
                    id.0,
                    c_expr(value)?
                ));
            }
        }
        IrInst::BindOk { id, ty, result } => {
            if is_c_owned_type(ty) {
                out.push_str(&format!(
                    "  t{} = ku_result_take_{}(&{});\n",
                    id.0,
                    c_type_suffix(ty)?,
                    c_addressable_expr(result)?
                ));
            } else {
                out.push_str(&format!(
                    "  {} t{} = ku_result_take_{}(&{});\n",
                    c_type(ty)?,
                    id.0,
                    c_type_suffix(ty)?,
                    c_addressable_expr(result)?
                ));
            }
        }
        IrInst::Let { name, ty, value } => {
            if is_c_owned_type(ty) {
                out.push_str(&format!(
                    "  {} = {};\n",
                    c_ident(name),
                    if is_native_zero(value) {
                        c_zero_initializer(ty)?
                    } else {
                        c_value_expr(value)?
                    }
                ));
            } else {
                out.push_str(&format!(
                    "  {} {} = {};\n",
                    c_type(ty)?,
                    c_ident(name),
                    if is_native_zero(value) {
                        c_zero_initializer(ty)?
                    } else {
                        c_value_expr(value)?
                    }
                ));
            }
        }
        IrInst::Store { target, value } => {
            if let IrLValue::Local(name) = target {
                if let Some(local) = owned_locals.iter().find(|local| local.source_name == *name) {
                    out.push_str(&format!(
                        "  {{ {} __ku_store = {};\n",
                        c_type(&local.ty)?,
                        c_value_expr(value)?
                    ));
                    emit_drop_expr(out, &local.ty, &local.name)?;
                    out.push_str(&format!("  {} = __ku_store; }}\n", local.name));
                    return Ok(());
                }
            }
            out.push_str(&format!(
                "  {} = {};\n",
                c_lvalue(target)?,
                c_value_expr(value)?
            ));
        }
        IrInst::Print(value) => emit_print(out, value)?,
        IrInst::Expr(value) => emit_expr_statement(out, value)?,
        IrInst::Fail(value) => {
            let IrType::Result(inner) = return_type else {
                return Err(unsupported("native C fail requires a Result return type"));
            };
            let result = format!(
                "({}){{ false, {}, {} }}",
                c_type(return_type)?,
                c_zero_value(inner)?,
                c_error_expr(value)?
            );
            emit_owned_cleanup(out, owned_locals)?;
            out.push_str(&format!("  return {result};\n"));
        }
        IrInst::Panic(value) => {
            if value.ty == IrType::Str {
                out.push_str(&format!(
                    "  fprintf(stderr, \"%s\\n\", {}); exit(1);\n",
                    c_expr(value)?
                ));
            } else {
                out.push_str("  fprintf(stderr, \"panic\\n\"); exit(1);\n");
            }
        }
        IrInst::BeginTry { .. } | IrInst::EndTry => {}
        IrInst::BindError { name, result } => {
            out.push_str(&format!(
                "  KuError {} = {}.error;\n",
                c_ident(name),
                c_expr(result)?
            ));
        }
        IrInst::DefineClosure { .. } | IrInst::Unsupported { .. } => {
            return Err(unsupported(format!(
                "native C prototype cannot lower IR instruction '{inst}'"
            )));
        }
    }
    Ok(())
}

fn emit_expr_statement(out: &mut String, value: &IrExpr) -> KuResult<()> {
    out.push_str(&format!("  (void){};\n", c_expr(value)?));
    Ok(())
}

fn emit_print(out: &mut String, value: &IrExpr) -> KuResult<()> {
    match value.ty {
        IrType::Int | IrType::Bool => {
            out.push_str(&format!(
                "  printf(\"%lld\\n\", (long long){});\n",
                c_expr(value)?
            ));
        }
        IrType::Str => {
            out.push_str(&format!("  printf(\"%s\\n\", {});\n", c_expr(value)?));
        }
        _ => {
            return Err(unsupported(
                "native C prototype print supports int/bool/str",
            ))
        }
    }
    Ok(())
}

fn emit_terminator(
    out: &mut String,
    terminator: &IrTerminator,
    return_type: &IrType,
    owned_locals: &[OwnedLocal],
) -> KuResult<()> {
    match terminator {
        IrTerminator::Next => Ok(()),
        IrTerminator::Jump(target) => {
            out.push_str(&format!("  goto block{};\n", target.0));
            Ok(())
        }
        IrTerminator::Branch {
            condition,
            then_block,
            else_block,
        } => {
            out.push_str(&format!(
                "  if ({}) goto block{}; else goto block{};\n",
                c_expr(condition)?,
                then_block.0,
                else_block.0
            ));
            Ok(())
        }
        IrTerminator::ForEach { .. } => Err(unsupported(
            "native C prototype does not support for lowering yet",
        )),
        IrTerminator::ResultBranch {
            result,
            ok_block,
            err_block,
        } => {
            out.push_str(&format!(
                "  if ({}.ok) goto block{}; else goto block{};\n",
                c_expr(result)?,
                ok_block.0,
                err_block.0
            ));
            Ok(())
        }
        IrTerminator::JumpErr { result, target } => {
            out.push_str(&format!(
                "  (void){}; goto block{};\n",
                c_expr(result)?,
                target.0
            ));
            Ok(())
        }
        IrTerminator::PropagateErr(value) => {
            let IrType::Result(return_inner) = return_type else {
                return Err(unsupported(
                    "native C prototype can only propagate errors from Result functions",
                ));
            };
            if !matches!(value.ty, IrType::Result(_)) {
                return Err(unsupported(
                    "native C error propagation requires a Result value",
                ));
            }
            let result = c_expr(value)?;
            out.push_str(&format!("  {{ KuError __ku_error = {}.error;\n", result));
            emit_owned_cleanup(out, owned_locals)?;
            out.push_str(&format!(
                "  return ({}){{ false, {}, __ku_error }}; }}\n",
                c_type(return_type)?,
                c_zero_value(return_inner)?
            ));
            Ok(())
        }
        IrTerminator::Return(Some(value)) => {
            if is_c_owned_type(&value.ty) {
                out.push_str(&format!(
                    "  {{ {} __ku_return = {};\n",
                    c_type(&value.ty)?,
                    c_value_expr(value)?
                ));
                emit_owned_cleanup(out, owned_locals)?;
                out.push_str("  return __ku_return; }\n");
            } else {
                let value = c_value_expr(value)?;
                emit_owned_cleanup(out, owned_locals)?;
                out.push_str(&format!("  return {value};\n"));
            }
            Ok(())
        }
        IrTerminator::Return(None) => {
            emit_owned_cleanup(out, owned_locals)?;
            out.push_str("  return;\n");
            Ok(())
        }
        IrTerminator::Unreachable => {
            out.push_str("  abort();\n");
            Ok(())
        }
    }
}

fn c_expr(expr: &IrExpr) -> KuResult<String> {
    match &expr.kind {
        IrExprKind::Literal(value) => {
            if value == "<native-zero>" {
                c_zero_initializer(&expr.ty)
            } else if expr.ty == IrType::Null && value == "null" {
                Ok("0".to_string())
            } else {
                Ok(value.clone())
            }
        }
        IrExprKind::Local(name) => Ok(c_symbol(name)),
        IrExprKind::Temp(id) => Ok(format!("t{}", id.0)),
        IrExprKind::StructLiteral { name, fields } => {
            let fields = fields
                .iter()
                .map(|(field, value)| Ok(format!(".{} = {}", c_ident(field), c_expr(value)?)))
                .collect::<KuResult<Vec<_>>>()?
                .join(", ");
            Ok(format!("({}){{ {fields} }}", c_struct_type(name)))
        }
        IrExprKind::Unary { op, expr } => Ok(format!("({}{})", c_unary(*op), c_expr(expr)?)),
        IrExprKind::Binary { left, op, right }
            if left.ty == IrType::Str
                && right.ty == IrType::Str
                && matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) =>
        {
            Ok(format!(
                "(strcmp({}, {}) {} 0)",
                c_expr(left)?,
                c_expr(right)?,
                if *op == BinaryOp::Equal { "==" } else { "!=" }
            ))
        }
        IrExprKind::Binary { left, op, .. }
            if matches!(left.ty, IrType::Array(_))
                && matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) =>
        {
            Err(unsupported(
                "native C prototype does not support array equality yet",
            ))
        }
        IrExprKind::Binary { left, op, .. }
            if matches!(
                &left.ty,
                IrType::Named(name) if enum_type_name(name).is_some()
            ) && matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) =>
        {
            Err(unsupported(
                "native C prototype does not support enum equality yet",
            ))
        }
        IrExprKind::Binary { left, op, right } => Ok(format!(
            "({} {} {})",
            c_expr(left)?,
            c_binary(*op),
            c_expr(right)?
        )),
        IrExprKind::Call { callee, args, kind } => {
            if let IrCallKind::Intrinsic(name) = kind {
                return c_intrinsic_expr(name, args, &expr.ty);
            }
            if !matches!(kind, IrCallKind::Direct(_)) {
                return Err(unsupported(
                    "native C prototype only supports direct function calls",
                ));
            }
            let callee = c_expr(callee)?;
            let args = args
                .iter()
                .map(c_value_expr)
                .collect::<KuResult<Vec<_>>>()?
                .join(", ");
            Ok(format!("{callee}({args})"))
        }
        IrExprKind::Field { target, name } => {
            Ok(format!("({}).{}", c_expr(target)?, c_ident(name)))
        }
        IrExprKind::Array(values) => {
            let IrType::Array(element) = &expr.ty else {
                return Err(unsupported(
                    "native C array literal is missing its element type",
                ));
            };
            if values.is_empty() {
                return Ok(format!(
                    "ku_array_make_{}(0, NULL)",
                    c_type_suffix(element)?
                ));
            }
            let len = values.len();
            let values = values
                .iter()
                .map(c_value_expr)
                .collect::<KuResult<Vec<_>>>()?
                .join(", ");
            Ok(format!(
                "ku_array_make_{}({}, ({}[]){{ {} }})",
                c_type_suffix(element)?,
                len,
                c_type(element)?,
                values
            ))
        }
        IrExprKind::Index { target, index } => {
            let IrType::Array(element) = &target.ty else {
                return Err(unsupported(
                    "native C index expression requires an array target",
                ));
            };
            Ok(format!(
                "ku_array_get_{}({}, {})",
                c_type_suffix(element)?,
                c_expr(target)?,
                c_expr(index)?
            ))
        }
        IrExprKind::TryUnwrap(_) => Err(unsupported(format!(
            "native C prototype cannot lower expression '{expr}'"
        ))),
    }
}

fn c_value_expr(expr: &IrExpr) -> KuResult<String> {
    if let IrExprKind::Call { kind, args, .. } = &expr.kind {
        if matches!(kind, IrCallKind::Intrinsic(name) if name == "__ku_clone") {
            let value = args
                .first()
                .ok_or_else(|| unsupported("clone intrinsic requires one argument"))?;
            return c_clone_expr(value);
        }
    }
    if let IrType::Array(element) = &expr.ty {
        match &expr.kind {
            IrExprKind::Local(name) => {
                return Ok(format!(
                    "ku_array_move_{}(&{})",
                    c_type_suffix(element)?,
                    c_symbol(name)
                ))
            }
            IrExprKind::Temp(id) => {
                return Ok(format!(
                    "ku_array_move_{}(&t{})",
                    c_type_suffix(element)?,
                    id.0
                ))
            }
            _ => {}
        }
    }
    if let IrType::Result(inner) = &expr.ty {
        match &expr.kind {
            IrExprKind::Local(name) => {
                return Ok(format!(
                    "ku_result_move_{}(&{})",
                    c_type_suffix(inner)?,
                    c_symbol(name)
                ))
            }
            IrExprKind::Temp(id) => {
                return Ok(format!(
                    "ku_result_move_{}(&t{})",
                    c_type_suffix(inner)?,
                    id.0
                ))
            }
            _ => {}
        }
    }
    if let IrType::Named(name) = &expr.ty {
        match &expr.kind {
            IrExprKind::Local(local) => {
                return Ok(format!(
                    "{}(&{})",
                    c_named_move_function(name),
                    c_symbol(local)
                ))
            }
            IrExprKind::Temp(id) => {
                return Ok(format!("{}(&t{})", c_named_move_function(name), id.0))
            }
            _ => {}
        }
    }
    c_expr(expr)
}

fn c_clone_expr(expr: &IrExpr) -> KuResult<String> {
    match &expr.ty {
        IrType::Array(element) => Ok(format!(
            "ku_array_clone_{}({})",
            c_type_suffix(element)?,
            c_expr(expr)?
        )),
        IrType::Str => c_expr(expr),
        IrType::Result(inner) => Ok(format!(
            "ku_result_clone_{}({})",
            c_type_suffix(inner)?,
            c_expr(expr)?
        )),
        IrType::Named(name) => Ok(format!(
            "{}({})",
            c_named_clone_function(name),
            c_expr(expr)?
        )),
        _ => Err(unsupported(format!(
            "native C clone() is not implemented for {} yet",
            expr.ty
        ))),
    }
}

fn c_addressable_expr(expr: &IrExpr) -> KuResult<String> {
    match &expr.kind {
        IrExprKind::Local(name) => Ok(c_symbol(name)),
        IrExprKind::Temp(id) => Ok(format!("t{}", id.0)),
        _ => Err(unsupported(
            "native C Result unwrapping requires a materialized local result",
        )),
    }
}

fn c_error_expr(value: &IrExpr) -> KuResult<String> {
    if value.ty == IrType::Named("__ku_error_type".to_string()) {
        c_expr(value)
    } else if value.ty == IrType::Str {
        Ok(format!("ku_error_message({})", c_expr(value)?))
    } else {
        Err(unsupported(
            "native C errors currently require a string or Error value",
        ))
    }
}

fn c_lvalue(target: &IrLValue) -> KuResult<String> {
    match target {
        IrLValue::Local(name) => Ok(c_ident(name)),
        IrLValue::Field { target, name } => Ok(format!("({}).{}", c_expr(target)?, c_ident(name))),
        IrLValue::Index { target, index } => {
            let IrType::Array(element) = &target.ty else {
                return Err(unsupported(
                    "native C index assignment requires an array target",
                ));
            };
            Ok(format!(
                "*ku_array_at_{}(&({}), {})",
                c_type_suffix(element)?,
                c_expr(target)?,
                c_expr(index)?
            ))
        }
    }
}

fn c_type(ty: &IrType) -> KuResult<String> {
    match ty {
        IrType::Int => Ok("int64_t".to_string()),
        IrType::Bool => Ok("bool".to_string()),
        IrType::Str => Ok("const char*".to_string()),
        IrType::Null => Ok("uint8_t".to_string()),
        IrType::Array(inner) => c_array_type(inner),
        IrType::Result(inner) => c_result_type(inner),
        IrType::Named(name) if name == "__ku_error_type" => Ok("KuError".to_string()),
        IrType::Named(name) => Ok(match enum_type_name(name) {
            Some(name) => c_enum_type(name),
            None => c_struct_type(name),
        }),
        IrType::Void => Ok("void".to_string()),
        _ => Err(unsupported(format!(
            "native C prototype does not support type {ty}"
        ))),
    }
}

fn emit_result_abi(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut result_types = Vec::new();
    for function in &program.functions {
        collect_result_type(&function.return_type, &mut result_types)?;
        for param in &function.params {
            collect_result_type(&param.ty, &mut result_types)?;
        }
        for block in &function.blocks {
            for inst in &block.instructions {
                match inst {
                    IrInst::Temp { ty, value, .. } => {
                        collect_result_type(ty, &mut result_types)?;
                        collect_result_type(&value.ty, &mut result_types)?;
                    }
                    IrInst::BindOk { result, .. } => {
                        collect_result_type(&result.ty, &mut result_types)?;
                    }
                    IrInst::Let { ty, value, .. } => {
                        collect_result_type(ty, &mut result_types)?;
                        collect_result_type(&value.ty, &mut result_types)?;
                    }
                    IrInst::Store { value, .. }
                    | IrInst::Print(value)
                    | IrInst::Expr(value)
                    | IrInst::Fail(value)
                    | IrInst::Panic(value) => {
                        collect_result_type(&value.ty, &mut result_types)?;
                    }
                    IrInst::BeginTry { .. }
                    | IrInst::EndTry
                    | IrInst::BindError { .. }
                    | IrInst::DefineClosure { .. }
                    | IrInst::Unsupported { .. } => {}
                }
            }
            match &block.terminator {
                IrTerminator::ResultBranch { result, .. }
                | IrTerminator::JumpErr { result, .. }
                | IrTerminator::PropagateErr(result)
                | IrTerminator::Return(Some(result)) => {
                    collect_result_type(&result.ty, &mut result_types)?;
                }
                IrTerminator::Branch { condition, .. } => {
                    collect_result_type(&condition.ty, &mut result_types)?;
                }
                IrTerminator::ForEach { iterable, .. } => {
                    collect_result_type(&iterable.ty, &mut result_types)?;
                }
                IrTerminator::Next
                | IrTerminator::Jump(_)
                | IrTerminator::Return(None)
                | IrTerminator::Unreachable => {}
            }
        }
    }
    for inner in &result_types {
        let result_type = c_result_type(inner)?;
        let suffix = c_type_suffix(inner)?;
        let value_type = c_type(inner)?;
        out.push_str(&format!(
            "typedef struct {{ bool ok; {value_type} value; KuError error; }} {result_type};\n"
        ));
        out.push_str(&format!(
            "static {result_type} ku_result_move_{suffix}({result_type}* result) {{ {result_type} value = *result; *result = ({result_type}){{0}}; return value; }}\n"
        ));
        out.push_str(&format!(
            "static {value_type} ku_result_take_{suffix}({result_type}* result) {{ {value_type} value = result->value; result->value = {}; result->ok = false; return value; }}\n",
            c_zero_value(inner)?
        ));
        out.push_str(&format!(
            "static {result_type} ku_result_clone_{suffix}({result_type} result) {{ if (result.ok) result.value = {}; return result; }}\n",
            c_clone_value(inner, "result.value")?
        ));
        out.push_str(&format!(
            "static void ku_result_drop_{suffix}({result_type}* result) {{ if (!result) return; if (result->ok) {{ {} }} *result = ({result_type}){{0}}; }}\n",
            c_drop_value(inner, "result->value")?
        ));
    }
    if !result_types.is_empty() {
        out.push('\n');
    }
    Ok(())
}

fn collect_result_type(ty: &IrType, result_types: &mut Vec<IrType>) -> KuResult<()> {
    match ty {
        IrType::Result(inner) => {
            match inner.as_ref() {
                IrType::Int
                | IrType::Bool
                | IrType::Str
                | IrType::Null
                | IrType::Array(_)
                | IrType::Named(_) => {}
                _ => {
                    return Err(unsupported(format!(
                        "native C prototype does not support Result<{inner}>"
                    )))
                }
            }
            if !result_types.contains(inner.as_ref()) {
                result_types.push(*inner.clone());
            }
        }
        IrType::Array(inner) => collect_result_type(inner, result_types)?,
        _ => {}
    }
    Ok(())
}

fn c_result_type(inner: &IrType) -> KuResult<String> {
    Ok(format!("KuResult_{}", c_type_suffix(inner)?))
}

fn emit_main_wrapper(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let Some(function) = program
        .functions
        .iter()
        .find(|function| function.name == "main")
    else {
        return Ok(());
    };
    if !function.params.is_empty() {
        return Err(unsupported(
            "native C main wrapper does not support main parameters",
        ));
    }
    out.push_str("int main(void) {\n");
    match &function.return_type {
        IrType::Void => {
            out.push_str("  ku_main();\n  return 0;\n");
        }
        IrType::Int => {
            out.push_str("  return (int)ku_main();\n");
        }
        IrType::Bool => {
            out.push_str("  return ku_main() ? 0 : 1;\n");
        }
        IrType::Str => {
            out.push_str("  const char* result = ku_main();\n  if (result) printf(\"%s\\n\", result);\n  return 0;\n");
        }
        IrType::Result(inner) => {
            out.push_str(&format!(
                "  {} result = ku_main();\n  if (!result.ok) {{ fprintf(stderr, \"%s\\n\", result.error.message ? result.error.message : \"error\"); ku_result_drop_{}(&result); return 1; }}\n  ku_result_drop_{}(&result);\n  return 0;\n",
                c_type(&function.return_type)?,
                c_type_suffix(inner)?,
                c_type_suffix(inner)?
            ));
        }
        other => {
            return Err(unsupported(format!(
                "native C main wrapper does not support main return type {other}"
            )));
        }
    }
    out.push_str("}\n");
    Ok(())
}

fn c_intrinsic_expr(name: &str, args: &[IrExpr], ty: &IrType) -> KuResult<String> {
    if let Some(rest) = name.strip_prefix("__ku_enum:") {
        let mut parts = rest.splitn(4, ':');
        let enum_name = parts
            .next()
            .ok_or_else(|| unsupported("invalid native enum constructor"))?;
        let variant = parts
            .next()
            .ok_or_else(|| unsupported("invalid native enum constructor"))?;
        let tag = parts
            .next()
            .ok_or_else(|| unsupported("invalid native enum constructor"))?;
        let fields = parts.next().unwrap_or_default();
        let field_names = if fields.is_empty() {
            Vec::new()
        } else {
            fields.split(',').collect::<Vec<_>>()
        };
        if field_names.len() != args.len() {
            return Err(unsupported(format!(
                "native enum constructor '{enum_name}.{variant}' payload metadata mismatch"
            )));
        }
        let mut initializer = format!("({}){{ .tag = {tag}", c_type(ty)?);
        if !args.is_empty() {
            let fields = field_names
                .iter()
                .zip(args)
                .map(|(field, value)| Ok(format!(".{} = {}", c_ident(field), c_expr(value)?)))
                .collect::<KuResult<Vec<_>>>()?
                .join(", ");
            initializer.push_str(&format!(", .payload.{} = {{ {fields} }}", c_ident(variant)));
        }
        initializer.push_str(" }");
        return Ok(initializer);
    }
    if name == "__ku_enum_tag" {
        let value = args
            .first()
            .ok_or_else(|| unsupported("enum tag requires one argument"))?;
        return Ok(format!("({}).tag", c_expr(value)?));
    }
    if let Some(rest) = name.strip_prefix("__ku_enum_payload:") {
        let (variant, field) = rest
            .split_once(':')
            .ok_or_else(|| unsupported("invalid native enum payload access"))?;
        let value = args
            .first()
            .ok_or_else(|| unsupported("enum payload access requires one argument"))?;
        return Ok(format!(
            "({}).payload.{}.{}",
            c_expr(value)?,
            c_ident(variant),
            c_ident(field)
        ));
    }
    match (name, ty) {
        ("ok", IrType::Result(_)) => {
            let value = args
                .first()
                .ok_or_else(|| unsupported("ok requires one argument"))?;
            Ok(format!(
                "({}){{ true, {}, (KuError){{0}} }}",
                c_type(ty)?,
                c_value_expr(value)?
            ))
        }
        ("err", IrType::Result(inner)) => {
            let value = args
                .first()
                .ok_or_else(|| unsupported("err requires one argument"))?;
            Ok(format!(
                "({}){{ false, {}, {} }}",
                c_type(ty)?,
                c_zero_value(inner)?,
                c_error_expr(value)?
            ))
        }
        _ => Err(unsupported(format!(
            "native C prototype cannot lower intrinsic '{name}'"
        ))),
    }
}

fn c_zero_value(ty: &IrType) -> KuResult<String> {
    match ty {
        IrType::Int | IrType::Null => Ok("0".to_string()),
        IrType::Bool => Ok("false".to_string()),
        IrType::Str => Ok("(const char*)0".to_string()),
        IrType::Array(_) | IrType::Named(_) | IrType::Result(_) => {
            Ok(format!("({}){{0}}", c_type(ty)?))
        }
        _ => Err(unsupported(format!(
            "native C prototype does not support zero value for {ty}"
        ))),
    }
}

fn is_native_zero(expr: &IrExpr) -> bool {
    matches!(&expr.kind, IrExprKind::Literal(value) if value == "<native-zero>")
}

fn c_zero_initializer(ty: &IrType) -> KuResult<String> {
    match ty {
        IrType::Int | IrType::Null => Ok("0".to_string()),
        IrType::Bool => Ok("false".to_string()),
        IrType::Str => Ok("(const char*)0".to_string()),
        IrType::Array(_) | IrType::Named(_) | IrType::Result(_) => {
            Ok(format!("({}){{0}}", c_type(ty)?))
        }
        _ => Err(unsupported(format!(
            "native C prototype does not support zero initialization for {ty}"
        ))),
    }
}

fn c_binary(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Subtract => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::Remainder => "%",
        BinaryOp::Equal => "==",
        BinaryOp::NotEqual => "!=",
        BinaryOp::Less => "<",
        BinaryOp::LessEqual => "<=",
        BinaryOp::Greater => ">",
        BinaryOp::GreaterEqual => ">=",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
    }
}

fn c_clone_value(ty: &IrType, expression: &str) -> KuResult<String> {
    match ty {
        IrType::Array(element) => Ok(format!(
            "ku_array_clone_{}({expression})",
            c_type_suffix(element)?
        )),
        IrType::Named(name) if name != "__ku_error_type" => {
            Ok(format!("{}({expression})", c_named_clone_function(name)))
        }
        IrType::Int | IrType::Bool | IrType::Str | IrType::Null => Ok(expression.to_string()),
        _ => Err(unsupported(format!(
            "native C clone is not implemented for {ty}"
        ))),
    }
}

fn c_drop_value(ty: &IrType, expression: &str) -> KuResult<String> {
    match ty {
        IrType::Array(element) => Ok(format!(
            "ku_array_drop_{}(&{expression});",
            c_type_suffix(element)?
        )),
        IrType::Named(name) if name != "__ku_error_type" => {
            Ok(format!("{}(&{expression});", c_named_drop_function(name)))
        }
        IrType::Int | IrType::Bool | IrType::Str | IrType::Null => Ok(String::new()),
        _ => Err(unsupported(format!(
            "native C drop is not implemented for {ty}"
        ))),
    }
}

fn c_unary(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Negate => "-",
        UnaryOp::Not => "!",
    }
}

fn c_ident(name: &str) -> String {
    let mut output = String::new();
    for (index, ch) in name.chars().enumerate() {
        if (index == 0 && (ch.is_ascii_alphabetic() || ch == '_'))
            || (index > 0 && (ch.is_ascii_alphanumeric() || ch == '_'))
        {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "_".to_string()
    } else {
        output
    }
}

fn c_symbol(name: &str) -> String {
    if name == "main" {
        "ku_main".to_string()
    } else {
        c_ident(name)
    }
}

fn c_struct_type(name: &str) -> String {
    format!("KuStruct_{}", c_ident(name))
}

fn c_enum_type(name: &str) -> String {
    format!("KuEnum_{}", c_ident(name))
}

fn c_array_type(element: &IrType) -> KuResult<String> {
    Ok(format!("KuArray_{}", c_type_suffix(element)?))
}

fn c_type_suffix(ty: &IrType) -> KuResult<String> {
    match ty {
        IrType::Int => Ok("int".to_string()),
        IrType::Bool => Ok("bool".to_string()),
        IrType::Str => Ok("str".to_string()),
        IrType::Null => Ok("null".to_string()),
        IrType::Named(name) => Ok(match enum_type_name(name) {
            Some(name) => format!("enum_{}", c_ident(name)),
            None => format!("struct_{}", c_ident(name)),
        }),
        IrType::Array(inner) => Ok(format!("array_{}", c_type_suffix(inner)?)),
        _ => Err(unsupported(format!(
            "native C prototype does not support arrays of {ty}"
        ))),
    }
}

fn enum_type_name(name: &str) -> Option<&str> {
    name.strip_prefix("__ku_enum_type:")
}

fn unsupported(message: impl Into<String>) -> KuError {
    KuError::runtime(message.into(), Span::default())
}

fn is_c_owned_type(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::Str | IrType::Array(_) | IrType::Result(_) | IrType::Named(_)
    )
}

fn collect_owned_locals(function: &IrFunction) -> Vec<OwnedLocal> {
    let mut locals = Vec::new();
    for param in &function.params {
        if is_c_owned_type(&param.ty) {
            locals.push(OwnedLocal {
                source_name: param.name.clone(),
                name: c_ident(&param.name),
                ty: param.ty.clone(),
                is_param: true,
            });
        }
    }
    for block in &function.blocks {
        for instruction in &block.instructions {
            match instruction {
                IrInst::Temp { id, ty, .. } if is_c_owned_type(ty) => locals.push(OwnedLocal {
                    source_name: format!("%t{}", id.0),
                    name: format!("t{}", id.0),
                    ty: ty.clone(),
                    is_param: false,
                }),
                IrInst::BindOk { id, ty, .. } if is_c_owned_type(ty) => locals.push(OwnedLocal {
                    source_name: format!("%t{}", id.0),
                    name: format!("t{}", id.0),
                    ty: ty.clone(),
                    is_param: false,
                }),
                IrInst::Let { name, ty, .. }
                    if is_c_owned_type(ty)
                        && !locals.iter().any(|local| local.source_name == *name) =>
                {
                    locals.push(OwnedLocal {
                        source_name: name.clone(),
                        name: c_ident(name),
                        ty: ty.clone(),
                        is_param: false,
                    });
                }
                _ => {}
            }
        }
    }
    locals
}

fn emit_owned_cleanup(out: &mut String, locals: &[OwnedLocal]) -> KuResult<()> {
    for local in locals.iter().rev() {
        emit_drop_expr(out, &local.ty, &local.name)?;
    }
    Ok(())
}

fn emit_drop_expr(out: &mut String, ty: &IrType, expression: &str) -> KuResult<()> {
    match ty {
        IrType::Str => Ok(()),
        IrType::Array(element) => {
            out.push_str(&format!(
                "  ku_array_drop_{}(&{});\n",
                c_type_suffix(element)?,
                expression
            ));
            Ok(())
        }
        IrType::Named(name) => {
            out.push_str(&format!(
                "  {}(&{});\n",
                c_named_drop_function(name),
                expression
            ));
            Ok(())
        }
        IrType::Result(inner) => {
            out.push_str(&format!(
                "  ku_result_drop_{}(&{});\n",
                c_type_suffix(inner)?,
                expression
            ));
            Ok(())
        }
        _ => Err(unsupported(format!(
            "native C drop is not implemented for {ty}"
        ))),
    }
}

fn emit_named_ownership_helpers(out: &mut String, program: &IrProgram) -> KuResult<()> {
    for layout in &program.layouts.structs {
        emit_named_ownership_helper(out, &layout.name, false)?;
    }
    for layout in &program.layouts.enums {
        emit_named_ownership_helper(out, &format!("__ku_enum_type:{}", layout.name), true)?;
    }
    if !program.layouts.structs.is_empty() || !program.layouts.enums.is_empty() {
        out.push('\n');
    }
    Ok(())
}

fn emit_named_ownership_helper(out: &mut String, name: &str, is_enum: bool) -> KuResult<()> {
    let ty = IrType::Named(name.to_string());
    let c_ty = c_type(&ty)?;
    let move_fn = c_named_move_function(name);
    let clone_fn = c_named_clone_function(name);
    let drop_fn = c_named_drop_function(name);
    if is_enum {
        out.push_str(&format!(
            "static {c_ty} {move_fn}({c_ty}* value) {{ {c_ty} result = *value; memset(value, 0, sizeof(*value)); value->tag = -1; return result; }}\n\
             static {c_ty} {clone_fn}({c_ty} value) {{ return value; }}\n\
             static void {drop_fn}({c_ty}* value) {{ if (value) {{ memset(value, 0, sizeof(*value)); value->tag = -1; }} }}\n"
        ));
    } else {
        out.push_str(&format!(
            "static {c_ty} {move_fn}({c_ty}* value) {{ {c_ty} result = *value; *value = ({c_ty}){{0}}; return result; }}\n\
             static {c_ty} {clone_fn}({c_ty} value) {{ return value; }}\n\
             static void {drop_fn}({c_ty}* value) {{ if (value) *value = ({c_ty}){{0}}; }}\n"
        ));
    }
    Ok(())
}

fn c_named_suffix(name: &str) -> String {
    match enum_type_name(name) {
        Some(name) => format!("enum_{}", c_ident(name)),
        None => format!("struct_{}", c_ident(name)),
    }
}

fn c_named_move_function(name: &str) -> String {
    format!("ku_move_{}", c_named_suffix(name))
}

fn c_named_clone_function(name: &str) -> String {
    format!("ku_clone_{}", c_named_suffix(name))
}

fn c_named_drop_function(name: &str) -> String {
    format!("ku_drop_{}", c_named_suffix(name))
}

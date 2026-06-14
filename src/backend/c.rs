use crate::{
    ast::{BinaryOp, UnaryOp},
    error::{KuError, KuResult},
    ir::{
        IrBlock, IrCallKind, IrExpr, IrExprKind, IrFunction, IrInst, IrProgram, IrTerminator,
        IrType,
    },
    span::Span,
};

pub fn generate_c_source(program: &IrProgram) -> KuResult<String> {
    reject_layouts(program)?;
    let mut out = String::from("#include <stdbool.h>\n#include <stdint.h>\n#include <stdio.h>\n\n");
    emit_result_abi(&mut out, program)?;
    for function in &program.functions {
        emit_function(&mut out, function)?;
        out.push('\n');
    }
    emit_main_wrapper(&mut out, program)?;
    Ok(out)
}

fn reject_layouts(program: &IrProgram) -> KuResult<()> {
    if !program.layouts.structs.is_empty() || !program.layouts.enums.is_empty() {
        return Err(unsupported(
            "native C prototype does not support struct or enum layouts yet",
        ));
    }
    Ok(())
}

fn emit_function(out: &mut String, function: &IrFunction) -> KuResult<()> {
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
    for block in &function.blocks {
        emit_block(out, block, &function.return_type)?;
    }
    if function.return_type == IrType::Void {
        out.push_str("  return;\n");
    }
    out.push_str("}\n");
    Ok(())
}

fn emit_block(out: &mut String, block: &IrBlock, return_type: &IrType) -> KuResult<()> {
    if block.id.0 != 0 {
        out.push_str(&format!("block{}:;\n", block.id.0));
    }
    for inst in &block.instructions {
        emit_inst(out, inst, return_type)?;
    }
    emit_terminator(out, &block.terminator, return_type)
}

fn emit_inst(out: &mut String, inst: &IrInst, return_type: &IrType) -> KuResult<()> {
    match inst {
        IrInst::Temp { id, ty, value } => {
            out.push_str(&format!(
                "  {} t{} = {};\n",
                c_type(ty)?,
                id.0,
                c_expr(value)?
            ));
        }
        IrInst::BindOk { id, ty, result } => {
            out.push_str(&format!(
                "  {} t{} = {}.value;\n",
                c_type(ty)?,
                id.0,
                c_expr(result)?
            ));
        }
        IrInst::Let { name, ty, value } => {
            out.push_str(&format!(
                "  {} {} = {};\n",
                c_type(ty)?,
                c_ident(name),
                c_expr(value)?
            ));
        }
        IrInst::Store { target, value } => {
            let crate::ir::IrLValue::Local(name) = target else {
                return Err(unsupported(
                    "native C prototype only supports local assignment",
                ));
            };
            out.push_str(&format!("  {} = {};\n", c_ident(name), c_expr(value)?));
        }
        IrInst::Print(value) => emit_print(out, value)?,
        IrInst::Expr(value) => emit_expr_statement(out, value)?,
        IrInst::Fail(value) => {
            let IrType::Result(inner) = return_type else {
                return Err(unsupported("native C fail requires a Result return type"));
            };
            out.push_str(&format!(
                "  return ({}){{ false, {}, {} }};\n",
                c_type(return_type)?,
                c_zero_value(inner)?,
                c_expr(value)?
            ));
        }
        IrInst::Panic(_)
        | IrInst::BeginTry { .. }
        | IrInst::EndTry
        | IrInst::BindError { .. }
        | IrInst::DefineClosure { .. }
        | IrInst::Unsupported { .. } => {
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
            if !matches!(return_type, IrType::Result(_)) {
                return Err(unsupported(
                    "native C prototype can only propagate errors from Result functions",
                ));
            }
            out.push_str(&format!("  return {};\n", c_expr(value)?));
            Ok(())
        }
        IrTerminator::Return(Some(value)) => {
            out.push_str(&format!("  return {};\n", c_expr(value)?));
            Ok(())
        }
        IrTerminator::Return(None) => {
            out.push_str("  return;\n");
            Ok(())
        }
        IrTerminator::Unreachable => {
            out.push_str("  __builtin_unreachable();\n");
            Ok(())
        }
    }
}

fn c_expr(expr: &IrExpr) -> KuResult<String> {
    match &expr.kind {
        IrExprKind::Literal(value) => Ok(value.clone()),
        IrExprKind::Local(name) => Ok(c_symbol(name)),
        IrExprKind::Temp(id) => Ok(format!("t{}", id.0)),
        IrExprKind::Unary { op, expr } => Ok(format!("({}{})", c_unary(*op), c_expr(expr)?)),
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
                .map(c_expr)
                .collect::<KuResult<Vec<_>>>()?
                .join(", ");
            Ok(format!("{callee}({args})"))
        }
        IrExprKind::Array(_)
        | IrExprKind::Index { .. }
        | IrExprKind::Field { .. }
        | IrExprKind::TryUnwrap(_) => Err(unsupported(format!(
            "native C prototype cannot lower expression '{expr}'"
        ))),
    }
}

fn c_type(ty: &IrType) -> KuResult<&'static str> {
    match ty {
        IrType::Int => Ok("int64_t"),
        IrType::Bool => Ok("bool"),
        IrType::Str => Ok("const char*"),
        IrType::Result(inner) => c_result_type(inner),
        IrType::Void => Ok("void"),
        _ => Err(unsupported(format!(
            "native C prototype does not support type {ty}"
        ))),
    }
}

fn emit_result_abi(out: &mut String, program: &IrProgram) -> KuResult<()> {
    let mut has_int = false;
    let mut has_bool = false;
    let mut has_str = false;
    for function in &program.functions {
        collect_result_type(
            &function.return_type,
            &mut has_int,
            &mut has_bool,
            &mut has_str,
        )?;
        for param in &function.params {
            collect_result_type(&param.ty, &mut has_int, &mut has_bool, &mut has_str)?;
        }
        for block in &function.blocks {
            for inst in &block.instructions {
                match inst {
                    IrInst::Temp { ty, value, .. } => {
                        collect_result_type(ty, &mut has_int, &mut has_bool, &mut has_str)?;
                        collect_result_type(&value.ty, &mut has_int, &mut has_bool, &mut has_str)?;
                    }
                    IrInst::BindOk { result, .. } => {
                        collect_result_type(&result.ty, &mut has_int, &mut has_bool, &mut has_str)?;
                    }
                    IrInst::Let { ty, value, .. } => {
                        collect_result_type(ty, &mut has_int, &mut has_bool, &mut has_str)?;
                        collect_result_type(&value.ty, &mut has_int, &mut has_bool, &mut has_str)?;
                    }
                    IrInst::Store { value, .. }
                    | IrInst::Print(value)
                    | IrInst::Expr(value)
                    | IrInst::Fail(value)
                    | IrInst::Panic(value) => {
                        collect_result_type(&value.ty, &mut has_int, &mut has_bool, &mut has_str)?;
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
                    collect_result_type(&result.ty, &mut has_int, &mut has_bool, &mut has_str)?;
                }
                IrTerminator::Branch { condition, .. } => {
                    collect_result_type(&condition.ty, &mut has_int, &mut has_bool, &mut has_str)?;
                }
                IrTerminator::ForEach { iterable, .. } => {
                    collect_result_type(&iterable.ty, &mut has_int, &mut has_bool, &mut has_str)?;
                }
                IrTerminator::Next
                | IrTerminator::Jump(_)
                | IrTerminator::Return(None)
                | IrTerminator::Unreachable => {}
            }
        }
    }
    if has_int {
        out.push_str(
            "typedef struct { bool ok; int64_t value; const char* error; } KuResultInt;\n",
        );
    }
    if has_bool {
        out.push_str("typedef struct { bool ok; bool value; const char* error; } KuResultBool;\n");
    }
    if has_str {
        out.push_str(
            "typedef struct { bool ok; const char* value; const char* error; } KuResultStr;\n",
        );
    }
    if has_int || has_bool || has_str {
        out.push('\n');
    }
    Ok(())
}

fn collect_result_type(
    ty: &IrType,
    has_int: &mut bool,
    has_bool: &mut bool,
    has_str: &mut bool,
) -> KuResult<()> {
    match ty {
        IrType::Result(inner) => match inner.as_ref() {
            IrType::Int => *has_int = true,
            IrType::Bool => *has_bool = true,
            IrType::Str => *has_str = true,
            _ => {
                return Err(unsupported(format!(
                    "native C prototype does not support Result<{inner}>"
                )))
            }
        },
        IrType::Array(inner) => collect_result_type(inner, has_int, has_bool, has_str)?,
        _ => {}
    }
    Ok(())
}

fn c_result_type(inner: &IrType) -> KuResult<&'static str> {
    match inner {
        IrType::Int => Ok("KuResultInt"),
        IrType::Bool => Ok("KuResultBool"),
        IrType::Str => Ok("KuResultStr"),
        _ => Err(unsupported(format!(
            "native C prototype does not support Result<{inner}>"
        ))),
    }
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
        IrType::Result(_) => {
            out.push_str(&format!(
                "  {} result = ku_main();\n  if (!result.ok) {{ fprintf(stderr, \"%s\\n\", result.error ? result.error : \"error\"); return 1; }}\n  return 0;\n",
                c_type(&function.return_type)?
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
    match (name, ty) {
        ("ok", IrType::Result(_)) => {
            let value = args
                .first()
                .ok_or_else(|| unsupported("ok requires one argument"))?;
            Ok(format!(
                "({}){{ true, {}, (const char*)0 }}",
                c_type(ty)?,
                c_expr(value)?
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
                c_expr(value)?
            ))
        }
        _ => Err(unsupported(format!(
            "native C prototype cannot lower intrinsic '{name}'"
        ))),
    }
}

fn c_zero_value(ty: &IrType) -> KuResult<&'static str> {
    match ty {
        IrType::Int => Ok("0"),
        IrType::Bool => Ok("false"),
        IrType::Str => Ok("(const char*)0"),
        _ => Err(unsupported(format!(
            "native C prototype does not support zero value for {ty}"
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

fn unsupported(message: impl Into<String>) -> KuError {
    KuError::runtime(message.into(), Span::default())
}

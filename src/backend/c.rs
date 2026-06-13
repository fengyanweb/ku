use crate::{
    ast::{BinaryOp, UnaryOp},
    error::{KuError, KuResult},
    ir::{IrBlock, IrExpr, IrExprKind, IrFunction, IrInst, IrProgram, IrTerminator, IrType},
    span::Span,
};

pub fn generate_c_source(program: &IrProgram) -> KuResult<String> {
    reject_layouts(program)?;
    let mut out = String::from("#include <stdbool.h>\n#include <stdint.h>\n#include <stdio.h>\n\n");
    for function in &program.functions {
        emit_function(&mut out, function)?;
        out.push('\n');
    }
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
        c_ident(&function.name)
    ));
    for (index, param) in function.params.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("{} {}", c_type(&param.ty)?, c_ident(&param.name)));
    }
    out.push_str(") {\n");
    for block in &function.blocks {
        emit_block(out, block)?;
    }
    if function.return_type == IrType::Void {
        out.push_str("  return;\n");
    }
    out.push_str("}\n");
    Ok(())
}

fn emit_block(out: &mut String, block: &IrBlock) -> KuResult<()> {
    if block.id.0 != 0 {
        out.push_str(&format!("block{}:\n", block.id.0));
    }
    for inst in &block.instructions {
        emit_inst(out, inst)?;
    }
    emit_terminator(out, &block.terminator)
}

fn emit_inst(out: &mut String, inst: &IrInst) -> KuResult<()> {
    match inst {
        IrInst::Temp { id, ty, value } => {
            out.push_str(&format!(
                "  {} t{} = {};\n",
                c_type(ty)?,
                id.0,
                c_expr(value)?
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
        IrInst::Fail(_)
        | IrInst::Panic(_)
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

fn emit_terminator(out: &mut String, terminator: &IrTerminator) -> KuResult<()> {
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
        IrExprKind::Local(name) => Ok(c_ident(name)),
        IrExprKind::Temp(id) => Ok(format!("t{}", id.0)),
        IrExprKind::Unary { op, expr } => Ok(format!("({}{})", c_unary(*op), c_expr(expr)?)),
        IrExprKind::Binary { left, op, right } => Ok(format!(
            "({} {} {})",
            c_expr(left)?,
            c_binary(*op),
            c_expr(right)?
        )),
        IrExprKind::Call { callee, args, .. } => {
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
        IrType::Void => Ok("void"),
        _ => Err(unsupported(format!(
            "native C prototype does not support type {ty}"
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

fn unsupported(message: impl Into<String>) -> KuError {
    KuError::runtime(message.into(), Span::default())
}

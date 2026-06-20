use std::collections::{BTreeMap, HashMap};

use crate::{
    ast::{BinaryOp, UnaryOp},
    error::{KuError, KuResult},
    ir::{
        BlockId, FunctionId, IrCallKind, IrExpr, IrExprKind, IrFunction, IrInst, IrLValue,
        IrProgram, IrTerminator, IrType, TempId,
    },
    span::Span,
};

pub fn generate_llvm_ir(program: &IrProgram) -> KuResult<String> {
    Generator::new(program)?.generate()
}

struct Generator<'a> {
    program: &'a IrProgram,
    symbols: HashMap<FunctionId, String>,
    strings: BTreeMap<Vec<u8>, String>,
}

impl<'a> Generator<'a> {
    fn new(program: &'a IrProgram) -> KuResult<Self> {
        if !program.layouts.structs.is_empty() || !program.layouts.enums.is_empty() {
            return Err(unsupported(
                "LLVM text prototype does not support struct or enum layouts yet",
            ));
        }

        let symbols = program
            .functions
            .iter()
            .map(|function| {
                let symbol = if function.name == "main" {
                    "ku_main".to_string()
                } else {
                    format!(
                        "ku_fn{}_{}",
                        function.id.0,
                        sanitize_identifier(&function.name)
                    )
                };
                (function.id, symbol)
            })
            .collect();
        let mut generator = Self {
            program,
            symbols,
            strings: BTreeMap::new(),
        };
        generator.collect_strings()?;
        Ok(generator)
    }

    fn generate(&self) -> KuResult<String> {
        let mut out = String::from(
            "; Ku LLVM text prototype\n\
             source_filename = \"ku\"\n\n\
             declare i32 @printf(i8*, ...)\n\
             declare i32 @puts(i8*)\n\n\
             @.ku.fmt.int = private unnamed_addr constant [6 x i8] c\"%lld\\0A\\00\"\n\
             @.ku.true = private unnamed_addr constant [5 x i8] c\"true\\00\"\n\
             @.ku.false = private unnamed_addr constant [6 x i8] c\"false\\00\"\n",
        );
        for (bytes, symbol) in &self.strings {
            out.push_str(&format!(
                "@{symbol} = private unnamed_addr constant [{} x i8] c\"{}\"\n",
                bytes.len() + 1,
                llvm_bytes(bytes)
            ));
        }
        out.push('\n');

        for function in &self.program.functions {
            FunctionEmitter::new(self, function)?.emit(&mut out)?;
            out.push('\n');
        }
        self.emit_main_wrapper(&mut out)?;
        Ok(out)
    }

    fn collect_strings(&mut self) -> KuResult<()> {
        for function in &self.program.functions {
            for block in &function.blocks {
                for instruction in &block.instructions {
                    match instruction {
                        IrInst::Temp { value, .. }
                        | IrInst::BindOk { result: value, .. }
                        | IrInst::Let { value, .. }
                        | IrInst::Store { value, .. }
                        | IrInst::Print(value)
                        | IrInst::Expr(value)
                        | IrInst::Fail(value)
                        | IrInst::Panic(value) => self.collect_expr_strings(value)?,
                        IrInst::BindError { result, .. } => self.collect_expr_strings(result)?,
                        IrInst::BeginTry { .. }
                        | IrInst::EndTry
                        | IrInst::DefineClosure { .. }
                        | IrInst::Unsupported { .. } => {}
                    }
                }
                match &block.terminator {
                    IrTerminator::Branch { condition, .. } => {
                        self.collect_expr_strings(condition)?
                    }
                    IrTerminator::ForEach { iterable, .. } => {
                        self.collect_expr_strings(iterable)?
                    }
                    IrTerminator::ResultBranch { result, .. }
                    | IrTerminator::JumpErr { result, .. }
                    | IrTerminator::PropagateErr(result)
                    | IrTerminator::Return(Some(result)) => self.collect_expr_strings(result)?,
                    IrTerminator::Next
                    | IrTerminator::Jump(_)
                    | IrTerminator::Return(None)
                    | IrTerminator::Unreachable => {}
                }
            }
        }
        Ok(())
    }

    fn collect_expr_strings(&mut self, expr: &IrExpr) -> KuResult<()> {
        match &expr.kind {
            IrExprKind::Literal(value) if expr.ty == IrType::Str => {
                let bytes = decode_string_literal(value)?;
                let next = self.strings.len();
                self.strings
                    .entry(bytes)
                    .or_insert_with(|| format!(".ku.str.{next}"));
            }
            IrExprKind::Unary { expr, .. } | IrExprKind::TryUnwrap(expr) => {
                self.collect_expr_strings(expr)?
            }
            IrExprKind::Binary { left, right, .. } => {
                self.collect_expr_strings(left)?;
                self.collect_expr_strings(right)?;
            }
            IrExprKind::Call { callee, args, .. } => {
                self.collect_expr_strings(callee)?;
                for arg in args {
                    self.collect_expr_strings(arg)?;
                }
            }
            IrExprKind::Array(values) => {
                for value in values {
                    self.collect_expr_strings(value)?;
                }
            }
            IrExprKind::Index { target, index } => {
                self.collect_expr_strings(target)?;
                self.collect_expr_strings(index)?;
            }
            IrExprKind::Field { target, .. } => self.collect_expr_strings(target)?,
            IrExprKind::StructLiteral { fields, .. } => {
                for (_, value) in fields {
                    self.collect_expr_strings(value)?;
                }
            }
            IrExprKind::Literal(_) | IrExprKind::Local(_) | IrExprKind::Temp(_) => {}
        }
        Ok(())
    }

    fn emit_main_wrapper(&self, out: &mut String) -> KuResult<()> {
        let Some(main) = self
            .program
            .functions
            .iter()
            .find(|function| function.name == "main")
        else {
            return Ok(());
        };
        if !main.params.is_empty() {
            return Err(unsupported(
                "LLVM text prototype does not support main parameters",
            ));
        }

        out.push_str("define i32 @main() {\nentry:\n");
        match main.return_type {
            IrType::Void => out.push_str("  call void @ku_main()\n  ret i32 0\n"),
            IrType::Int => {
                out.push_str("  %result = call i64 @ku_main()\n");
                out.push_str("  %exit = trunc i64 %result to i32\n  ret i32 %exit\n");
            }
            IrType::Bool => {
                out.push_str("  %result = call i1 @ku_main()\n");
                out.push_str("  %exit = select i1 %result, i32 0, i32 1\n  ret i32 %exit\n");
            }
            IrType::Str => {
                out.push_str("  %result = call i8* @ku_main()\n");
                out.push_str("  call i32 @puts(i8* %result)\n  ret i32 0\n");
            }
            ref ty => {
                return Err(unsupported(format!(
                    "LLVM text prototype does not support main return type {ty}"
                )))
            }
        }
        out.push_str("}\n");
        Ok(())
    }

    fn function_symbol(&self, id: FunctionId) -> KuResult<&str> {
        self.symbols.get(&id).map(String::as_str).ok_or_else(|| {
            unsupported(format!(
                "LLVM text prototype cannot find function #{}",
                id.0
            ))
        })
    }

    fn string_operand(&self, value: &str) -> KuResult<Operand> {
        let bytes = decode_string_literal(value)?;
        let symbol = self
            .strings
            .get(&bytes)
            .ok_or_else(|| unsupported("LLVM text prototype lost a string literal"))?;
        Ok(Operand {
            ty: IrType::Str,
            text: format!(
                "getelementptr inbounds ([{} x i8], [{} x i8]* @{}, i64 0, i64 0)",
                bytes.len() + 1,
                bytes.len() + 1,
                symbol
            ),
        })
    }
}

struct FunctionEmitter<'a> {
    generator: &'a Generator<'a>,
    function: &'a IrFunction,
    locals: HashMap<String, IrType>,
    temps: HashMap<TempId, Operand>,
    next_value: usize,
}

impl<'a> FunctionEmitter<'a> {
    fn new(generator: &'a Generator<'a>, function: &'a IrFunction) -> KuResult<Self> {
        llvm_type(&function.return_type)?;
        let mut locals = HashMap::new();
        for param in &function.params {
            ensure_local_type(&param.ty)?;
            locals.insert(param.name.clone(), param.ty.clone());
        }
        for block in &function.blocks {
            for instruction in &block.instructions {
                if let IrInst::Let { name, ty, .. } = instruction {
                    ensure_local_type(ty)?;
                    match locals.get(name) {
                        Some(existing) if existing != ty => {
                            return Err(unsupported(format!(
                                "LLVM text prototype found conflicting types for local '{name}'"
                            )))
                        }
                        _ => {
                            locals.insert(name.clone(), ty.clone());
                        }
                    }
                }
            }
        }
        Ok(Self {
            generator,
            function,
            locals,
            temps: HashMap::new(),
            next_value: 0,
        })
    }

    fn emit(mut self, out: &mut String) -> KuResult<()> {
        let return_type = llvm_type(&self.function.return_type)?;
        let symbol = self.generator.function_symbol(self.function.id)?;
        out.push_str(&format!("define {return_type} @{symbol}("));
        for (index, param) in self.function.params.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!(
                "{} %arg.{}",
                llvm_type(&param.ty)?,
                sanitize_identifier(&param.name)
            ));
        }
        out.push_str(") {\n");

        let first_block =
            self.function.blocks.first().ok_or_else(|| {
                unsupported("LLVM text prototype found a function without blocks")
            })?;
        out.push_str("entry:\n");
        let mut locals = self.locals.iter().collect::<Vec<_>>();
        locals.sort_by(|left, right| left.0.cmp(right.0));
        for (name, ty) in locals {
            out.push_str(&format!(
                "  %local.{} = alloca {}\n",
                sanitize_identifier(name),
                llvm_type(ty)?
            ));
        }
        for param in &self.function.params {
            out.push_str(&format!(
                "  store {} %arg.{}, {}* %local.{}\n",
                llvm_type(&param.ty)?,
                sanitize_identifier(&param.name),
                llvm_type(&param.ty)?,
                sanitize_identifier(&param.name)
            ));
        }
        out.push_str(&format!("  br label %{}\n", block_label(first_block.id)));

        for (index, block) in self.function.blocks.iter().enumerate() {
            out.push_str(&format!("{}:\n", block_label(block.id)));
            for instruction in &block.instructions {
                self.emit_instruction(out, instruction)?;
            }
            let next = self.function.blocks.get(index + 1).map(|block| block.id);
            self.emit_terminator(out, &block.terminator, next)?;
        }
        out.push_str("}\n");
        Ok(())
    }

    fn emit_instruction(&mut self, out: &mut String, instruction: &IrInst) -> KuResult<()> {
        match instruction {
            IrInst::Temp { id, ty, value } => {
                if ty != &value.ty {
                    return Err(unsupported(format!(
                        "LLVM text prototype found mismatched type for temporary %t{}",
                        id.0
                    )));
                }
                let value = self.emit_expr(out, value)?;
                self.temps.insert(*id, value);
            }
            IrInst::Let { name, ty, value } => {
                let value = self.emit_expr(out, value)?;
                ensure_same_type(ty, &value.ty, "local initialization")?;
                self.emit_store(out, name, &value)?;
            }
            IrInst::Store { target, value } => {
                let IrLValue::Local(name) = target else {
                    return Err(unsupported(
                        "LLVM text prototype only supports local assignment",
                    ));
                };
                let value = self.emit_expr(out, value)?;
                let ty = self
                    .locals
                    .get(name)
                    .ok_or_else(|| unsupported(format!("unknown LLVM local '{name}'")))?;
                ensure_same_type(ty, &value.ty, "local assignment")?;
                self.emit_store(out, name, &value)?;
            }
            IrInst::Print(value) => {
                let value = self.emit_expr(out, value)?;
                self.emit_print(out, &value)?;
            }
            IrInst::Expr(value) => {
                self.emit_expr(out, value)?;
            }
            IrInst::BindOk { .. }
            | IrInst::Fail(_)
            | IrInst::Panic(_)
            | IrInst::BeginTry { .. }
            | IrInst::EndTry
            | IrInst::BindError { .. }
            | IrInst::DefineClosure { .. }
            | IrInst::Unsupported { .. } => {
                return Err(unsupported(format!(
                    "LLVM text prototype cannot lower IR instruction '{instruction}'"
                )))
            }
        }
        Ok(())
    }

    fn emit_store(&self, out: &mut String, name: &str, value: &Operand) -> KuResult<()> {
        let ty = llvm_type(&value.ty)?;
        out.push_str(&format!(
            "  store {ty} {}, {ty}* %local.{}\n",
            value.text,
            sanitize_identifier(name)
        ));
        Ok(())
    }

    fn emit_print(&mut self, out: &mut String, value: &Operand) -> KuResult<()> {
        match value.ty {
            IrType::Int => {
                let call = self.fresh_value();
                out.push_str(&format!(
                    "  {call} = call i32 (i8*, ...) @printf(i8* getelementptr inbounds ([6 x i8], [6 x i8]* @.ku.fmt.int, i64 0, i64 0), i64 {})\n",
                    value.text
                ));
            }
            IrType::Bool => {
                let selected = self.fresh_value();
                out.push_str(&format!(
                    "  {selected} = select i1 {}, i8* getelementptr inbounds ([5 x i8], [5 x i8]* @.ku.true, i64 0, i64 0), i8* getelementptr inbounds ([6 x i8], [6 x i8]* @.ku.false, i64 0, i64 0)\n",
                    value.text
                ));
                let call = self.fresh_value();
                out.push_str(&format!("  {call} = call i32 @puts(i8* {selected})\n"));
            }
            IrType::Str => {
                let call = self.fresh_value();
                out.push_str(&format!("  {call} = call i32 @puts(i8* {})\n", value.text));
            }
            _ => {
                return Err(unsupported(
                    "LLVM text prototype print supports int/bool/str",
                ))
            }
        }
        Ok(())
    }

    fn emit_terminator(
        &mut self,
        out: &mut String,
        terminator: &IrTerminator,
        next: Option<BlockId>,
    ) -> KuResult<()> {
        match terminator {
            IrTerminator::Next => {
                if let Some(next) = next {
                    out.push_str(&format!("  br label %{}\n", block_label(next)));
                } else if self.function.return_type == IrType::Void {
                    out.push_str("  ret void\n");
                } else {
                    return Err(unsupported(format!(
                        "LLVM text prototype function '{}' can reach its end without returning {}",
                        self.function.name, self.function.return_type
                    )));
                }
            }
            IrTerminator::Jump(target) => {
                out.push_str(&format!("  br label %{}\n", block_label(*target)));
            }
            IrTerminator::Branch {
                condition,
                then_block,
                else_block,
            } => {
                let condition = self.emit_expr(out, condition)?;
                ensure_same_type(&IrType::Bool, &condition.ty, "branch condition")?;
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    condition.text,
                    block_label(*then_block),
                    block_label(*else_block)
                ));
            }
            IrTerminator::Return(value) => match value {
                Some(value) => {
                    let value = self.emit_expr(out, value)?;
                    ensure_same_type(&self.function.return_type, &value.ty, "function return")?;
                    out.push_str(&format!("  ret {} {}\n", llvm_type(&value.ty)?, value.text));
                }
                None if self.function.return_type == IrType::Void => out.push_str("  ret void\n"),
                None => {
                    return Err(unsupported(format!(
                        "LLVM text prototype function '{}' must return {}",
                        self.function.name, self.function.return_type
                    )))
                }
            },
            IrTerminator::Unreachable => out.push_str("  unreachable\n"),
            IrTerminator::ForEach { .. }
            | IrTerminator::ResultBranch { .. }
            | IrTerminator::JumpErr { .. }
            | IrTerminator::PropagateErr(_) => {
                return Err(unsupported(format!(
                    "LLVM text prototype cannot lower terminator '{terminator}'"
                )))
            }
        }
        Ok(())
    }

    fn emit_expr(&mut self, out: &mut String, expr: &IrExpr) -> KuResult<Operand> {
        match &expr.kind {
            IrExprKind::Literal(value) => self.literal_operand(value, &expr.ty),
            IrExprKind::Local(name) => {
                let ty = self
                    .locals
                    .get(name)
                    .cloned()
                    .ok_or_else(|| unsupported(format!("unknown LLVM local '{name}'")))?;
                let register = self.fresh_value();
                let llvm_ty = llvm_type(&ty)?;
                out.push_str(&format!(
                    "  {register} = load {llvm_ty}, {llvm_ty}* %local.{}\n",
                    sanitize_identifier(name)
                ));
                Ok(Operand { ty, text: register })
            }
            IrExprKind::Temp(id) => self
                .temps
                .get(id)
                .cloned()
                .ok_or_else(|| unsupported(format!("unknown LLVM temporary %t{}", id.0))),
            IrExprKind::Unary { op, expr } => {
                let value = self.emit_expr(out, expr)?;
                self.emit_unary(out, *op, value)
            }
            IrExprKind::Binary { left, op, right } => {
                let left = self.emit_expr(out, left)?;
                let right = self.emit_expr(out, right)?;
                self.emit_binary(out, left, *op, right)
            }
            IrExprKind::Call {
                args,
                kind: IrCallKind::Direct(id),
                ..
            } => {
                let target = self
                    .generator
                    .program
                    .functions
                    .iter()
                    .find(|function| function.id == *id)
                    .ok_or_else(|| unsupported(format!("unknown direct function #{}", id.0)))?;
                if target.params.len() != args.len() {
                    return Err(unsupported(format!(
                        "LLVM direct call to '{}' has the wrong argument count",
                        target.name
                    )));
                }
                let mut lowered = Vec::with_capacity(args.len());
                for (arg, param) in args.iter().zip(&target.params) {
                    let arg = self.emit_expr(out, arg)?;
                    ensure_same_type(&param.ty, &arg.ty, "direct call argument")?;
                    lowered.push(format!("{} {}", llvm_type(&arg.ty)?, arg.text));
                }
                let return_ty = llvm_type(&target.return_type)?;
                let call = format!(
                    "call {return_ty} @{}({})",
                    self.generator.function_symbol(*id)?,
                    lowered.join(", ")
                );
                if target.return_type == IrType::Void {
                    out.push_str(&format!("  {call}\n"));
                    Ok(Operand {
                        ty: IrType::Void,
                        text: String::new(),
                    })
                } else {
                    let register = self.fresh_value();
                    out.push_str(&format!("  {register} = {call}\n"));
                    Ok(Operand {
                        ty: target.return_type.clone(),
                        text: register,
                    })
                }
            }
            IrExprKind::Call { kind, .. } => Err(unsupported(format!(
                "LLVM text prototype only supports direct function calls, got {kind:?}"
            ))),
            IrExprKind::Array(_)
            | IrExprKind::StructLiteral { .. }
            | IrExprKind::Index { .. }
            | IrExprKind::Field { .. }
            | IrExprKind::TryUnwrap(_) => Err(unsupported(format!(
                "LLVM text prototype cannot lower expression '{expr}'"
            ))),
        }
    }

    fn literal_operand(&self, value: &str, ty: &IrType) -> KuResult<Operand> {
        let text = match ty {
            IrType::Int => value
                .parse::<i64>()
                .map_err(|_| unsupported(format!("invalid LLVM int literal '{value}'")))?
                .to_string(),
            IrType::Bool if value == "true" => "1".to_string(),
            IrType::Bool if value == "false" => "0".to_string(),
            IrType::Bool => {
                return Err(unsupported(format!("invalid LLVM bool literal '{value}'")))
            }
            IrType::Str => return self.generator.string_operand(value),
            other => {
                return Err(unsupported(format!(
                    "LLVM text prototype does not support {other} literals"
                )))
            }
        };
        Ok(Operand {
            ty: ty.clone(),
            text,
        })
    }

    fn emit_unary(&mut self, out: &mut String, op: UnaryOp, value: Operand) -> KuResult<Operand> {
        let register = self.fresh_value();
        match (op, &value.ty) {
            (UnaryOp::Negate, IrType::Int) => {
                out.push_str(&format!("  {register} = sub i64 0, {}\n", value.text));
                Ok(Operand {
                    ty: IrType::Int,
                    text: register,
                })
            }
            (UnaryOp::Not, IrType::Bool) => {
                out.push_str(&format!("  {register} = xor i1 {}, true\n", value.text));
                Ok(Operand {
                    ty: IrType::Bool,
                    text: register,
                })
            }
            _ => Err(unsupported(format!(
                "LLVM text prototype cannot apply {op:?} to {}",
                value.ty
            ))),
        }
    }

    fn emit_binary(
        &mut self,
        out: &mut String,
        left: Operand,
        op: BinaryOp,
        right: Operand,
    ) -> KuResult<Operand> {
        ensure_same_type(&left.ty, &right.ty, "binary expression")?;
        let (instruction, result_ty) = match (&left.ty, op) {
            (IrType::Int, BinaryOp::Add) => ("add i64", IrType::Int),
            (IrType::Int, BinaryOp::Subtract) => ("sub i64", IrType::Int),
            (IrType::Int, BinaryOp::Multiply) => ("mul i64", IrType::Int),
            (IrType::Int, BinaryOp::Divide) => ("sdiv i64", IrType::Int),
            (IrType::Int, BinaryOp::Remainder) => ("srem i64", IrType::Int),
            (IrType::Int, BinaryOp::Equal) => ("icmp eq i64", IrType::Bool),
            (IrType::Int, BinaryOp::NotEqual) => ("icmp ne i64", IrType::Bool),
            (IrType::Int, BinaryOp::Less) => ("icmp slt i64", IrType::Bool),
            (IrType::Int, BinaryOp::LessEqual) => ("icmp sle i64", IrType::Bool),
            (IrType::Int, BinaryOp::Greater) => ("icmp sgt i64", IrType::Bool),
            (IrType::Int, BinaryOp::GreaterEqual) => ("icmp sge i64", IrType::Bool),
            (IrType::Bool, BinaryOp::And) => ("and i1", IrType::Bool),
            (IrType::Bool, BinaryOp::Or) => ("or i1", IrType::Bool),
            (IrType::Bool, BinaryOp::Equal) => ("icmp eq i1", IrType::Bool),
            (IrType::Bool, BinaryOp::NotEqual) => ("icmp ne i1", IrType::Bool),
            _ => {
                return Err(unsupported(format!(
                    "LLVM text prototype cannot lower binary {op:?} for {}",
                    left.ty
                )))
            }
        };
        let register = self.fresh_value();
        out.push_str(&format!(
            "  {register} = {instruction} {}, {}\n",
            left.text, right.text
        ));
        Ok(Operand {
            ty: result_ty,
            text: register,
        })
    }

    fn fresh_value(&mut self) -> String {
        let value = format!("%v{}", self.next_value);
        self.next_value += 1;
        value
    }
}

#[derive(Clone)]
struct Operand {
    ty: IrType,
    text: String,
}

fn llvm_type(ty: &IrType) -> KuResult<&'static str> {
    match ty {
        IrType::Int => Ok("i64"),
        IrType::Bool => Ok("i1"),
        IrType::Str => Ok("i8*"),
        IrType::Void => Ok("void"),
        _ => Err(unsupported(format!(
            "LLVM text prototype does not support type {ty}"
        ))),
    }
}

fn ensure_local_type(ty: &IrType) -> KuResult<()> {
    if *ty == IrType::Void {
        return Err(unsupported("LLVM text prototype cannot store a void local"));
    }
    llvm_type(ty).map(|_| ())
}

fn ensure_same_type(expected: &IrType, actual: &IrType, context: &str) -> KuResult<()> {
    if expected == actual {
        Ok(())
    } else {
        Err(unsupported(format!(
            "LLVM text prototype {context} expected {expected}, got {actual}"
        )))
    }
}

fn block_label(id: BlockId) -> String {
    format!("b{}", id.0)
}

fn sanitize_identifier(name: &str) -> String {
    let mut output = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            output.push(ch);
        } else {
            output.push_str(&format!("_u{:x}_", ch as u32));
        }
    }
    if output.is_empty() {
        output.push_str("unnamed");
    }
    output
}

fn decode_string_literal(value: &str) -> KuResult<Vec<u8>> {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return Err(unsupported(format!(
            "invalid LLVM string literal representation '{value}'"
        )));
    };
    let mut chars = inner.chars().peekable();
    let mut decoded = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }
        let escaped = chars
            .next()
            .ok_or_else(|| unsupported("unterminated LLVM string escape"))?;
        match escaped {
            '\\' => decoded.push('\\'),
            '"' => decoded.push('"'),
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            '0' => decoded.push('\0'),
            'u' => {
                if chars.next() != Some('{') {
                    return Err(unsupported("invalid LLVM unicode escape"));
                }
                let mut digits = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(ch) if ch.is_ascii_hexdigit() => digits.push(ch),
                        _ => return Err(unsupported("invalid LLVM unicode escape")),
                    }
                }
                let code = u32::from_str_radix(&digits, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| unsupported("invalid LLVM unicode scalar"))?;
                decoded.push(code);
            }
            other => {
                return Err(unsupported(format!(
                    "unsupported LLVM string escape '\\{other}'"
                )))
            }
        }
    }
    Ok(decoded.into_bytes())
}

fn llvm_bytes(bytes: &[u8]) -> String {
    let mut output = String::new();
    for byte in bytes.iter().copied().chain(std::iter::once(0)) {
        if byte.is_ascii_graphic() && byte != b'"' && byte != b'\\' {
            output.push(byte as char);
        } else {
            output.push_str(&format!("\\{byte:02X}"));
        }
    }
    output
}

fn unsupported(message: impl Into<String>) -> KuError {
    KuError::runtime(message, Span::default())
}

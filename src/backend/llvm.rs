use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::{
    ast::{BinaryOp, UnaryOp},
    error::{KuError, KuResult},
    ir::{
        BlockId, FunctionId, IrCallKind, IrExpr, IrExprKind, IrFunction, IrInst, IrLValue,
        IrProgram, IrStructLayout, IrTerminator, IrType, TempId,
    },
    span::Span,
};

pub fn generate_llvm_ir(program: &IrProgram) -> KuResult<String> {
    Generator::new(program)?.generate()
}

struct Generator<'a> {
    program: &'a IrProgram,
    symbols: HashMap<FunctionId, String>,
    struct_symbols: HashMap<String, String>,
    struct_layouts: HashMap<String, &'a IrStructLayout>,
    strings: BTreeMap<Vec<u8>, String>,
}

impl<'a> Generator<'a> {
    fn new(program: &'a IrProgram) -> KuResult<Self> {
        if !program.layouts.enums.is_empty() {
            return Err(unsupported(
                "LLVM text prototype does not support enum layouts yet",
            ));
        }

        let struct_symbols = program
            .layouts
            .structs
            .iter()
            .enumerate()
            .map(|(index, layout)| {
                (
                    layout.name.clone(),
                    format!("ku.struct.{index}.{}", sanitize_identifier(&layout.name)),
                )
            })
            .collect::<HashMap<_, _>>();
        let struct_layouts = program
            .layouts
            .structs
            .iter()
            .map(|layout| (layout.name.clone(), layout))
            .collect::<HashMap<_, _>>();
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
            struct_symbols,
            struct_layouts,
            strings: BTreeMap::new(),
        };
        generator.validate_struct_layouts()?;
        generator.collect_strings()?;
        Ok(generator)
    }

    fn generate(&self) -> KuResult<String> {
        let mut out = String::from(
            "; Ku LLVM text prototype\n\
             source_filename = \"ku\"\n\n\
             declare i32 @printf(i8*, ...)\n\n\
             @.ku.fmt.int = private unnamed_addr constant [5 x i8] c\"%lld\\00\"\n\
             @.ku.fmt.str = private unnamed_addr constant [3 x i8] c\"%s\\00\"\n\
             @.ku.true = private unnamed_addr constant [5 x i8] c\"true\\00\"\n\
             @.ku.false = private unnamed_addr constant [6 x i8] c\"false\\00\"\n",
        );
        for layout in &self.program.layouts.structs {
            let fields = layout
                .fields
                .iter()
                .map(|field| self.llvm_type(&field.ty))
                .collect::<KuResult<Vec<_>>>()?
                .join(", ");
            out.push_str(&format!(
                "%{} = type {{ {fields} }}\n",
                self.struct_symbol(&layout.name)?
            ));
        }
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

    fn validate_struct_layouts(&self) -> KuResult<()> {
        let indexes = self
            .program
            .layouts
            .structs
            .iter()
            .enumerate()
            .map(|(index, layout)| (layout.name.as_str(), index))
            .collect::<HashMap<_, _>>();
        let mut dependency_count = vec![0usize; indexes.len()];
        let mut dependents = vec![Vec::new(); indexes.len()];

        for (index, layout) in self.program.layouts.structs.iter().enumerate() {
            let mut field_names = HashSet::new();
            for (expected_offset, field) in layout.fields.iter().enumerate() {
                if field.offset != expected_offset {
                    return Err(unsupported(format!(
                        "LLVM struct '{}.{}' has non-contiguous field offset {}; expected {expected_offset}",
                        layout.name, field.name, field.offset
                    )));
                }
                if !field_names.insert(field.name.as_str()) {
                    return Err(unsupported(format!(
                        "LLVM struct '{}' has duplicate field '{}'",
                        layout.name, field.name
                    )));
                }
                self.llvm_type(&field.ty)?;
                let mut dependencies = Vec::new();
                collect_named_dependencies(&field.ty, &mut dependencies);
                for name in dependencies {
                    let Some(&dependency) = indexes.get(name.as_str()) else {
                        return Err(unsupported(format!(
                            "LLVM struct '{}.{}' references unknown struct '{name}'",
                            layout.name, field.name
                        )));
                    };
                    dependency_count[index] += 1;
                    dependents[dependency].push(index);
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
        if visited != self.program.layouts.structs.len() {
            return Err(unsupported(
                "LLVM text prototype does not support recursive value struct layouts",
            ));
        }
        Ok(())
    }

    fn struct_symbol(&self, name: &str) -> KuResult<&str> {
        self.struct_symbols
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| unsupported(format!("LLVM cannot find struct layout '{name}'")))
    }

    fn struct_layout(&self, name: &str) -> KuResult<&IrStructLayout> {
        self.struct_layouts
            .get(name)
            .copied()
            .ok_or_else(|| unsupported(format!("LLVM cannot find struct layout '{name}'")))
    }

    fn llvm_type(&self, ty: &IrType) -> KuResult<String> {
        match ty {
            IrType::Int => Ok("i64".to_string()),
            IrType::Bool => Ok("i1".to_string()),
            IrType::Str => Ok("i8*".to_string()),
            IrType::Named(name) => Ok(format!("%{}", self.struct_symbol(name)?)),
            IrType::Result(inner) => Ok(format!(
                "{{ i1, {}, i8* }}",
                self.result_payload_type(inner)?
            )),
            IrType::Void => Ok("void".to_string()),
            _ => Err(unsupported(format!(
                "LLVM text prototype does not support type {ty}"
            ))),
        }
    }

    fn result_payload_type(&self, ty: &IrType) -> KuResult<String> {
        match ty {
            IrType::Int | IrType::Bool | IrType::Str | IrType::Named(_) => self.llvm_type(ty),
            _ => Err(unsupported(format!(
                "LLVM text prototype does not support Result<{ty}>"
            ))),
        }
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
        match &main.return_type {
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
            IrType::Result(_) => {
                let ty = self.llvm_type(&main.return_type)?;
                out.push_str(&format!("  %result = call {ty} @ku_main()\n"));
                out.push_str(&format!("  %ok = extractvalue {ty} %result, 0\n"));
                out.push_str("  br i1 %ok, label %result.ok, label %result.err\n");
                out.push_str("result.ok:\n  ret i32 0\n");
                out.push_str("result.err:\n");
                out.push_str(&format!("  %error = extractvalue {ty} %result, 2\n"));
                out.push_str("  %has.error = icmp ne i8* %error, null\n");
                out.push_str("  br i1 %has.error, label %result.print, label %result.exit\n");
                out.push_str("result.print:\n  call i32 @puts(i8* %error)\n");
                out.push_str("  br label %result.exit\n");
                out.push_str("result.exit:\n  ret i32 1\n");
            }
            ty => {
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
        validate_cfg(function)?;
        generator.llvm_type(&function.return_type)?;
        let mut locals = HashMap::new();
        for param in &function.params {
            ensure_local_type(generator, &param.ty)?;
            locals.insert(param.name.clone(), param.ty.clone());
        }
        for block in &function.blocks {
            for instruction in &block.instructions {
                let local = match instruction {
                    IrInst::Let { name, ty, .. } => Some((name, ty.clone())),
                    IrInst::BindError { name, .. } => Some((name, IrType::Str)),
                    _ => None,
                };
                if let Some((name, ty)) = local {
                    ensure_local_type(generator, &ty)?;
                    if let Some(existing) = locals.get(name) {
                        if existing != &ty {
                            return Err(unsupported(format!(
                                "LLVM text prototype found conflicting types for local '{name}'"
                            )));
                        }
                    } else {
                        locals.insert(name.clone(), ty);
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
        let return_type = self.generator.llvm_type(&self.function.return_type)?;
        let symbol = self.generator.function_symbol(self.function.id)?;
        out.push_str(&format!("define {return_type} @{symbol}("));
        for (index, param) in self.function.params.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!(
                "{} %arg.{}",
                self.generator.llvm_type(&param.ty)?,
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
                self.generator.llvm_type(ty)?
            ));
        }
        for param in &self.function.params {
            let ty = self.generator.llvm_type(&param.ty)?;
            out.push_str(&format!(
                "  store {} %arg.{}, {}* %local.{}\n",
                ty,
                sanitize_identifier(&param.name),
                ty,
                sanitize_identifier(&param.name)
            ));
        }
        out.push_str(&format!("  br label %{}\n", block_label(first_block.id)));

        for (index, block) in self.function.blocks.iter().enumerate() {
            out.push_str(&format!("{}:\n", block_label(block.id)));
            let mut terminated = false;
            for instruction in &block.instructions {
                if terminated {
                    return Err(unsupported(format!(
                        "LLVM block '{}' has instructions after a terminating instruction",
                        block.name
                    )));
                }
                terminated = self.emit_instruction(out, instruction)?;
            }
            if !terminated {
                let next = self.function.blocks.get(index + 1).map(|block| block.id);
                self.emit_terminator(out, &block.terminator, next)?;
            } else if block.terminator != IrTerminator::Unreachable {
                return Err(unsupported(format!(
                    "LLVM block '{}' terminates before IR terminator '{}'",
                    block.name, block.terminator
                )));
            }
        }
        out.push_str("}\n");
        Ok(())
    }

    fn emit_instruction(&mut self, out: &mut String, instruction: &IrInst) -> KuResult<bool> {
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
                let value = self.emit_expr(out, value)?;
                let (target_ty, pointer) = self.emit_lvalue_pointer(out, target)?;
                ensure_same_type(&target_ty, &value.ty, "assignment")?;
                let ty = self.generator.llvm_type(&target_ty)?;
                out.push_str(&format!("  store {ty} {}, {ty}* {pointer}\n", value.text));
            }
            IrInst::Print(value) => {
                let value = self.emit_expr(out, value)?;
                self.emit_print(out, &value)?;
            }
            IrInst::Expr(value) => {
                self.emit_expr(out, value)?;
            }
            IrInst::BindOk { id, ty, result } => {
                let result = self.emit_expr(out, result)?;
                let IrType::Result(inner) = &result.ty else {
                    return Err(unsupported("LLVM BindOk requires a Result value"));
                };
                ensure_same_type(ty, inner, "Result ok binding")?;
                let result_ty = self.generator.llvm_type(&result.ty)?;
                let register = self.fresh_value();
                out.push_str(&format!(
                    "  {register} = extractvalue {result_ty} {}, 1\n",
                    result.text
                ));
                self.temps.insert(
                    *id,
                    Operand {
                        ty: ty.clone(),
                        text: register,
                    },
                );
            }
            IrInst::Fail(error) => {
                let IrType::Result(inner) = &self.function.return_type else {
                    return Err(unsupported("LLVM fail requires a Result return type"));
                };
                let error = self.emit_expr(out, error)?;
                ensure_same_type(&IrType::Str, &error.ty, "fail error")?;
                let result = self.emit_result_value(out, inner, false, None, Some(error))?;
                let ty = self.generator.llvm_type(&self.function.return_type)?;
                out.push_str(&format!("  ret {ty} {}\n", result.text));
                return Ok(true);
            }
            IrInst::BeginTry { .. } | IrInst::EndTry => {}
            IrInst::BindError { name, result } => {
                let result = self.emit_expr(out, result)?;
                if !matches!(result.ty, IrType::Result(_)) {
                    return Err(unsupported("LLVM BindError requires a Result value"));
                }
                let result_ty = self.generator.llvm_type(&result.ty)?;
                let register = self.fresh_value();
                out.push_str(&format!(
                    "  {register} = extractvalue {result_ty} {}, 2\n",
                    result.text
                ));
                self.emit_store(
                    out,
                    name,
                    &Operand {
                        ty: IrType::Str,
                        text: register,
                    },
                )?;
            }
            IrInst::Panic(_) | IrInst::DefineClosure { .. } | IrInst::Unsupported { .. } => {
                return Err(unsupported(format!(
                    "LLVM text prototype cannot lower IR instruction '{instruction}'"
                )))
            }
        }
        Ok(false)
    }

    fn emit_store(&self, out: &mut String, name: &str, value: &Operand) -> KuResult<()> {
        let expected = self
            .locals
            .get(name)
            .ok_or_else(|| unsupported(format!("unknown LLVM local '{name}'")))?;
        ensure_same_type(expected, &value.ty, "local store")?;
        let ty = self.generator.llvm_type(&value.ty)?;
        out.push_str(&format!(
            "  store {ty} {}, {ty}* %local.{}\n",
            value.text,
            sanitize_identifier(name)
        ));
        Ok(())
    }

    fn emit_lvalue_pointer(
        &mut self,
        out: &mut String,
        target: &IrLValue,
    ) -> KuResult<(IrType, String)> {
        match target {
            IrLValue::Local(name) => {
                let ty = self
                    .locals
                    .get(name)
                    .cloned()
                    .ok_or_else(|| unsupported(format!("unknown LLVM local '{name}'")))?;
                Ok((ty, format!("%local.{}", sanitize_identifier(name))))
            }
            IrLValue::Field { target, name } => {
                let (container_ty, pointer) = self.emit_expr_pointer(out, target)?;
                let IrType::Named(struct_name) = &container_ty else {
                    return Err(unsupported(format!(
                        "LLVM field assignment requires a struct target, got {container_ty}"
                    )));
                };
                let (index, field_ty) = self.struct_field(struct_name, name)?;
                let struct_ty = self.generator.llvm_type(&container_ty)?;
                let field_pointer = self.fresh_value();
                out.push_str(&format!(
                    "  {field_pointer} = getelementptr inbounds {struct_ty}, {struct_ty}* {pointer}, i32 0, i32 {index}\n"
                ));
                Ok((field_ty, field_pointer))
            }
            IrLValue::Index { .. } => Err(unsupported(
                "LLVM text prototype does not support array/index assignment",
            )),
        }
    }

    fn emit_expr_pointer(&mut self, out: &mut String, expr: &IrExpr) -> KuResult<(IrType, String)> {
        match &expr.kind {
            IrExprKind::Local(name) => {
                let ty = self
                    .locals
                    .get(name)
                    .cloned()
                    .ok_or_else(|| unsupported(format!("unknown LLVM local '{name}'")))?;
                Ok((ty, format!("%local.{}", sanitize_identifier(name))))
            }
            IrExprKind::Field { target, name } => {
                let (container_ty, pointer) = self.emit_expr_pointer(out, target)?;
                let IrType::Named(struct_name) = &container_ty else {
                    return Err(unsupported(format!(
                        "LLVM nested field assignment requires a struct target, got {container_ty}"
                    )));
                };
                let (index, field_ty) = self.struct_field(struct_name, name)?;
                let struct_ty = self.generator.llvm_type(&container_ty)?;
                let field_pointer = self.fresh_value();
                out.push_str(&format!(
                    "  {field_pointer} = getelementptr inbounds {struct_ty}, {struct_ty}* {pointer}, i32 0, i32 {index}\n"
                ));
                Ok((field_ty, field_pointer))
            }
            _ => Err(unsupported(
                "LLVM field assignment target must be rooted in a local struct",
            )),
        }
    }

    fn emit_print(&mut self, out: &mut String, value: &Operand) -> KuResult<()> {
        match value.ty {
            IrType::Int => {
                let call = self.fresh_value();
                out.push_str(&format!(
                    "  {call} = call i32 (i8*, ...) @printf(i8* getelementptr inbounds ([5 x i8], [5 x i8]* @.ku.fmt.int, i64 0, i64 0), i64 {})\n",
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
                out.push_str(&format!("  {call} = call i32 (i8*, ...) @printf(i8* getelementptr inbounds ([3 x i8], [3 x i8]* @.ku.fmt.str, i64 0, i64 0), i8* {selected})\n"));
            }
            IrType::Str => {
                let call = self.fresh_value();
                out.push_str(&format!("  {call} = call i32 (i8*, ...) @printf(i8* getelementptr inbounds ([3 x i8], [3 x i8]* @.ku.fmt.str, i64 0, i64 0), i8* {})\n", value.text));
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
                    out.push_str(&format!(
                        "  ret {} {}\n",
                        self.generator.llvm_type(&value.ty)?,
                        value.text
                    ));
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
            IrTerminator::ResultBranch {
                result,
                ok_block,
                err_block,
            } => {
                let result = self.emit_expr(out, result)?;
                if !matches!(result.ty, IrType::Result(_)) {
                    return Err(unsupported("LLVM ResultBranch requires a Result value"));
                }
                let ty = self.generator.llvm_type(&result.ty)?;
                let ok = self.fresh_value();
                out.push_str(&format!("  {ok} = extractvalue {ty} {}, 0\n", result.text));
                out.push_str(&format!(
                    "  br i1 {ok}, label %{}, label %{}\n",
                    block_label(*ok_block),
                    block_label(*err_block)
                ));
            }
            IrTerminator::JumpErr { result, target } => {
                let result = self.emit_expr(out, result)?;
                if !matches!(result.ty, IrType::Result(_)) {
                    return Err(unsupported("LLVM JumpErr requires a Result value"));
                }
                out.push_str(&format!("  br label %{}\n", block_label(*target)));
            }
            IrTerminator::PropagateErr(value) => {
                let value = self.emit_expr(out, value)?;
                if !matches!(self.function.return_type, IrType::Result(_)) {
                    return Err(unsupported(
                        "LLVM can only propagate errors from Result functions",
                    ));
                }
                ensure_same_type(&self.function.return_type, &value.ty, "Result propagation")?;
                out.push_str(&format!(
                    "  ret {} {}\n",
                    self.generator.llvm_type(&value.ty)?,
                    value.text
                ));
            }
            IrTerminator::ForEach { .. } => {
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
                let llvm_ty = self.generator.llvm_type(&ty)?;
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
                    lowered.push(format!(
                        "{} {}",
                        self.generator.llvm_type(&arg.ty)?,
                        arg.text
                    ));
                }
                let return_ty = self.generator.llvm_type(&target.return_type)?;
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
            IrExprKind::Call {
                args,
                kind: IrCallKind::Intrinsic(name),
                ..
            } => self.emit_intrinsic(out, name, args, &expr.ty),
            IrExprKind::Call { kind, .. } => Err(unsupported(format!(
                "LLVM text prototype only supports direct function calls, got {kind:?}"
            ))),
            IrExprKind::Array(_) | IrExprKind::Index { .. } | IrExprKind::TryUnwrap(_) => {
                Err(unsupported(format!(
                    "LLVM text prototype cannot lower expression '{expr}'"
                )))
            }
            IrExprKind::StructLiteral { name, fields } => {
                self.emit_struct_literal(out, name, fields, &expr.ty)
            }
            IrExprKind::Field { target, name } => {
                let target = self.emit_expr(out, target)?;
                let IrType::Named(struct_name) = &target.ty else {
                    return Err(unsupported(format!(
                        "LLVM field access requires a struct target, got {}",
                        target.ty
                    )));
                };
                let (index, field_ty) = self.struct_field(struct_name, name)?;
                let struct_ty = self.generator.llvm_type(&target.ty)?;
                let register = self.fresh_value();
                out.push_str(&format!(
                    "  {register} = extractvalue {struct_ty} {}, {index}\n",
                    target.text
                ));
                ensure_same_type(&expr.ty, &field_ty, "field expression")?;
                Ok(Operand {
                    ty: field_ty,
                    text: register,
                })
            }
        }
    }

    fn emit_intrinsic(
        &mut self,
        out: &mut String,
        name: &str,
        args: &[IrExpr],
        result_ty: &IrType,
    ) -> KuResult<Operand> {
        let IrType::Result(inner) = result_ty else {
            return Err(unsupported(format!(
                "LLVM intrinsic '{name}' requires a Result type"
            )));
        };
        if args.len() != 1 {
            return Err(unsupported(format!(
                "LLVM intrinsic '{name}' expects one argument"
            )));
        }
        let value = self.emit_expr(out, &args[0])?;
        match name {
            "ok" => {
                ensure_same_type(inner, &value.ty, "ok payload")?;
                self.emit_result_value(out, inner, true, Some(value), None)
            }
            "err" => {
                ensure_same_type(&IrType::Str, &value.ty, "err payload")?;
                self.emit_result_value(out, inner, false, None, Some(value))
            }
            _ => Err(unsupported(format!(
                "LLVM text prototype cannot lower intrinsic '{name}'"
            ))),
        }
    }

    fn emit_result_value(
        &mut self,
        out: &mut String,
        inner: &IrType,
        ok: bool,
        value: Option<Operand>,
        error: Option<Operand>,
    ) -> KuResult<Operand> {
        let result_ty = IrType::Result(Box::new(inner.clone()));
        let llvm_ty = self.generator.llvm_type(&result_ty)?;
        let tag = self.fresh_value();
        out.push_str(&format!(
            "  {tag} = insertvalue {llvm_ty} undef, i1 {}, 0\n",
            if ok { 1 } else { 0 }
        ));

        let payload_text = match value {
            Some(value) => {
                ensure_same_type(inner, &value.ty, "Result payload")?;
                value.text
            }
            None => zero_value(inner)?,
        };
        let payload = self.fresh_value();
        out.push_str(&format!(
            "  {payload} = insertvalue {llvm_ty} {tag}, {} {payload_text}, 1\n",
            self.generator.result_payload_type(inner)?
        ));

        let error_text = match error {
            Some(error) => {
                ensure_same_type(&IrType::Str, &error.ty, "Result error")?;
                error.text
            }
            None => "null".to_string(),
        };
        let complete = self.fresh_value();
        out.push_str(&format!(
            "  {complete} = insertvalue {llvm_ty} {payload}, i8* {error_text}, 2\n"
        ));
        Ok(Operand {
            ty: result_ty,
            text: complete,
        })
    }

    fn emit_struct_literal(
        &mut self,
        out: &mut String,
        name: &str,
        fields: &[(String, IrExpr)],
        expr_ty: &IrType,
    ) -> KuResult<Operand> {
        ensure_same_type(&IrType::Named(name.to_string()), expr_ty, "struct literal")?;
        let layout = self.generator.struct_layout(name)?;
        if fields.len() != layout.fields.len() {
            return Err(unsupported(format!(
                "LLVM struct literal '{name}' expected {} fields, got {}",
                layout.fields.len(),
                fields.len()
            )));
        }
        let mut provided = HashSet::new();
        for (field, _) in fields {
            if !provided.insert(field.as_str()) {
                return Err(unsupported(format!(
                    "LLVM struct literal '{name}' repeats field '{field}'"
                )));
            }
            if !layout
                .fields
                .iter()
                .any(|layout_field| layout_field.name == *field)
            {
                return Err(unsupported(format!(
                    "LLVM struct '{name}' has no field '{field}'"
                )));
            }
        }

        let llvm_ty = self.generator.llvm_type(expr_ty)?;
        let mut aggregate = "undef".to_string();
        for (field_name, value) in fields {
            let field = layout
                .fields
                .iter()
                .find(|field| field.name == *field_name)
                .expect("validated struct field");
            let value = self.emit_expr(out, value)?;
            ensure_same_type(&field.ty, &value.ty, "struct field")?;
            let next = self.fresh_value();
            out.push_str(&format!(
                "  {next} = insertvalue {llvm_ty} {aggregate}, {} {}, {}\n",
                self.generator.llvm_type(&field.ty)?,
                value.text,
                field.offset
            ));
            aggregate = next;
        }
        Ok(Operand {
            ty: expr_ty.clone(),
            text: if layout.fields.is_empty() {
                "zeroinitializer".to_string()
            } else {
                aggregate
            },
        })
    }

    fn struct_field(&self, struct_name: &str, field_name: &str) -> KuResult<(usize, IrType)> {
        let layout = self.generator.struct_layout(struct_name)?;
        let field = layout
            .fields
            .iter()
            .find(|field| field.name == field_name)
            .ok_or_else(|| {
                unsupported(format!(
                    "LLVM struct '{struct_name}' has no field '{field_name}'"
                ))
            })?;
        Ok((field.offset, field.ty.clone()))
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

fn ensure_local_type(generator: &Generator<'_>, ty: &IrType) -> KuResult<()> {
    if *ty == IrType::Void {
        return Err(unsupported("LLVM text prototype cannot store a void local"));
    }
    generator.llvm_type(ty).map(|_| ())
}

fn zero_value(ty: &IrType) -> KuResult<String> {
    match ty {
        IrType::Int | IrType::Bool => Ok("0".to_string()),
        IrType::Str => Ok("null".to_string()),
        IrType::Named(_) => Ok("zeroinitializer".to_string()),
        _ => Err(unsupported(format!(
            "LLVM text prototype does not support zero value for {ty}"
        ))),
    }
}

fn collect_named_dependencies(ty: &IrType, output: &mut Vec<String>) {
    match ty {
        IrType::Named(name) => output.push(name.clone()),
        IrType::Result(inner) => collect_named_dependencies(inner, output),
        _ => {}
    }
}

fn validate_cfg(function: &IrFunction) -> KuResult<()> {
    let mut block_ids = HashSet::new();
    for block in &function.blocks {
        if !block_ids.insert(block.id) {
            return Err(unsupported(format!(
                "LLVM function '{}' has duplicate block id {}",
                function.name, block.id.0
            )));
        }
    }
    for block in &function.blocks {
        let mut targets = Vec::new();
        match &block.terminator {
            IrTerminator::Jump(target) => {
                if *target == block.id {
                    return Err(unsupported(format!(
                        "LLVM function '{}' block {} has an unconditional self-jump",
                        function.name, block.id.0
                    )));
                }
                targets.push(*target);
            }
            IrTerminator::Branch {
                then_block,
                else_block,
                ..
            } => {
                targets.push(*then_block);
                targets.push(*else_block);
            }
            IrTerminator::ForEach {
                body_block,
                after_block,
                ..
            } => {
                targets.push(*body_block);
                targets.push(*after_block);
            }
            IrTerminator::ResultBranch {
                ok_block,
                err_block,
                ..
            } => {
                targets.push(*ok_block);
                targets.push(*err_block);
            }
            IrTerminator::JumpErr { target, .. } => targets.push(*target),
            IrTerminator::Next
            | IrTerminator::PropagateErr(_)
            | IrTerminator::Return(_)
            | IrTerminator::Unreachable => {}
        }
        for target in targets {
            if !block_ids.contains(&target) {
                return Err(unsupported(format!(
                    "LLVM function '{}' block {} branches to missing block {}",
                    function.name, block.id.0, target.0
                )));
            }
        }
    }
    Ok(())
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

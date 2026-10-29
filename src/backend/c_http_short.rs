//! Conservative, private proof for owner-local synchronous HTTP handlers.
//! Unknown work keeps the existing worker path. This is a size/work bound,
//! never a promise about OS scheduling or allocator latency.
use std::collections::{HashMap, HashSet};

use super::{
    closure_invoke_symbol, closure_thunk_targets, expr_children, walk_inst_exprs,
    walk_terminator_exprs, COutput,
};
use crate::ir::{
    BlockId, FunctionId, IrCallKind, IrExpr, IrExprKind, IrFunction, IrInst, IrLValue, IrProgram,
    IrTerminator, IrType, TempId,
};

const MAX_NODES: usize = 192;
const MAX_STRING: usize = 256;
const MAX_BYTE_WORK: usize = 4096;
const MAX_CALLS: usize = 8;

#[derive(Clone)]
struct Bound {
    ty: IrType,
    bytes: usize,
}
impl Bound {
    fn new(ty: &IrType, bytes: usize) -> Option<Self> {
        fn admitted(ty: &IrType) -> bool {
            match ty {
                IrType::Int
                | IrType::Float
                | IrType::Bool
                | IrType::Null
                | IrType::Str
                | IrType::Void => true,
                IrType::Named(name) => name == "__ku_http_response" || name == "__ku_error_type",
                IrType::Result(inner) => admitted(inner),
                _ => false,
            }
        }
        (admitted(ty) && bytes <= MAX_STRING).then(|| Self {
            ty: ty.clone(),
            bytes,
        })
    }
}

#[derive(Default)]
struct Proof {
    nodes: usize,
    byte_work: usize,
    stack: HashSet<FunctionId>,
}
impl Proof {
    fn tick(&mut self, bytes: usize) -> Option<()> {
        self.nodes = self.nodes.checked_add(1)?;
        self.byte_work = self.byte_work.checked_add(bytes)?;
        (self.nodes <= MAX_NODES && self.byte_work <= MAX_BYTE_WORK).then_some(())
    }
    fn expression(
        &mut self,
        program: &IrProgram,
        expr: &IrExpr,
        locals: &HashMap<String, Bound>,
        temps: &HashMap<TempId, Bound>,
    ) -> Option<Bound> {
        self.tick(0)?;
        let bytes = match &expr.kind {
            IrExprKind::Literal(value) => {
                if expr.ty == IrType::Str {
                    crate::backend::llvm::decode_string_literal(value)
                        .ok()?
                        .len()
                } else {
                    0
                }
            }
            IrExprKind::Local(name) | IrExprKind::BorrowedParam(name) => {
                let bound = locals.get(name)?;
                if bound.ty != expr.ty {
                    return None;
                }
                bound.bytes
            }
            IrExprKind::Temp(id) | IrExprKind::BorrowedTemp(id) => {
                let bound = temps.get(id)?;
                if bound.ty != expr.ty {
                    return None;
                }
                bound.bytes
            }
            IrExprKind::Borrow(value) => self.expression(program, value, locals, temps)?.bytes,
            IrExprKind::Unary { expr: value, .. } => {
                if !matches!(value.ty, IrType::Int | IrType::Float | IrType::Bool) {
                    return None;
                }
                self.expression(program, value, locals, temps)?;
                0
            }
            IrExprKind::Binary { left, op, right } => {
                let a = self.expression(program, left, locals, temps)?;
                let b = self.expression(program, right, locals, temps)?;
                if a.ty == IrType::Str && b.ty == IrType::Str {
                    if *op == crate::ast::BinaryOp::Add {
                        let bytes = a.bytes.checked_add(b.bytes)?;
                        self.tick(bytes)?;
                        bytes
                    } else {
                        self.tick(a.bytes.max(b.bytes))?;
                        0
                    }
                } else if matches!(
                    a.ty,
                    IrType::Int | IrType::Float | IrType::Bool | IrType::Null
                ) && matches!(
                    b.ty,
                    IrType::Int | IrType::Float | IrType::Bool | IrType::Null
                ) {
                    0
                } else {
                    return None;
                }
            }
            IrExprKind::Call { args, kind, .. } => {
                let args = args
                    .iter()
                    .map(|arg| self.expression(program, arg, locals, temps))
                    .collect::<Option<Vec<_>>>()?;
                match kind {
                    IrCallKind::Direct(id) => {
                        let callee = program
                            .functions
                            .iter()
                            .find(|function| function.id == *id)?;
                        self.function(program, callee, &args)?.bytes
                    }
                    IrCallKind::Intrinsic(name) => match (name.as_str(), args.as_slice()) {
                        ("http.text" | "http.html" | "http.redirect", [body])
                            if body.ty == IrType::Str =>
                        {
                            body.bytes
                        }
                        ("http.text" | "http.html" | "http.redirect", [status, body])
                            if status.ty == IrType::Int && body.ty == IrType::Str =>
                        {
                            body.bytes
                        }
                        ("http.empty", []) => 0,
                        ("http.empty", [status]) if status.ty == IrType::Int => 0,
                        ("ok" | "err" | "fail", [value]) => value.bytes,
                        ("__ku_drop_borrow_temp", [value]) => {
                            self.tick(value.bytes)?;
                            0
                        }
                        ("__ku_clone", [value]) => {
                            self.tick(value.bytes)?;
                            value.bytes
                        }
                        ("str", [value]) => match value.ty {
                            IrType::Int => {
                                self.tick(32)?;
                                32
                            }
                            IrType::Bool | IrType::Null => 5,
                            IrType::Str => {
                                self.tick(value.bytes)?;
                                value.bytes
                            }
                            _ => return None,
                        },
                        _ => return None,
                    },
                    IrCallKind::Indirect => return None,
                }
            }
            // No arrays, arbitrary aggregate destructors, captured cells,
            // callbacks, indexing, JSON, I/O or unknown foreign calls.
            _ => return None,
        };
        Bound::new(&expr.ty, bytes)
    }
    fn function(
        &mut self,
        program: &IrProgram,
        function: &IrFunction,
        args: &[Bound],
    ) -> Option<Bound> {
        if function.params.len() != args.len()
            || !function.captures.is_empty()
            || self.stack.len() >= MAX_CALLS
            || !self.stack.insert(function.id)
        {
            return None;
        }
        if function.blocks.len() > MAX_NODES || !acyclic(function) {
            return None;
        }
        let mut locals = HashMap::new();
        let mut temps = HashMap::new();
        for (param, arg) in function.params.iter().zip(args) {
            if param.ty != arg.ty {
                return None;
            }
            locals.insert(param.name.clone(), arg.clone());
        }
        let mut returned = Bound::new(&function.return_type, 0)?;
        for block in &function.blocks {
            self.tick(0)?;
            for instruction in &block.instructions {
                self.tick(0)?;
                match instruction {
                    IrInst::Temp { id, ty, value } => {
                        let bound = self.expression(program, value, &locals, &temps)?;
                        if bound.ty != *ty {
                            return None;
                        }
                        let bytes = temps
                            .get(id)
                            .map_or(bound.bytes, |old: &Bound| old.bytes.max(bound.bytes));
                        temps.insert(*id, Bound::new(ty, bytes)?);
                    }
                    IrInst::Let { name, ty, value } => {
                        let bound = self.expression(program, value, &locals, &temps)?;
                        if bound.ty != *ty {
                            return None;
                        }
                        let bytes = locals
                            .get(name)
                            .map_or(bound.bytes, |old: &Bound| old.bytes.max(bound.bytes));
                        locals.insert(name.clone(), Bound::new(ty, bytes)?);
                    }
                    IrInst::Store {
                        target: IrLValue::Local(name),
                        value,
                    } => {
                        let bound = self.expression(program, value, &locals, &temps)?;
                        let previous = locals.get(name)?;
                        if bound.ty != previous.ty {
                            return None;
                        }
                        locals.insert(
                            name.clone(),
                            Bound::new(&bound.ty, bound.bytes.max(previous.bytes))?,
                        );
                    }
                    IrInst::Expr(value) => {
                        self.expression(program, value, &locals, &temps)?;
                    }
                    _ => return None,
                }
            }
            let value = match &block.terminator {
                IrTerminator::Branch { condition, .. } => Some(condition),
                IrTerminator::Return(Some(value)) => {
                    let bound = self.expression(program, value, &locals, &temps)?;
                    if bound.ty != returned.ty {
                        return None;
                    }
                    returned.bytes = returned.bytes.max(bound.bytes);
                    None
                }
                IrTerminator::Next
                | IrTerminator::Jump(_)
                | IrTerminator::Safepoint { .. }
                | IrTerminator::SyncGuard { .. }
                | IrTerminator::Return(None)
                | IrTerminator::Unreachable => None,
                _ => return None,
            };
            if let Some(value) = value {
                self.expression(program, value, &locals, &temps)?;
            }
        }
        self.stack.remove(&function.id);
        Some(returned)
    }
}

fn acyclic(function: &IrFunction) -> bool {
    fn visit(
        function: &IrFunction,
        id: BlockId,
        active: &mut HashSet<BlockId>,
        done: &mut HashSet<BlockId>,
    ) -> bool {
        if done.contains(&id) {
            return true;
        }
        if !active.insert(id) {
            return false;
        }
        let Some(position) = function.blocks.iter().position(|block| block.id == id) else {
            return false;
        };
        let edges = match &function.blocks[position].terminator {
            IrTerminator::Next => function
                .blocks
                .get(position + 1)
                .map(|block| vec![block.id])
                .unwrap_or_default(),
            IrTerminator::Jump(target) => vec![*target],
            IrTerminator::Branch {
                then_block,
                else_block,
                ..
            } => vec![*then_block, *else_block],
            IrTerminator::Safepoint {
                continue_block,
                timeout_block,
            } => vec![*continue_block, *timeout_block],
            IrTerminator::SyncGuard {
                continue_block,
                cleanup_block,
            } => vec![*continue_block, *cleanup_block],
            IrTerminator::Return(_) | IrTerminator::Unreachable => vec![],
            _ => return false,
        };
        if !edges
            .into_iter()
            .all(|next| visit(function, next, active, done))
        {
            return false;
        }
        active.remove(&id);
        done.insert(id);
        true
    }
    let mut active = HashSet::new();
    let mut done = HashSet::new();
    function
        .blocks
        .iter()
        .all(|block| visit(function, block.id, &mut active, &mut done))
}

fn proven(program: &IrProgram, function: &IrFunction) -> bool {
    // Request materialization remains on the worker path: without field-size
    // proof even a loop-free handler can copy the full configured body.
    function.params.is_empty()
        && matches!(&function.return_type,
        IrType::Named(name) if name=="__ku_http_response")
        && Proof::default().function(program, function, &[]).is_some()
        || function.params.is_empty()
            && matches!(&function.return_type,
            IrType::Result(inner) if matches!(inner.as_ref(),IrType::Named(name) if name=="__ku_http_response"))
            && Proof::default().function(program, function, &[]).is_some()
}

fn request_fields(function: &IrFunction) -> Option<u32> {
    let [parameter] = function.params.as_slice() else {
        return None;
    };
    if parameter.ty != IrType::Named("__ku_http_request".into()) {
        return None;
    }
    let name = &parameter.name;
    let is_request = |expr: &IrExpr| {
        matches!(&expr.kind,
        IrExprKind::Local(local) | IrExprKind::BorrowedParam(local) if local == name)
    };
    let mut fields = 0u32;
    let mut nodes = 0usize;
    let mut inspect = |root: &IrExpr| -> Option<()> {
        let mut pending = vec![root];
        while let Some(expr) = pending.pop() {
            nodes += 1;
            if nodes > 8192 {
                return None;
            }
            if let IrExprKind::Field { target, name } = &expr.kind {
                if is_request(target) {
                    fields |= match name.as_str() {
                        "headers" => 1,
                        "params" => 2,
                        "query" => 4,
                        "method" => 8,
                        "path" => 16,
                        "body" => 32,
                        _ => return None,
                    };
                    continue;
                }
            }
            if is_request(expr) {
                return None;
            }
            pending.extend(expr_children(expr));
        }
        Some(())
    };
    for block in &function.blocks {
        for instruction in &block.instructions {
            if let IrInst::DefineClosure { captures, .. } = instruction {
                if captures.iter().any(|capture| capture == name) {
                    return None;
                }
            }
            if let IrInst::Store { target, .. } = instruction {
                match target {
                    IrLValue::Local(local) if local == name => return None,
                    IrLValue::Field { target, .. } => inspect(target)?,
                    IrLValue::Index { target, index } => {
                        inspect(target)?;
                        inspect(index)?;
                    }
                    _ => {}
                }
            }
            let mut valid = true;
            walk_inst_exprs(instruction, &mut |expr| {
                if inspect(expr).is_none() {
                    valid = false;
                }
            });
            if !valid {
                return None;
            }
        }
        let mut valid = true;
        walk_terminator_exprs(&block.terminator, &mut |expr| {
            if inspect(expr).is_none() {
                valid = false;
            }
        });
        if !valid {
            return None;
        }
    }
    Some(fields)
}

pub(super) fn emit(out: &mut COutput, program: &IrProgram, async_names: &HashSet<String>) {
    let targets = closure_thunk_targets(program);
    out.push_str("static int ku_http_handler_is_short(void* invoke) {\n");
    for function in &program.functions {
        if (!function.is_closure_body && !targets.iter().any(|target| target.id == function.id))
            || !proven(program, function)
        {
            continue;
        }
        if let Some(symbol) = closure_invoke_symbol(function.id) {
            out.push_str(&format!(
                "  if(invoke==(void*){symbol}) return 1; /* bounded owner handler {} */\n",
                function.id.0
            ));
        }
    }
    out.push_str("  return 0;\n}\n");
    out.push_str("static unsigned int ku_http_handler_request_fields(void* invoke) {\n");
    for function in &program.functions {
        if async_names.contains(&function.name)
            || (!function.is_closure_body && !targets.iter().any(|target| target.id == function.id))
        {
            continue;
        }
        if let (Some(mask), Some(symbol)) =
            (request_fields(function), closure_invoke_symbol(function.id))
        {
            out.push_str(&format!("  if(invoke==(void*){symbol}) return {mask}u;\n"));
        }
    }
    out.push_str("  return 63u; /* unknown or Task handler: retain complete request */\n}\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    fn accepts(source: &str, name: &str) -> bool {
        let ast = crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
            .parse_program()
            .unwrap();
        let program = crate::ir::lower_program(&ast).unwrap();
        proven(
            &program,
            program.functions.iter().find(|f| f.name == name).unwrap(),
        )
    }
    #[test]
    fn owner_proof_bounds_transitive_work_and_rejects_unknown_paths() {
        assert!(accepts("import \"std.http\" fn Ready(): HttpResponse { return http.text(\"ok\") } fn main() {}","Ready"));
        assert!(accepts("import \"std.http\" fn Text(): str { return \"ok\".clone() } fn Ready(): HttpResponse { return http.text(Text()) } fn main() {}","Ready"));
        assert!(accepts(
            "import \"std.http\" fn Ready(): HttpResponse! { return err(\"no\") } fn main() {}",
            "Ready"
        ));
        assert!(!accepts("import \"std.http\" fn Loop(): int { while (true) {} return 1 } fn Ready(): HttpResponse { n=Loop() return http.empty(n) } fn main() {}","Ready"));
        assert!(!accepts("import \"std.http\" fn Rec(): int { return Rec() } fn Ready(): HttpResponse { return http.empty(Rec()) } fn main() {}","Ready"));
        assert!(!accepts("import \"std.http\" fn Ready(req: HttpRequest): HttpResponse { return http.text(req.body) } fn main() {}","Ready"));
        let long = "a".repeat(MAX_STRING + 1);
        assert!(!accepts(&format!("import \"std.http\" fn Ready(): HttpResponse {{ return http.text(\"{long}\") }} fn main() {{}}"),"Ready"));
    }
    #[test]
    fn request_field_proof_rejects_whole_value_escape_and_keeps_direct_fields() {
        fn mask(source: &str) -> Option<u32> {
            let ast =
                crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                    .parse_program()
                    .unwrap();
            let program = crate::ir::lower_program(&ast).unwrap();
            request_fields(
                program
                    .functions
                    .iter()
                    .find(|function| function.name == "Ready")
                    .unwrap(),
            )
        }
        assert_eq!(mask("import \"std.http\" fn Ready(req: HttpRequest): HttpResponse { return http.text(req.body + req.query.id) } fn main() {}"),Some(36));
        assert_eq!(mask("import \"std.http\" fn Read(req: HttpRequest): HttpResponse { return http.text(req.body) } fn Ready(req: HttpRequest): HttpResponse { return Read(req) } fn main() {}"),None);
        assert_eq!(mask("import \"std.http\" fn Ready(req: HttpRequest): HttpResponse { copy=req.clone() return http.text(copy.body) } fn main() {}"),None);
        assert_eq!(mask("import \"std.http\" fn Ready(req: HttpRequest): HttpResponse { return http.text(req.headers.host + req.params.id + req.path + req.method) } fn main() {}"),Some(27));
    }
}

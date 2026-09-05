//! Verification of synchronous, non-owning parameter provenance. This runs on
//! generated IR as well as at the backend boundary, including optimized IR.
use super::*;

fn invalid(message: &str) -> KuError {
    KuError::runtime(format!("invalid borrowed IR: {message}"), Span::default())
}

fn children(expr: &IrExpr) -> Vec<&IrExpr> {
    match &expr.kind {
        IrExprKind::Borrow(x)
        | IrExprKind::CellLoad(x)
        | IrExprKind::TryUnwrap(x)
        | IrExprKind::Unary { expr: x, .. } => vec![x],
        IrExprKind::Field { target, .. } => vec![target],
        IrExprKind::Index { target, index } => vec![target, index],
        IrExprKind::Binary { left, right, .. } => vec![left, right],
        IrExprKind::Call { callee, args, .. } => {
            std::iter::once(callee.as_ref()).chain(args).collect()
        }
        IrExprKind::StructLiteral { fields, .. } => fields.iter().map(|(_, v)| v).collect(),
        IrExprKind::Array(values) => values.iter().collect(),
        _ => vec![],
    }
}

fn check_type(ty: &IrType) -> KuResult<()> {
    let mut pending = vec![(ty, 0)];
    while let Some((ty, depth)) = pending.pop() {
        if depth > 128 {
            return Err(invalid("type nesting exceeds verifier limit"));
        }
        match ty {
            IrType::Closure {
                params,
                param_modes,
                ret,
            } => {
                if params.len() != param_modes.len() {
                    return Err(invalid("parameter mode count mismatch"));
                }
                pending.extend(params.iter().map(|p| (p, depth + 1)));
                pending.push((ret, depth + 1));
            }
            IrType::Array(x) | IrType::Result(x) | IrType::Cell(x) => pending.push((x, depth + 1)),
            _ => {}
        }
    }
    Ok(())
}

struct Verifier<'a> {
    program: &'a IrProgram,
    params: HashMap<String, IrType>,
    aliases: HashSet<TempId>,
    temp_types: HashMap<TempId, IrType>,
}

impl Verifier<'_> {
    fn borrowed(&self, expr: &IrExpr) -> bool {
        let mut cursor = expr;
        loop {
            match &cursor.kind {
                IrExprKind::Borrow(_) | IrExprKind::BorrowedParam(_) => return true,
                IrExprKind::Local(name) => return self.params.contains_key(name),
                IrExprKind::Temp(id) | IrExprKind::BorrowedTemp(id) => {
                    return self.aliases.contains(id)
                }
                IrExprKind::Field { target, .. } | IrExprKind::Index { target, .. } => {
                    cursor = target
                }
                _ => return false,
            }
        }
    }

    fn consume(&self, expr: &IrExpr) -> KuResult<()> {
        self.check_consumed(expr)?;
        self.expr(expr)
    }

    fn check_consumed(&self, expr: &IrExpr) -> KuResult<()> {
        if ir_type_is_owned(&expr.ty) && self.borrowed(expr) {
            return Err(invalid("cannot move, store or return borrowed value"));
        }
        Ok(())
    }

    fn writable(&self, target: &IrLValue) -> KuResult<()> {
        match target {
            IrLValue::Field { target, .. } => self.expr(target)?,
            IrLValue::Index { target, index } => {
                self.expr(target)?;
                self.expr(index)?;
            }
            IrLValue::Local(_) => {}
        }
        let bad = match target {
            IrLValue::Local(name) => self.params.contains_key(name),
            IrLValue::Field { target, .. } | IrLValue::Index { target, .. } => {
                self.borrowed(target)
            }
        };
        if bad {
            Err(invalid("cannot modify through borrowed parameter"))
        } else {
            Ok(())
        }
    }

    fn expr(&self, expr: &IrExpr) -> KuResult<()> {
        let mut pending = vec![(expr, 0)];
        while let Some((expr, depth)) = pending.pop() {
            if depth > 128 {
                return Err(invalid("expression nesting exceeds verifier limit"));
            }
            self.expr_node(expr)?;
            pending.extend(children(expr).into_iter().map(|child| (child, depth + 1)));
        }
        Ok(())
    }

    fn expr_node(&self, expr: &IrExpr) -> KuResult<()> {
        check_type(&expr.ty)?;
        match &expr.kind {
            IrExprKind::BorrowedParam(name) => {
                if self.params.get(name) != Some(&expr.ty) {
                    return Err(invalid("unknown or mistyped borrowed parameter"));
                }
            }
            IrExprKind::Local(name) if self.params.contains_key(name) => {
                return Err(invalid("borrowed parameter read lost its provenance"))
            }
            IrExprKind::Temp(id) if self.aliases.contains(id) => {
                return Err(invalid("borrowed temporary read lost its provenance"))
            }
            IrExprKind::BorrowedTemp(id) if !self.aliases.contains(id) => {
                return Err(invalid("unproven borrowed temporary"))
            }
            IrExprKind::Temp(id) | IrExprKind::BorrowedTemp(id)
                if self.temp_types.get(id) != Some(&expr.ty) =>
            {
                return Err(invalid("unknown or mistyped temporary"))
            }
            IrExprKind::Call { callee, args, kind } => {
                let modes = match kind {
                    IrCallKind::Direct(id) => {
                        let f = self
                            .program
                            .functions
                            .iter()
                            .find(|f| f.id == *id)
                            .ok_or_else(|| invalid("unknown callee"))?;
                        // A lifted local function's recursive call supplies env.
                        let skip =
                            usize::from(f.is_closure_body && args.len() == f.params.len() + 1);
                        let mut modes = vec![ParamMode::Owned; skip];
                        modes.extend(f.params.iter().map(|p| p.mode));
                        Some(modes)
                    }
                    IrCallKind::Indirect => match &callee.ty {
                        IrType::Closure { param_modes, .. } => Some(param_modes.clone()),
                        _ => return Err(invalid("indirect callee is not a function value")),
                    },
                    IrCallKind::Intrinsic(_) => None,
                };
                if let Some(modes) = modes {
                    if modes.len() != args.len() {
                        return Err(invalid("call arity does not match parameter modes"));
                    }
                    for (arg, mode) in args.iter().zip(modes) {
                        if (mode == ParamMode::View) != matches!(arg.kind, IrExprKind::Borrow(_)) {
                            return Err(invalid("call argument parameter mode mismatch"));
                        }
                        if mode == ParamMode::Owned {
                            self.check_consumed(arg)?;
                        }
                    }
                } else if let IrCallKind::Intrinsic(name) = kind {
                    let borrowed_read = name == "__ku_clone"
                        || name == "__ku_enum_tag"
                        || (name.starts_with("__ku_enum_payload:") && !ir_type_is_owned(&expr.ty));
                    if !borrowed_read {
                        let signature = metadata::builtin_signature(name).or_else(|| {
                            name.split_once('.')
                                .and_then(|(m, f)| metadata::dotted_signature(m, f))
                        });
                        for (i, arg) in args.iter().enumerate() {
                            if self.borrowed(arg)
                                && ir_type_is_owned(&arg.ty)
                                && signature.as_ref().and_then(|s| s.arg_modes.get(i))
                                    != Some(&ParamMode::View)
                            {
                                return Err(invalid(
                                    "borrowed value passed to an owning intrinsic",
                                ));
                            }
                        }
                    }
                }
            }
            IrExprKind::StructLiteral { fields, .. } => {
                for (_, v) in fields {
                    self.check_consumed(v)?;
                }
            }
            IrExprKind::Array(values) => {
                for v in values {
                    self.check_consumed(v)?;
                }
            }
            IrExprKind::TryUnwrap(value) if self.borrowed(value) => {
                return Err(invalid("cannot consume borrowed Result"))
            }
            IrExprKind::MakeClosure {
                function_id,
                captures,
            } => {
                if captures.iter().any(|(n, _, _)| self.params.contains_key(n)) {
                    return Err(invalid("cannot capture borrowed parameter"));
                }
                let f = self
                    .program
                    .functions
                    .iter()
                    .find(|f| f.id == *function_id)
                    .ok_or_else(|| invalid("unknown function value"))?;
                if let IrType::Closure {
                    params,
                    param_modes,
                    ret,
                } = &expr.ty
                {
                    if params != &f.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>()
                        || param_modes != &f.params.iter().map(|p| p.mode).collect::<Vec<_>>()
                        || ret.as_ref() != &f.return_type
                    {
                        return Err(invalid(
                            "function value signature or parameter mode mismatch",
                        ));
                    }
                } else {
                    return Err(invalid("function value has a non-function type"));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn verify_borrow_contract(program: &IrProgram) -> KuResult<()> {
    for f in &program.functions {
        let mut v = Verifier {
            program,
            params: f
                .params
                .iter()
                .filter(|p| p.mode == ParamMode::View)
                .map(|p| (p.name.clone(), p.ty.clone()))
                .collect(),
            aliases: HashSet::new(),
            temp_types: HashMap::new(),
        };
        for p in &f.params {
            check_type(&p.ty)?;
        }
        check_type(&f.return_type)?;
        // Temps are numbered after their operands. A sorted pass propagates
        // aliases without CFG fixed-point iteration or an unbounded retry loop.
        let mut defs = f
            .blocks
            .iter()
            .flat_map(|b| &b.instructions)
            .filter_map(|i| match i {
                IrInst::Temp { id, ty, value } => Some((*id, ty, value, true)),
                IrInst::BindOk { id, ty, result } => Some((*id, ty, result, false)),
                _ => None,
            })
            .collect::<Vec<_>>();
        defs.sort_by_key(|(id, _, _, _)| id.0);
        for (id, ty, value, alias_allowed) in defs {
            check_type(ty)?;
            if alias_allowed && ty != &value.ty {
                return Err(invalid("temporary type differs from its value"));
            }
            if v.temp_types.insert(id, ty.clone()).is_some() {
                return Err(invalid("duplicate temporary definition"));
            }
            let mut pending = vec![value];
            while let Some(expr) = pending.pop() {
                if matches!(expr.kind, IrExprKind::Temp(operand) | IrExprKind::BorrowedTemp(operand) if operand.0 >= id.0)
                {
                    return Err(invalid("temporary operands must precede their definition"));
                }
                pending.extend(children(expr));
            }
            // A Copy projection is a snapshot, not a view of its owner.
            // Keep this aligned with FunctionLowerer::emit_temp.
            if alias_allowed && ir_type_is_owned(ty) && v.borrowed(value) {
                v.aliases.insert(id);
            }
        }
        for block in &f.blocks {
            for inst in &block.instructions {
                match inst {
                    IrInst::Temp { value, .. } => v.expr(value)?,
                    IrInst::Let { name, value, .. }
                    | IrInst::CellNew {
                        name, init: value, ..
                    } => {
                        if v.params.contains_key(name) {
                            return Err(invalid("borrowed parameter stored or boxed"));
                        }
                        v.consume(value)?;
                    }
                    IrInst::Store { target, value } => {
                        v.writable(target)?;
                        v.consume(value)?;
                    }
                    IrInst::CellStore { cell, value } => {
                        v.expr(cell)?;
                        if v.borrowed(cell) {
                            return Err(invalid("borrowed parameter written through cell"));
                        }
                        v.consume(value)?;
                    }
                    IrInst::BindOk { result, .. } | IrInst::BindError { result, .. } => {
                        v.consume(result)?
                    }
                    IrInst::Print(e) | IrInst::Expr(e) | IrInst::Panic(e) => v.expr(e)?,
                    IrInst::Fail(e) => v.consume(e)?,
                    IrInst::CellRelease(name) if v.params.contains_key(name) => {
                        return Err(invalid("borrowed parameter dropped"))
                    }
                    IrInst::DefineClosure { captures, .. }
                        if captures.iter().any(|n| v.params.contains_key(n)) =>
                    {
                        return Err(invalid("borrowed parameter captured"))
                    }
                    _ => {}
                }
            }
            match &block.terminator {
                IrTerminator::Return(Some(e))
                | IrTerminator::PropagateErr(e)
                | IrTerminator::JumpErr { result: e, .. }
                | IrTerminator::ResultBranch { result: e, .. }
                | IrTerminator::ForEach { iterable: e, .. } => v.consume(e)?,
                IrTerminator::Branch { condition, .. } => v.expr(condition)?,
                _ => {}
            }
        }
    }
    Ok(())
}

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    fmt,
    rc::Rc,
};

use crate::{
    ast::{
        AssignTarget, BinaryOp, EnumDecl, Expr, ExprKind, Item, Literal, MatchArm, MatchPattern,
        Program, Stmt, StructDecl, TypeName, UnaryOp,
    },
    error::{KuError, KuResult},
    span::Span,
    stdlib::metadata::{self, Signature, TypePattern},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrProgram {
    pub functions: Vec<IrFunction>,
    pub layouts: IrLayoutTable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrFunction {
    pub id: FunctionId,
    pub name: String,
    pub params: Vec<IrParam>,
    pub return_type: IrType,
    pub blocks: Vec<IrBlock>,
    /// True for lifted closure-body functions (Stage 6a). Their C signature
    /// carries a leading `void* __env` and they are invoked directly through a
    /// `KuClosure` `invoke` slot; top-level functions (false) are reached via a
    /// generated `__thunk` adapter instead.
    pub is_closure_body: bool,
    /// Stage 6b: the cells this closure body captures, in the same (sorted)
    /// order the enclosing `MakeClosure` passes them to `ku_env_{id}_new`. Each
    /// entry's type is `Cell(payload)`. Empty for top-level functions and for
    /// non-capturing closure bodies (Stage 6a).
    pub captures: Vec<(String, IrType)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TempId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrLayoutTable {
    pub structs: Vec<IrStructLayout>,
    pub enums: Vec<IrEnumLayout>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrStructLayout {
    pub name: String,
    pub fields: Vec<IrFieldLayout>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrEnumLayout {
    pub name: String,
    pub variants: Vec<IrVariantLayout>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrVariantLayout {
    pub name: String,
    pub tag: usize,
    pub fields: Vec<IrFieldLayout>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrFieldLayout {
    pub name: String,
    pub ty: IrType,
    pub offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrParam {
    pub name: String,
    pub ty: IrType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrBlock {
    pub id: BlockId,
    pub name: String,
    pub instructions: Vec<IrInst>,
    pub terminator: IrTerminator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrInst {
    Temp {
        id: TempId,
        ty: IrType,
        value: IrExpr,
    },
    BindOk {
        id: TempId,
        ty: IrType,
        result: IrExpr,
    },
    Let {
        name: String,
        ty: IrType,
        value: IrExpr,
    },
    Store {
        target: IrLValue,
        value: IrExpr,
    },
    Print(IrExpr),
    Expr(IrExpr),
    Fail(IrExpr),
    Panic(IrExpr),
    BeginTry {
        catch_block: Option<BlockId>,
        finally_block: Option<BlockId>,
        after_block: BlockId,
    },
    EndTry,
    BindError {
        name: String,
        result: IrExpr,
    },
    DefineClosure {
        name: String,
        function_id: FunctionId,
        captures: Vec<String>,
    },
    /// Stage 6b: box a captured Copy local into a heap `KuCell` (rc=1). `ty` is
    /// the payload type (Int/Float/Bool/Null); the local's IR type becomes
    /// `Cell(ty)`.
    CellNew {
        name: String,
        ty: IrType,
        init: IrExpr,
    },
    /// Stage 6b: write through a cell pointer (`cell->value = value`). `cell`
    /// evaluates to a `KuCell*` (a `Local` cell or a `CapturedCell`).
    CellStore {
        cell: IrExpr,
        value: IrExpr,
    },
    /// Stage 6b: release the outer scope's reference to a boxed local's cell.
    CellRelease(String),
    Unsupported {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrTerminator {
    Next,
    Jump(BlockId),
    Branch {
        condition: IrExpr,
        then_block: BlockId,
        else_block: BlockId,
    },
    ForEach {
        name: String,
        iterable: IrExpr,
        body_block: BlockId,
        after_block: BlockId,
    },
    ResultBranch {
        result: IrExpr,
        ok_block: BlockId,
        err_block: BlockId,
    },
    JumpErr {
        result: IrExpr,
        target: BlockId,
    },
    PropagateErr(IrExpr),
    Return(Option<IrExpr>),
    Unreachable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrLValue {
    Local(String),
    Index { target: IrExpr, index: IrExpr },
    Field { target: IrExpr, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrExpr {
    pub kind: IrExprKind,
    pub ty: IrType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrExprKind {
    Literal(String),
    Local(String),
    Temp(TempId),
    StructLiteral {
        name: String,
        fields: Vec<(String, IrExpr)>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<IrExpr>,
    },
    Binary {
        left: Box<IrExpr>,
        op: BinaryOp,
        right: Box<IrExpr>,
    },
    Call {
        callee: Box<IrExpr>,
        args: Vec<IrExpr>,
        kind: IrCallKind,
    },
    Array(Vec<IrExpr>),
    Index {
        target: Box<IrExpr>,
        index: Box<IrExpr>,
    },
    Field {
        target: Box<IrExpr>,
        name: String,
    },
    TryUnwrap(Box<IrExpr>),
    /// A closure value (Stage 6a): a function pointer paired with an env. In 6a
    /// `captures` is always empty and the env is NULL at runtime. `function_id`
    /// points either at a lifted closure body or at a top-level function (reached
    /// through a `__thunk`).
    MakeClosure {
        function_id: FunctionId,
        captures: Vec<(String, IrType)>,
    },
    /// Stage 6b: read a cell's payload (`cell->value`). The inner expr evaluates
    /// to a `KuCell*`; the result type is the payload type.
    CellLoad(Box<IrExpr>),
    /// Stage 6b: inside a closure body, the captured cell pointer for `name`
    /// (resolves to `__e->{name}`). Its type is `Cell(payload)`.
    CapturedCell(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrCallKind {
    Direct(FunctionId),
    Intrinsic(String),
    Indirect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrType {
    Int,
    Float,
    Bool,
    Str,
    Null,
    Array(Box<IrType>),
    Result(Box<IrType>),
    Named(String),
    Function,
    Closure {
        params: Vec<IrType>,
        ret: Box<IrType>,
    },
    /// Stage 6b: a heap-boxed Copy local shared with the closures that capture
    /// it. Backend type is `KuCell_{suffix}*`.
    Cell(Box<IrType>),
    Unknown,
    Void,
}

#[derive(Debug, Clone)]
struct FunctionSig {
    id: FunctionId,
    params: Vec<IrType>,
    returns: IrType,
}

/// Whether an expression names a place (variable/field/index/cell) rather than a
/// fresh owned rvalue (a literal, call, or arithmetic result). A place is still owned
/// by its binding, so it must not be materialized/moved when only borrowed.
fn ir_expr_is_place(expr: &IrExpr) -> bool {
    matches!(
        expr.kind,
        IrExprKind::Local(_)
            | IrExprKind::Temp(_)
            | IrExprKind::Field { .. }
            | IrExprKind::Index { .. }
            | IrExprKind::CapturedCell(_)
            | IrExprKind::CellLoad(_)
    )
}

/// Whether a value of this type owns heap memory and therefore needs a clone/drop
/// (rather than being a trivially-copyable Copy value). Matches the set of types
/// `c_clone_expr` / `c_drop_value` know how to handle in the backend.
fn ir_type_is_owned(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::Str
            | IrType::Array(_)
            | IrType::Result(_)
            | IrType::Named(_)
            | IrType::Closure { .. }
    )
}

pub fn lower_program(program: &Program) -> KuResult<IrProgram> {
    let layouts = lower_layouts(program);
    let mut signatures = HashMap::new();
    let mut next_function_id = 0;
    for item in &program.items {
        if let Item::Function(function) = item {
            let id = FunctionId(next_function_id);
            next_function_id += 1;
            signatures.insert(
                function.name.clone(),
                FunctionSig {
                    id,
                    params: function
                        .params
                        .iter()
                        .map(|p| lower_optional_type(&p.ty, &layouts))
                        .collect(),
                    returns: function
                        .return_type
                        .as_ref()
                        .map(|ty| lower_type(ty, &layouts))
                        .unwrap_or(IrType::Void),
                },
            );
        }
    }

    // Seed the shared FunctionId allocator past the top-level function ids so
    // lifted closure bodies get fresh, globally-unique ids.
    let next_function_id = Rc::new(Cell::new(next_function_id));
    let lifted_functions = Rc::new(RefCell::new(Vec::new()));

    // Stage 8e: infer the return type of a top-level function declared without a
    // `: T` annotation from the body's `return <value>` — the same body-based
    // inference closures already use. Without this, an unannotated function
    // lowered with a `void` return type, so referencing it as a value (e.g. a
    // named HTTP route handler `app.get("/x", handler)`) produced a
    // `Closure { ret: void }` the C backend cannot emit. The checker already
    // infers unannotated returns, so this keeps native == checker (rule 8).
    //
    // Each unannotated body is lowered once in a throwaway probe (Unknown seed,
    // like a closure literal, so `try`/`finally` return slots are still created)
    // and the first concrete `return` type is recovered. Probe failures and
    // value-less bodies fall back to `void` — identical to the previous
    // behaviour — so this is strictly additive. Functions are visited in source
    // order and their signatures updated in place, so a later function that
    // returns an earlier one's result sees the inferred type.
    for item in &program.items {
        let Item::Function(function) = item else {
            continue;
        };
        if function.return_type.is_some() {
            continue;
        }
        let saved_id = next_function_id.get();
        let inferred = {
            let throwaway_lifted: Rc<RefCell<Vec<IrFunction>>> =
                Rc::new(RefCell::new(Vec::new()));
            let mut probe = FunctionLowerer::new(
                &signatures,
                &layouts,
                IrType::Unknown,
                next_function_id.clone(),
                throwaway_lifted,
            );
            if let Some(sig) = signatures.get(&function.name) {
                for (param, ty) in function.params.iter().zip(sig.params.iter()) {
                    probe.locals.insert(param.name.clone(), ty.clone());
                }
            }
            if probe
                .lower_block_body("entry", &function.body, function.span)
                .is_ok()
            {
                // Fold the body's `return <value>` types the way the checker's
                // merge_return_types does: `null` is the identity element, so a
                // body that returns `null` on one path and a concrete type on
                // another has the concrete type -- taking the first return
                // instead would infer `null` where the checker says `int`. Only
                // an all-`null` body is `null`; a body with no value return (or
                // one whose returns route through a `try`/`finally` slot, which
                // the Unknown seed leaves untyped) stays `void`, which is the
                // pre-existing behaviour.
                let mut found = IrType::Void;
                for block in &probe.blocks {
                    let IrTerminator::Return(Some(value)) = &block.terminator else {
                        continue;
                    };
                    if value.ty == IrType::Unknown {
                        continue;
                    }
                    if value.ty == IrType::Null {
                        if found == IrType::Void {
                            found = IrType::Null;
                        }
                        continue;
                    }
                    found = value.ty.clone();
                    break;
                }
                found
            } else {
                IrType::Void
            }
        };
        // Give back the FunctionId range the probe consumed for nested closures
        // so the real lowering re-allocates identical ids; the probe's lifted
        // bodies and blocks are dropped.
        next_function_id.set(saved_id);
        if inferred != IrType::Void {
            if let Some(sig) = signatures.get_mut(&function.name) {
                sig.returns = inferred;
            }
        }
    }

    let mut functions = Vec::new();
    for item in &program.items {
        if let Item::Function(function) = item {
            let signature = signatures
                .get(&function.name)
                .ok_or_else(|| KuError::runtime("missing function signature", function.span))?;
            let params = function
                .params
                .iter()
                .zip(signature.params.iter())
                .map(|(param, ty)| IrParam {
                    name: param.name.clone(),
                    ty: ty.clone(),
                })
                .collect::<Vec<_>>();
            let mut lower = FunctionLowerer::new(
                &signatures,
                &layouts,
                signature.returns.clone(),
                next_function_id.clone(),
                lifted_functions.clone(),
            );
            for param in &params {
                lower.locals.insert(param.name.clone(), param.ty.clone());
            }
            lower.lower_block_body("entry", &function.body, function.span)?;
            functions.push(IrFunction {
                id: signature.id,
                name: function.name.clone(),
                params,
                return_type: signature.returns.clone(),
                blocks: lower.blocks,
                is_closure_body: false,
                captures: Vec::new(),
            });
        }
    }
    functions.append(&mut lifted_functions.borrow_mut());
    Ok(IrProgram { functions, layouts })
}

pub fn optimize_program(program: &IrProgram) -> IrProgram {
    IrProgram {
        functions: program.functions.iter().map(optimize_function).collect(),
        layouts: program.layouts.clone(),
    }
}

fn optimize_function(function: &IrFunction) -> IrFunction {
    let mut optimized = function.clone();
    for block in &mut optimized.blocks {
        for inst in &mut block.instructions {
            optimize_inst(inst);
        }
        optimize_terminator(&mut block.terminator);
    }
    remove_unreachable_blocks(&mut optimized);
    optimized
}

fn optimize_inst(inst: &mut IrInst) {
    match inst {
        IrInst::Temp { value, .. }
        | IrInst::BindOk { result: value, .. }
        | IrInst::Let { value, .. }
        | IrInst::Store { value, .. }
        | IrInst::Print(value)
        | IrInst::Expr(value)
        | IrInst::Fail(value)
        | IrInst::Panic(value) => {
            *value = optimize_expr(value.clone());
        }
        IrInst::CellNew { init, .. } => {
            *init = optimize_expr(init.clone());
        }
        IrInst::CellStore { cell, value } => {
            *cell = optimize_expr(cell.clone());
            *value = optimize_expr(value.clone());
        }
        IrInst::BeginTry { .. }
        | IrInst::EndTry
        | IrInst::BindError { .. }
        | IrInst::DefineClosure { .. }
        | IrInst::CellRelease(_)
        | IrInst::Unsupported { .. } => {}
    }
}

fn optimize_terminator(terminator: &mut IrTerminator) {
    match terminator {
        IrTerminator::Branch {
            condition,
            then_block,
            else_block,
        } => {
            *condition = optimize_expr(condition.clone());
            if let Some(value) = bool_literal_value(condition) {
                *terminator = IrTerminator::Jump(if value { *then_block } else { *else_block });
            }
        }
        IrTerminator::ForEach { iterable, .. } => {
            *iterable = optimize_expr(iterable.clone());
        }
        IrTerminator::ResultBranch { result, .. }
        | IrTerminator::JumpErr { result, .. }
        | IrTerminator::PropagateErr(result)
        | IrTerminator::Return(Some(result)) => {
            *result = optimize_expr(result.clone());
        }
        IrTerminator::Next
        | IrTerminator::Jump(_)
        | IrTerminator::Return(None)
        | IrTerminator::Unreachable => {}
    }
}

fn optimize_expr(expr: IrExpr) -> IrExpr {
    match expr.kind {
        IrExprKind::Unary { op, expr: inner } => {
            let inner = optimize_expr(*inner);
            fold_unary(op, inner).unwrap_or_else(|inner| IrExpr {
                ty: expr.ty,
                kind: IrExprKind::Unary {
                    op,
                    expr: Box::new(inner),
                },
            })
        }
        IrExprKind::Binary { left, op, right } => {
            let left = optimize_expr(*left);
            let right = optimize_expr(*right);
            if let Some(folded) = fold_binary(&left, op, &right) {
                folded
            } else {
                IrExpr {
                    ty: expr.ty,
                    kind: IrExprKind::Binary {
                        left: Box::new(left),
                        op,
                        right: Box::new(right),
                    },
                }
            }
        }
        IrExprKind::Call { callee, args, kind } => IrExpr {
            ty: expr.ty,
            kind: IrExprKind::Call {
                callee: Box::new(optimize_expr(*callee)),
                args: args.into_iter().map(optimize_expr).collect(),
                kind,
            },
        },
        IrExprKind::Array(values) => IrExpr {
            ty: expr.ty,
            kind: IrExprKind::Array(values.into_iter().map(optimize_expr).collect()),
        },
        IrExprKind::Index { target, index } => IrExpr {
            ty: expr.ty,
            kind: IrExprKind::Index {
                target: Box::new(optimize_expr(*target)),
                index: Box::new(optimize_expr(*index)),
            },
        },
        IrExprKind::Field { target, name } => IrExpr {
            ty: expr.ty,
            kind: IrExprKind::Field {
                target: Box::new(optimize_expr(*target)),
                name,
            },
        },
        IrExprKind::StructLiteral { name, fields } => IrExpr {
            ty: expr.ty,
            kind: IrExprKind::StructLiteral {
                name,
                fields: fields
                    .into_iter()
                    .map(|(name, value)| (name, optimize_expr(value)))
                    .collect(),
            },
        },
        IrExprKind::TryUnwrap(value) => IrExpr {
            ty: expr.ty,
            kind: IrExprKind::TryUnwrap(Box::new(optimize_expr(*value))),
        },
        IrExprKind::CellLoad(inner) => IrExpr {
            ty: expr.ty,
            kind: IrExprKind::CellLoad(Box::new(optimize_expr(*inner))),
        },
        IrExprKind::Literal(_)
        | IrExprKind::Local(_)
        | IrExprKind::Temp(_)
        | IrExprKind::MakeClosure { .. }
        | IrExprKind::CapturedCell(_) => expr,
    }
}

fn fold_unary(op: UnaryOp, expr: IrExpr) -> Result<IrExpr, IrExpr> {
    match op {
        UnaryOp::Negate => {
            if let Some(value) = int_literal_value(&expr) {
                if let Some(value) = value.checked_neg() {
                    return Ok(int_literal(value));
                }
            }
        }
        UnaryOp::Not => {
            if let Some(value) = bool_literal_value(&expr) {
                return Ok(bool_literal(!value));
            }
        }
    }
    Err(expr)
}

fn fold_binary(left: &IrExpr, op: BinaryOp, right: &IrExpr) -> Option<IrExpr> {
    if let (Some(left_value), Some(right_value)) =
        (int_literal_value(left), int_literal_value(right))
    {
        let folded = match op {
            BinaryOp::Add => left_value.checked_add(right_value).map(int_literal),
            BinaryOp::Subtract => left_value.checked_sub(right_value).map(int_literal),
            BinaryOp::Multiply => left_value.checked_mul(right_value).map(int_literal),
            BinaryOp::Divide if right_value != 0 => {
                left_value.checked_div(right_value).map(int_literal)
            }
            BinaryOp::Remainder if right_value != 0 => {
                left_value.checked_rem(right_value).map(int_literal)
            }
            BinaryOp::Equal => Some(bool_literal(left_value == right_value)),
            BinaryOp::NotEqual => Some(bool_literal(left_value != right_value)),
            BinaryOp::Less => Some(bool_literal(left_value < right_value)),
            BinaryOp::LessEqual => Some(bool_literal(left_value <= right_value)),
            BinaryOp::Greater => Some(bool_literal(left_value > right_value)),
            BinaryOp::GreaterEqual => Some(bool_literal(left_value >= right_value)),
            BinaryOp::Divide | BinaryOp::Remainder | BinaryOp::And | BinaryOp::Or => None,
        };
        if let Some(expr) = folded {
            return Some(expr);
        }
    }
    if let (Some(left_value), Some(right_value)) =
        (bool_literal_value(left), bool_literal_value(right))
    {
        let folded = match op {
            BinaryOp::And => Some(bool_literal(left_value && right_value)),
            BinaryOp::Or => Some(bool_literal(left_value || right_value)),
            BinaryOp::Equal => Some(bool_literal(left_value == right_value)),
            BinaryOp::NotEqual => Some(bool_literal(left_value != right_value)),
            _ => None,
        };
        if let Some(expr) = folded {
            return Some(expr);
        }
    }
    None
}

fn remove_unreachable_blocks(function: &mut IrFunction) {
    let Some(entry) = function.blocks.first().map(|block| block.id) else {
        return;
    };
    let mut index_by_id = HashMap::new();
    for (index, block) in function.blocks.iter().enumerate() {
        index_by_id.insert(block.id, index);
    }
    let mut stack = vec![entry];
    let mut reachable = HashSet::new();
    while let Some(id) = stack.pop() {
        if !reachable.insert(id) {
            continue;
        }
        let Some(&index) = index_by_id.get(&id) else {
            continue;
        };
        for target in terminator_successors(
            &function.blocks[index].terminator,
            function.blocks.get(index + 1).map(|block| block.id),
        ) {
            stack.push(target);
        }
    }
    function
        .blocks
        .retain(|block| reachable.contains(&block.id));
}

fn terminator_successors(terminator: &IrTerminator, next: Option<BlockId>) -> Vec<BlockId> {
    match terminator {
        IrTerminator::Next => next.into_iter().collect(),
        IrTerminator::Jump(target) | IrTerminator::JumpErr { target, .. } => vec![*target],
        IrTerminator::Branch {
            then_block,
            else_block,
            ..
        } => vec![*then_block, *else_block],
        IrTerminator::ForEach {
            body_block,
            after_block,
            ..
        }
        | IrTerminator::ResultBranch {
            ok_block: body_block,
            err_block: after_block,
            ..
        } => vec![*body_block, *after_block],
        IrTerminator::PropagateErr(_) | IrTerminator::Return(_) | IrTerminator::Unreachable => {
            Vec::new()
        }
    }
}

fn int_literal(value: i64) -> IrExpr {
    IrExpr {
        kind: IrExprKind::Literal(value.to_string()),
        ty: IrType::Int,
    }
}

fn int_literal_value(expr: &IrExpr) -> Option<i64> {
    if expr.ty != IrType::Int {
        return None;
    }
    let IrExprKind::Literal(value) = &expr.kind else {
        return None;
    };
    value.parse().ok()
}

fn bool_literal_value(expr: &IrExpr) -> Option<bool> {
    if expr.ty != IrType::Bool {
        return None;
    }
    let IrExprKind::Literal(value) = &expr.kind else {
        return None;
    };
    match value.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

impl fmt::Display for IrProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.layouts.structs.is_empty() || !self.layouts.enums.is_empty() {
            writeln!(f, "layouts {{")?;
            for layout in &self.layouts.structs {
                write!(f, "  struct {} {{", layout.name)?;
                for (index, field) in layout.fields.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}@{}: {}", field.name, field.offset, field.ty)?;
                }
                writeln!(f, "}}")?;
            }
            for layout in &self.layouts.enums {
                writeln!(f, "  enum {} {{", layout.name)?;
                for variant in &layout.variants {
                    write!(f, "    #{} {}(", variant.tag, variant.name)?;
                    for (index, field) in variant.fields.iter().enumerate() {
                        if index > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}@{}: {}", field.name, field.offset, field.ty)?;
                    }
                    writeln!(f, ")")?;
                }
                writeln!(f, "  }}")?;
            }
            writeln!(f, "}}")?;
        }
        for function in &self.functions {
            write!(f, "fn {}(", function.name)?;
            for (index, param) in function.params.iter().enumerate() {
                if index > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}: {}", param.name, param.ty)?;
            }
            writeln!(f, ") -> {} {{", function.return_type)?;
            for block in &function.blocks {
                writeln!(f, "  {}:", block.name)?;
                for inst in &block.instructions {
                    writeln!(f, "    {inst}")?;
                }
                if block.terminator != IrTerminator::Next {
                    writeln!(f, "    {}", block.terminator)?;
                }
            }
            writeln!(f, "}}")?;
        }
        Ok(())
    }
}

impl fmt::Display for IrType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrType::Int => write!(f, "int"),
            IrType::Float => write!(f, "float"),
            IrType::Bool => write!(f, "bool"),
            IrType::Str => write!(f, "str"),
            IrType::Null => write!(f, "null"),
            IrType::Array(inner) => write!(f, "[{inner}]"),
            IrType::Result(inner) => write!(f, "{inner}!"),
            IrType::Named(name) => write!(f, "{}", enum_type_name(name).unwrap_or(name)),
            IrType::Function => write!(f, "function"),
            IrType::Closure { .. } => write!(f, "closure"),
            IrType::Cell(inner) => write!(f, "cell<{inner}>"),
            IrType::Unknown => write!(f, "unknown"),
            IrType::Void => write!(f, "void"),
        }
    }
}

impl fmt::Display for IrInst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrInst::Temp { id, ty, value } => write!(f, "%t{}: {ty} = {value}", id.0),
            IrInst::BindOk { id, ty, result } => {
                write!(f, "%t{}: {ty} = ok_value {result}", id.0)
            }
            IrInst::Let { name, ty, value } => write!(f, "let {name}: {ty} = {value}"),
            IrInst::Store { target, value } => write!(f, "store {target} = {value}"),
            IrInst::Print(value) => write!(f, "print {value}"),
            IrInst::Expr(value) => write!(f, "expr {value}"),
            IrInst::Fail(value) => write!(f, "fail {value}"),
            IrInst::Panic(value) => write!(f, "panic {value}"),
            IrInst::BeginTry {
                catch_block,
                finally_block,
                after_block,
            } => {
                write!(f, "try after block{}", after_block.0)?;
                if let Some(block) = catch_block {
                    write!(f, " catch block{}", block.0)?;
                }
                if let Some(block) = finally_block {
                    write!(f, " finally block{}", block.0)?;
                }
                Ok(())
            }
            IrInst::EndTry => write!(f, "end_try"),
            IrInst::BindError { name, result } => write!(f, "bind_error {name} from {result}"),
            IrInst::DefineClosure {
                name,
                function_id,
                captures,
            } => {
                write!(f, "closure {name} = fn#{}", function_id.0)?;
                if !captures.is_empty() {
                    write!(f, " captures [{}]", captures.join(", "))?;
                }
                Ok(())
            }
            IrInst::CellNew { name, ty, init } => {
                write!(f, "cell_new {name}: {ty} = {init}")
            }
            IrInst::CellStore { cell, value } => write!(f, "cell_store {cell} = {value}"),
            IrInst::CellRelease(name) => write!(f, "cell_release {name}"),
            IrInst::Unsupported { reason } => write!(f, "unsupported {reason}"),
        }
    }
}

impl fmt::Display for IrTerminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrTerminator::Next => Ok(()),
            IrTerminator::Jump(target) => write!(f, "jump block{}", target.0),
            IrTerminator::Branch {
                condition,
                then_block,
                else_block,
            } => write!(
                f,
                "branch {condition} ? block{} : block{}",
                then_block.0, else_block.0
            ),
            IrTerminator::ForEach {
                name,
                iterable,
                body_block,
                after_block,
            } => write!(
                f,
                "foreach {name} in {iterable} ? block{} : block{}",
                body_block.0, after_block.0
            ),
            IrTerminator::ResultBranch {
                result,
                ok_block,
                err_block,
            } => write!(
                f,
                "result_branch {result} ok block{} err block{}",
                ok_block.0, err_block.0
            ),
            IrTerminator::JumpErr { result, target } => {
                write!(f, "jump_err {result} block{}", target.0)
            }
            IrTerminator::PropagateErr(value) => write!(f, "propagate_err {value}"),
            IrTerminator::Return(Some(value)) => write!(f, "return {value}"),
            IrTerminator::Return(None) => write!(f, "return"),
            IrTerminator::Unreachable => write!(f, "unreachable"),
        }
    }
}

impl fmt::Display for IrLValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrLValue::Local(name) => write!(f, "{name}"),
            IrLValue::Index { target, index } => write!(f, "{target}[{index}]"),
            IrLValue::Field { target, name } => write!(f, "{target}.{name}"),
        }
    }
}

impl fmt::Display for IrExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            IrExprKind::Literal(value) | IrExprKind::Local(value) => write!(f, "{value}"),
            IrExprKind::Temp(id) => write!(f, "%t{}", id.0),
            IrExprKind::StructLiteral { name, fields } => {
                write!(f, "{name} {{")?;
                for (index, (field, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{field}: {value}")?;
                }
                write!(f, "}}")
            }
            IrExprKind::Unary { op, expr } => write!(f, "{}{}", unary_text(*op), expr),
            IrExprKind::Binary { left, op, right } => {
                write!(f, "{left} {} {right}", binary_text(*op))
            }
            IrExprKind::Call { callee, args, .. } => {
                write!(f, "{callee}(")?;
                for (index, arg) in args.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                write!(f, ")")
            }
            IrExprKind::Array(values) => {
                write!(f, "[")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{value}")?;
                }
                write!(f, "]")
            }
            IrExprKind::Index { target, index } => write!(f, "{target}[{index}]"),
            IrExprKind::Field { target, name } => write!(f, "{target}.{name}"),
            IrExprKind::TryUnwrap(value) => write!(f, "{value}?"),
            IrExprKind::MakeClosure { function_id, .. } => {
                write!(f, "make_closure fn#{}", function_id.0)
            }
            IrExprKind::CellLoad(inner) => write!(f, "cell_load {inner}"),
            IrExprKind::CapturedCell(name) => write!(f, "captured_cell {name}"),
        }
    }
}

struct FunctionLowerer<'a> {
    signatures: &'a HashMap<String, FunctionSig>,
    layouts: &'a IrLayoutTable,
    return_type: IrType,
    locals: HashMap<String, IrType>,
    blocks: Vec<IrBlock>,
    current: IrBlock,
    next_block_id: usize,
    next_temp_id: usize,
    try_handlers: Vec<IrTryHandler>,
    pattern_bindings: HashMap<String, IrExpr>,
    /// Program-global FunctionId allocator, shared between the top-level lowerer
    /// and every child lowerer that lifts a closure body (Stage 6a).
    next_function_id: Rc<Cell<usize>>,
    /// Closure bodies lifted out of expressions, appended to the program's
    /// functions once every top-level function has been lowered.
    lifted_functions: Rc<RefCell<Vec<IrFunction>>>,
    /// Stage 6b: names declared in this function body that some closure literal
    /// captures, so their first `Let`/`Assign` boxes them into a `KuCell`.
    boxed: HashSet<String>,
    /// Stage 6b: for a closure-body lowerer, the cells captured from the
    /// enclosing scope (`name` -> `Cell(payload)`). Reads become
    /// `CellLoad(CapturedCell(name))`; writes become
    /// `CellStore{CapturedCell(name), ..}`. Empty for top-level functions.
    captures: HashMap<String, IrType>,
    /// Stage 6f: inside a lifted local-named-function body, the function's own
    /// name plus the lifted function id and its return type. A call to this name
    /// in the body lowers to a direct call to the lifted function threading the
    /// running `__env` (rather than capturing the function into its own env,
    /// which would form a reference cycle). `None` everywhere else.
    self_recurse: Option<(String, FunctionId, IrType)>,
}

#[derive(Debug, Clone)]
struct IrTryHandler {
    error_block: BlockId,
    error_name: String,
    return_block: Option<BlockId>,
    return_name: Option<String>,
}

impl<'a> FunctionLowerer<'a> {
    fn new(
        signatures: &'a HashMap<String, FunctionSig>,
        layouts: &'a IrLayoutTable,
        return_type: IrType,
        next_function_id: Rc<Cell<usize>>,
        lifted_functions: Rc<RefCell<Vec<IrFunction>>>,
    ) -> Self {
        Self {
            signatures,
            layouts,
            return_type,
            locals: HashMap::new(),
            blocks: Vec::new(),
            current: IrBlock {
                id: BlockId(0),
                name: "entry".to_string(),
                instructions: Vec::new(),
                terminator: IrTerminator::Next,
            },
            next_block_id: 1,
            next_temp_id: 0,
            try_handlers: Vec::new(),
            pattern_bindings: HashMap::new(),
            next_function_id,
            lifted_functions,
            boxed: HashSet::new(),
            captures: HashMap::new(),
            self_recurse: None,
        }
    }

    fn lower_block_body(&mut self, name: &str, body: &[Stmt], span: Span) -> KuResult<()> {
        // Stage 6b: any local this body declares that a nested closure literal
        // captures must be boxed into a shared `KuCell` at its declaration.
        let mut boxed = HashSet::new();
        collect_boxed_candidates(body, &mut boxed);
        self.boxed = boxed;
        self.current.name = name.to_string();
        for stmt in body {
            self.lower_stmt(stmt)?;
            if self.current.terminator != IrTerminator::Next {
                break;
            }
        }
        if self.current.instructions.len() > 10_000 || self.blocks.len() > 10_000 {
            return Err(KuError::runtime("ir function is too large", span));
        }
        self.finish_current();
        Ok(())
    }

    fn lower_stmt(&mut self, stmt: &Stmt) -> KuResult<()> {
        match stmt {
            Stmt::VarDecl {
                name, ty, value, ..
            } => {
                let span = value.span;
                let declared = ty.as_ref().map(|ty| lower_type(ty, self.layouts));
                let value = self.lower_expr_with_expected(value, declared.as_ref())?;
                let ty = declared.unwrap_or_else(|| value.ty.clone());
                if self.boxed.contains(name) {
                    self.push_cell_new(name.clone(), ty, value, span)?;
                } else {
                    self.locals.insert(name.clone(), ty.clone());
                    self.current.instructions.push(IrInst::Let {
                        name: name.clone(),
                        ty,
                        value,
                    });
                }
            }
            Stmt::Assign { name, value, .. } => {
                let span = value.span;
                // A closure assigned to an already-declared function-typed local
                // takes that local's type as its expected function type.
                let expected = match self.locals.get(name) {
                    Some(IrType::Cell(inner)) => Some((**inner).clone()),
                    Some(ty) => Some(ty.clone()),
                    None => self.captures.get(name).and_then(|ty| match ty {
                        IrType::Cell(inner) => Some((**inner).clone()),
                        other => Some(other.clone()),
                    }),
                };
                let value = self.lower_expr_with_expected(value, expected.as_ref())?;
                if self.captures.contains_key(name) {
                    // Closure body writing a cell captured from the outer scope.
                    let cell = self.captured_cell_expr(name);
                    self.current
                        .instructions
                        .push(IrInst::CellStore { cell, value });
                } else if let Some(inner) = self.boxed_local_inner(name) {
                    // Write through an already-boxed local's cell.
                    let cell = IrExpr {
                        kind: IrExprKind::Local(name.clone()),
                        ty: IrType::Cell(Box::new(inner)),
                    };
                    self.current
                        .instructions
                        .push(IrInst::CellStore { cell, value });
                } else if self.boxed.contains(name) && !self.locals.contains_key(name) {
                    // First assignment to a to-be-boxed local: allocate its cell.
                    let inner = value.ty.clone();
                    self.push_cell_new(name.clone(), inner, value, span)?;
                } else if self.locals.contains_key(name) {
                    self.current.instructions.push(IrInst::Store {
                        target: IrLValue::Local(name.clone()),
                        value,
                    });
                } else {
                    let ty = value.ty.clone();
                    self.locals.insert(name.clone(), ty.clone());
                    self.current.instructions.push(IrInst::Let {
                        name: name.clone(),
                        ty,
                        value,
                    });
                }
            }
            Stmt::AssignTarget { target, value, .. } => {
                let target = self.lower_lvalue(target)?;
                let value = self.lower_expr(value)?;
                self.current
                    .instructions
                    .push(IrInst::Store { target, value });
            }
            Stmt::CompoundAssign {
                target, op, value, ..
            } => {
                let target = self.lower_lvalue_cached(target)?;
                let left = self.lvalue_read_expr(&target);
                let right = self.lower_expr(value)?;
                let value = self.emit_temp(IrExpr {
                    ty: binary_type(*op, &left.ty, &right.ty),
                    kind: IrExprKind::Binary {
                        left: Box::new(left),
                        op: *op,
                        right: Box::new(right),
                    },
                })?;
                self.current
                    .instructions
                    .push(IrInst::Store { target, value });
            }
            Stmt::DestructureAssign {
                names,
                values,
                span,
            } => {
                if names.len() != values.len() {
                    return Err(KuError::runtime(
                        format!(
                            "destructuring assignment expects {} values but got {}",
                            names.len(),
                            values.len()
                        ),
                        *span,
                    ));
                }
                let values = values
                    .iter()
                    .map(|value| {
                        let value = self.lower_expr(value)?;
                        self.emit_temp(value)
                    })
                    .collect::<KuResult<Vec<_>>>()?;
                for (name, value) in names.iter().zip(values) {
                    let Some(name) = name else {
                        continue;
                    };
                    if self.locals.contains_key(name) {
                        self.current.instructions.push(IrInst::Store {
                            target: IrLValue::Local(name.clone()),
                            value,
                        });
                    } else {
                        let ty = value.ty.clone();
                        self.locals.insert(name.clone(), ty.clone());
                        self.current.instructions.push(IrInst::Let {
                            name: name.clone(),
                            ty,
                            value,
                        });
                    }
                }
            }
            Stmt::ObjectDestructureAssign { span, .. } => {
                return Err(KuError::runtime(
                    "IR/native lowering does not support object destructuring yet; use interpreter mode or destructure fields explicitly",
                    *span,
                ));
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => self.lower_if(condition, then_branch, else_branch)?,
            Stmt::While {
                condition, body, ..
            } => self.lower_while(condition, body)?,
            Stmt::For {
                name,
                iterable,
                body,
                ..
            } => {
                let iterable = self.lower_expr(iterable)?;
                self.lower_for(name, iterable, body)?;
            }
            Stmt::Break { span } | Stmt::Continue { span } => {
                return Err(KuError::runtime(
                    "break/continue are not supported by IR/native lowering yet",
                    *span,
                ));
            }
            Stmt::Function(function) => self.lower_local_function(function)?,
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                finally_body,
                ..
            } => self.lower_try(body, catch_name, catch_body, finally_body)?,
            Stmt::Fail { value, .. } => {
                let value = self.lower_error_expr(value)?;
                if self.try_handlers.is_empty() {
                    self.current.instructions.push(IrInst::Fail(value));
                    self.current.terminator = IrTerminator::Unreachable;
                } else {
                    let result_ty = match &self.return_type {
                        IrType::Result(_) => self.return_type.clone(),
                        _ => IrType::Result(Box::new(IrType::Null)),
                    };
                    let result = IrExpr {
                        kind: IrExprKind::Call {
                            callee: Box::new(IrExpr {
                                kind: IrExprKind::Local("err".to_string()),
                                ty: IrType::Function,
                            }),
                            args: vec![value],
                            kind: IrCallKind::Intrinsic("err".to_string()),
                        },
                        ty: result_ty,
                    };
                    self.current.terminator = self.err_terminator(result);
                }
            }
            Stmt::Panic { value, .. } => {
                let value = self.lower_expr(value)?;
                self.current.instructions.push(IrInst::Panic(value));
                self.current.terminator = IrTerminator::Unreachable;
            }
            Stmt::Return { value, .. } => {
                let expected = self.return_type.clone();
                let value = value
                    .as_ref()
                    .map(|value| self.lower_expr_with_expected(value, Some(&expected)))
                    .transpose()?;
                self.current.terminator = self.return_terminator(value);
            }
            Stmt::Print { value, .. } => {
                let value = self.lower_expr(value)?;
                self.current.instructions.push(IrInst::Print(value));
            }
            Stmt::Expr { expr: value, .. } => {
                let value = self.lower_expr(value)?;
                self.current.instructions.push(IrInst::Expr(value));
            }
        }
        Ok(())
    }

    fn lower_if(
        &mut self,
        condition: &Expr,
        then_branch: &[Stmt],
        else_branch: &[Stmt],
    ) -> KuResult<()> {
        let condition = self.lower_expr(condition)?;
        let then_id = self.next_block("then");
        let else_id = self.next_block("else");
        let after_id = self.next_block("after");
        self.current.terminator = IrTerminator::Branch {
            condition,
            then_block: then_id,
            else_block: else_id,
        };
        self.finish_current();

        self.start_block(then_id, "then");
        for stmt in then_branch {
            self.lower_stmt(stmt)?;
            if self.current.terminator != IrTerminator::Next {
                break;
            }
        }
        if self.current.terminator == IrTerminator::Next {
            self.current.terminator = IrTerminator::Jump(after_id);
        }
        self.finish_current();

        self.start_block(else_id, "else");
        for stmt in else_branch {
            self.lower_stmt(stmt)?;
            if self.current.terminator != IrTerminator::Next {
                break;
            }
        }
        if self.current.terminator == IrTerminator::Next {
            self.current.terminator = IrTerminator::Jump(after_id);
        }
        self.finish_current();

        self.start_block(after_id, "after");
        Ok(())
    }

    fn lower_while(&mut self, condition: &Expr, body: &[Stmt]) -> KuResult<()> {
        let cond_id = self.next_block("while_cond");
        let body_id = self.next_block("while_body");
        let after_id = self.next_block("while_after");
        self.current.terminator = IrTerminator::Jump(cond_id);
        self.finish_current();

        self.start_block(cond_id, "while_cond");
        let condition = self.lower_expr(condition)?;
        self.current.terminator = IrTerminator::Branch {
            condition,
            then_block: body_id,
            else_block: after_id,
        };
        self.finish_current();

        self.start_block(body_id, "while_body");
        for stmt in body {
            self.lower_stmt(stmt)?;
            if self.current.terminator != IrTerminator::Next {
                break;
            }
        }
        if self.current.terminator == IrTerminator::Next {
            self.current.terminator = IrTerminator::Jump(cond_id);
        }
        self.finish_current();

        self.start_block(after_id, "while_after");
        Ok(())
    }

    fn lower_for(&mut self, name: &str, iterable: IrExpr, body: &[Stmt]) -> KuResult<()> {
        let iter_id = self.current.id;
        let body_id = self.next_block("for_body");
        let after_id = self.next_block("for_after");
        self.current.terminator = IrTerminator::ForEach {
            name: name.to_string(),
            iterable,
            body_block: body_id,
            after_block: after_id,
        };
        self.finish_current();

        let previous = self.locals.insert(name.to_string(), IrType::Unknown);
        self.start_block(body_id, "for_body");
        for stmt in body {
            self.lower_stmt(stmt)?;
            if self.current.terminator != IrTerminator::Next {
                break;
            }
        }
        if self.current.terminator == IrTerminator::Next {
            self.current.terminator = IrTerminator::Jump(iter_id);
        }
        self.finish_current();
        match previous {
            Some(ty) => {
                self.locals.insert(name.to_string(), ty);
            }
            None => {
                self.locals.remove(name);
            }
        }

        self.start_block(after_id, "for_after");
        Ok(())
    }

    fn lower_try(
        &mut self,
        body: &[Stmt],
        catch_name: &Option<String>,
        catch_body: &[Stmt],
        finally_body: &[Stmt],
    ) -> KuResult<()> {
        let catch_id =
            (!catch_body.is_empty() || catch_name.is_some()).then(|| self.next_block("catch"));
        let finally_id = (!finally_body.is_empty()).then(|| self.next_block("finally"));
        let finally_err_id = (!finally_body.is_empty()).then(|| self.next_block("finally_err"));
        let finally_return_id =
            (!finally_body.is_empty()).then(|| self.next_block("finally_return"));
        let after_id = self.next_block("try_after");
        let error_block = catch_id.or(finally_err_id).unwrap_or(after_id);
        let error_name = format!("__ku_error_{}", after_id.0);
        let return_name =
            (self.return_type != IrType::Void).then(|| format!("__ku_return_{}", after_id.0));
        if let Some(name) = &return_name {
            self.locals.insert(name.clone(), self.return_type.clone());
            self.current.instructions.push(IrInst::Let {
                name: name.clone(),
                ty: self.return_type.clone(),
                value: zero_expr(self.return_type.clone()),
            });
        }
        self.current.instructions.push(IrInst::BeginTry {
            catch_block: catch_id,
            finally_block: finally_id,
            after_block: after_id,
        });
        self.try_handlers.push(IrTryHandler {
            error_block,
            error_name: error_name.clone(),
            return_block: finally_return_id,
            return_name: return_name.clone(),
        });
        for stmt in body {
            self.lower_stmt(stmt)?;
            if self.current.terminator != IrTerminator::Next {
                break;
            }
        }
        self.try_handlers.pop();
        if self.current.terminator == IrTerminator::Next {
            self.current.instructions.push(IrInst::EndTry);
            self.current.terminator = IrTerminator::Jump(finally_id.unwrap_or(after_id));
        }
        self.finish_current();

        if let Some(catch_id) = catch_id {
            self.start_block(catch_id, "catch");
            let previous = catch_name
                .as_ref()
                .map(|name| self.locals.insert(name.clone(), error_ir_type()));
            if let Some(name) = catch_name {
                self.current.instructions.push(IrInst::BindError {
                    name: name.clone(),
                    result: IrExpr {
                        kind: IrExprKind::Local(error_name.clone()),
                        ty: self
                            .locals
                            .get(&error_name)
                            .cloned()
                            .unwrap_or_else(|| IrType::Result(Box::new(IrType::Null))),
                    },
                });
            }
            if let Some(finally_err_id) = finally_err_id {
                self.try_handlers.push(IrTryHandler {
                    error_block: finally_err_id,
                    error_name: error_name.clone(),
                    return_block: finally_return_id,
                    return_name: return_name.clone(),
                });
            }
            for stmt in catch_body {
                self.lower_stmt(stmt)?;
                if self.current.terminator != IrTerminator::Next {
                    break;
                }
            }
            if finally_err_id.is_some() {
                self.try_handlers.pop();
            }
            if self.current.terminator == IrTerminator::Next {
                self.current.terminator = IrTerminator::Jump(finally_id.unwrap_or(after_id));
            }
            self.finish_current();
            if let Some(name) = catch_name {
                match previous.flatten() {
                    Some(ty) => {
                        self.locals.insert(name.clone(), ty);
                    }
                    None => {
                        self.locals.remove(name);
                    }
                }
            }
        }

        if let Some(finally_id) = finally_id {
            self.start_block(finally_id, "finally");
            for stmt in finally_body {
                self.lower_stmt(stmt)?;
                if self.current.terminator != IrTerminator::Next {
                    break;
                }
            }
            if self.current.terminator == IrTerminator::Next {
                self.current.terminator = IrTerminator::Jump(after_id);
            }
            self.finish_current();
        }

        if let Some(finally_err_id) = finally_err_id {
            self.start_block(finally_err_id, "finally_err");
            for stmt in finally_body {
                self.lower_stmt(stmt)?;
                if self.current.terminator != IrTerminator::Next {
                    break;
                }
            }
            if self.current.terminator == IrTerminator::Next {
                let result = IrExpr {
                    kind: IrExprKind::Local(error_name.clone()),
                    ty: self
                        .locals
                        .get(&error_name)
                        .cloned()
                        .unwrap_or_else(|| IrType::Result(Box::new(IrType::Null))),
                };
                self.current.terminator = self.err_terminator(result);
            }
            self.finish_current();
        }

        if let Some(finally_return_id) = finally_return_id {
            self.start_block(finally_return_id, "finally_return");
            for stmt in finally_body {
                self.lower_stmt(stmt)?;
                if self.current.terminator != IrTerminator::Next {
                    break;
                }
            }
            if self.current.terminator == IrTerminator::Next {
                let value = return_name.as_ref().map(|name| IrExpr {
                    kind: IrExprKind::Local(name.clone()),
                    ty: self.return_type.clone(),
                });
                self.current.terminator = self.return_terminator(value);
            }
            self.finish_current();
        }

        self.start_block(after_id, "try_after");
        Ok(())
    }

    fn lower_lvalue(&mut self, target: &AssignTarget) -> KuResult<IrLValue> {
        match target {
            AssignTarget::Variable(name) => Ok(IrLValue::Local(name.clone())),
            AssignTarget::Index { target, index } => Ok(IrLValue::Index {
                target: self.lower_expr(target)?,
                index: self.lower_expr(index)?,
            }),
            AssignTarget::Field { target, name } => Ok(IrLValue::Field {
                target: self.lower_lvalue_target_expr(target)?,
                name: name.clone(),
            }),
        }
    }

    fn lower_lvalue_cached(&mut self, target: &AssignTarget) -> KuResult<IrLValue> {
        match target {
            AssignTarget::Variable(name) => Ok(IrLValue::Local(name.clone())),
            AssignTarget::Index { target, index } => {
                let target = self.lower_expr(target)?;
                let target = self.emit_temp(target)?;
                let index = self.lower_expr(index)?;
                let index = self.emit_temp(index)?;
                Ok(IrLValue::Index { target, index })
            }
            AssignTarget::Field { target, name } => Ok(IrLValue::Field {
                target: self.lower_lvalue_target_expr(target)?,
                name: name.clone(),
            }),
        }
    }

    fn lvalue_read_expr(&self, target: &IrLValue) -> IrExpr {
        match target {
            IrLValue::Local(name) => IrExpr {
                kind: IrExprKind::Local(name.clone()),
                ty: self.locals.get(name).cloned().unwrap_or(IrType::Unknown),
            },
            IrLValue::Index { target, index } => IrExpr {
                ty: match &target.ty {
                    IrType::Array(element) => *element.clone(),
                    _ => IrType::Unknown,
                },
                kind: IrExprKind::Index {
                    target: Box::new(target.clone()),
                    index: Box::new(index.clone()),
                },
            },
            IrLValue::Field { target, name } => IrExpr {
                ty: self.field_type(&target.ty, name),
                kind: IrExprKind::Field {
                    target: Box::new(target.clone()),
                    name: name.clone(),
                },
            },
        }
    }

    /// Desugar a backtick template string into `lit + str(expr) + lit + ...`,
    /// mirroring the interpreter's `eval_template` exactly: literal text keeps every
    /// escape except `\{` -> `{` and `\}` -> `}`, and each `{expr}` becomes
    /// `str(<parsed expr>)` (native `str` == the interpreter's `value.to_string()`).
    /// Interpolations whose type `str()` can't render (e.g. a struct) fail loudly at
    /// build time rather than silently, upholding native==interpreter.
    fn lower_template_string(&mut self, raw: &str, span: Span) -> KuResult<IrExpr> {
        let mut parts: Vec<Expr> = Vec::new();
        let mut text = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some(next @ ('{' | '}')) => text.push(next),
                    Some(next) => {
                        text.push('\\');
                        text.push(next);
                    }
                    None => text.push('\\'),
                }
                continue;
            }
            if ch != '{' {
                text.push(ch);
                continue;
            }
            if !text.is_empty() {
                parts.push(Expr::new(
                    ExprKind::Literal(Literal::String(std::mem::take(&mut text))),
                    span,
                ));
            }
            let mut source = String::new();
            let mut found_end = false;
            while let Some(inner) = chars.next() {
                if inner == '\\' {
                    if let Some(next) = chars.next() {
                        source.push('\\');
                        source.push(next);
                    }
                    continue;
                }
                if inner == '}' {
                    found_end = true;
                    break;
                }
                source.push(inner);
            }
            if !found_end {
                return Err(KuError::runtime("unterminated template interpolation", span));
            }
            if source.trim().is_empty() {
                return Err(KuError::runtime("empty template interpolation", span));
            }
            let tokens = crate::lexer::Lexer::new(&source).tokenize()?;
            let expr = crate::parser::Parser::new(tokens).parse_expression_only()?;
            // `{expr}` -> `str(expr)` so the run-time value is stringified like the
            // interpreter's `to_string()`.
            parts.push(Expr::new(
                ExprKind::Call {
                    callee: Box::new(Expr::new(ExprKind::Variable("str".to_string()), span)),
                    args: vec![expr],
                },
                span,
            ));
        }
        if !text.is_empty() {
            parts.push(Expr::new(ExprKind::Literal(Literal::String(text)), span));
        }
        let mut iter = parts.into_iter();
        let mut acc = iter.next().unwrap_or_else(|| {
            Expr::new(ExprKind::Literal(Literal::String(String::new())), span)
        });
        for part in iter {
            acc = Expr::new(
                ExprKind::Binary {
                    left: Box::new(acc),
                    op: BinaryOp::Add,
                    right: Box::new(part),
                },
                span,
            );
        }
        self.lower_expr(&acc)
    }

    /// Lower the target of a value-position field read. A nested `Field` is built
    /// as an in-place projection (no `emit_temp`, so no whole-struct move); any
    /// other base (a variable — which may need a cell load — a call result, etc.)
    /// is lowered normally.
    fn lower_field_target(&mut self, expr: &Expr) -> KuResult<IrExpr> {
        if let ExprKind::Field { target, name } = &expr.kind {
            // Only build an in-place projection when the target is statically a
            // user struct — reading a field of a struct is a member access, whereas
            // a field of an object / string-map is a runtime lookup that must go
            // through the normal lowering. Peek the type without lowering to avoid
            // lowering the target twice.
            if matches!(self.static_place_type(target), Some(IrType::Named(ref n)) if self.layouts.structs.iter().any(|s| &s.name == n))
            {
                let base = self.lower_field_target(target)?;
                let ty = self.field_type(&base.ty, name);
                return Ok(IrExpr {
                    kind: IrExprKind::Field {
                        target: Box::new(base),
                        name: name.clone(),
                    },
                    ty,
                });
            }
        }
        self.lower_expr(expr)
    }

    /// The static type of a variable / struct-field-chain place, without emitting
    /// anything. `None` for anything that is not a plain projection.
    fn static_place_type(&self, expr: &Expr) -> Option<IrType> {
        match &expr.kind {
            ExprKind::Variable(name) => self.locals.get(name).cloned(),
            ExprKind::Field { target, name } => {
                let target_ty = self.static_place_type(target)?;
                Some(self.field_type(&target_ty, name))
            }
            _ => None,
        }
    }

    fn lower_lvalue_target_expr(&mut self, expr: &Expr) -> KuResult<IrExpr> {
        match &expr.kind {
            ExprKind::Variable(name) => Ok(IrExpr {
                kind: IrExprKind::Local(name.clone()),
                ty: self.locals.get(name).cloned().unwrap_or(IrType::Unknown),
            }),
            ExprKind::Field { target, name } => {
                let target = self.lower_lvalue_target_expr(target)?;
                let ty = self.field_type(&target.ty, name);
                Ok(IrExpr {
                    kind: IrExprKind::Field {
                        target: Box::new(target),
                        name: name.clone(),
                    },
                    ty,
                })
            }
            _ => self.lower_expr(expr),
        }
    }

    /// Lower a `fail` payload. An object literal `{domain, code, message}` becomes
    /// a native `__ku_error_make` intrinsic (a fixed three-KuString `KuError`),
    /// not a generic object — so Error stays representable without dynamic objects.
    /// Other payloads (a string message, or an existing Error value) lower normally.
    fn lower_error_expr(&mut self, value: &Expr) -> KuResult<IrExpr> {
        if let ExprKind::ObjectLiteral { fields } = &value.kind {
            let mut domain = None;
            let mut code = None;
            let mut message = None;
            for (name, expr) in fields {
                match name.as_str() {
                    "domain" => domain = Some(expr),
                    "code" => code = Some(expr),
                    "message" => message = Some(expr),
                    _ => {}
                }
            }
            if let (Some(domain), Some(code), Some(message)) = (domain, code, message) {
                if fields.len() == 3 {
                    let domain = self.lower_expr(domain)?;
                    let code = self.lower_expr(code)?;
                    let message = self.lower_expr(message)?;
                    return Ok(IrExpr {
                        kind: IrExprKind::Call {
                            callee: Box::new(IrExpr {
                                kind: IrExprKind::Local("__ku_error_make".to_string()),
                                ty: IrType::Function,
                            }),
                            args: vec![domain, code, message],
                            kind: IrCallKind::Intrinsic("__ku_error_make".to_string()),
                        },
                        ty: error_ir_type(),
                    });
                }
            }
        }
        self.lower_expr(value)
    }

    /// Stage 8a: lower a native HTTP server method call. `get/post/put/del`
    /// register an exact-path route (method + path + handler closure); `listen`
    /// runs the single-threaded accept loop. The handler is lowered with an
    /// expected `(req) -> _` function type so an unannotated `req` parameter is
    /// filled with the synthetic request struct type (matching how the checker
    /// types `req`); a `fn()` handler simply ignores the expected parameter.
    fn lower_http_server_method(
        &mut self,
        server: IrExpr,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> KuResult<IrExpr> {
        if method == "listen" {
            if args.len() != 1 {
                return Err(KuError::runtime(
                    format!("http service listen expects 1 argument but got {}", args.len()),
                    span,
                ));
            }
            let address = self.lower_expr(&args[0])?;
            return self.emit_temp(IrExpr {
                kind: IrExprKind::Call {
                    callee: Box::new(IrExpr {
                        kind: IrExprKind::Local("__ku_http_listen".to_string()),
                        ty: IrType::Function,
                    }),
                    args: vec![server, address],
                    kind: IrCallKind::Intrinsic("__ku_http_listen".to_string()),
                },
                ty: IrType::Result(Box::new(IrType::Null)),
            });
        }
        if args.len() != 2 {
            return Err(KuError::runtime(
                format!("http service {method} expects 2 arguments but got {}", args.len()),
                span,
            ));
        }
        let path = self.lower_expr(&args[0])?;
        let expected = IrType::Closure {
            params: vec![IrType::Named(HTTP_REQUEST_TYPE.to_string())],
            ret: Box::new(IrType::Unknown),
        };
        let handler = self.lower_expr_with_expected(&args[1], Some(&expected))?;
        let (arity, returns_result) = match &handler.ty {
            IrType::Closure { params, ret } => (
                params.len(),
                matches!(ret.as_ref(), IrType::Result(_)),
            ),
            _ => {
                return Err(KuError::runtime(
                    format!("http service {method} handler must be a function"),
                    args[1].span,
                ))
            }
        };
        let http_method = match method {
            "get" => "GET",
            "post" => "POST",
            "put" => "PUT",
            "del" => "DELETE",
            _ => "GET",
        };
        let intrinsic = format!(
            "__ku_http_route:{http_method}:{arity}:{}",
            if returns_result { 1 } else { 0 }
        );
        self.emit_temp(IrExpr {
            kind: IrExprKind::Call {
                callee: Box::new(IrExpr {
                    kind: IrExprKind::Local(intrinsic.clone()),
                    ty: IrType::Function,
                }),
                args: vec![server, path, handler],
                kind: IrCallKind::Intrinsic(intrinsic),
            },
            ty: IrType::Named(HTTP_SERVER_TYPE.to_string()),
        })
    }

    fn field_type(&self, target: &IrType, field_name: &str) -> IrType {
        let IrType::Named(struct_name) = target else {
            return IrType::Unknown;
        };
        if struct_name == "__ku_error_type" && matches!(field_name, "domain" | "code" | "message") {
            return IrType::Str;
        }
        if struct_name == HTTP_REQUEST_TYPE && matches!(field_name, "method" | "path" | "body") {
            return IrType::Str;
        }
        // Stage 8b: `req.params` / `req.query` / `req.headers` are dynamic string
        // maps backed by the native `KuObject` ABI. Typing them as `__ku_object`
        // lets `req.query.get_or(...)` dispatch and marks the program as using the
        // object runtime (see `program_uses_object`).
        if struct_name == HTTP_REQUEST_TYPE
            && matches!(field_name, "params" | "query" | "headers")
        {
            return IrType::Named("__ku_object".to_string());
        }
        self.layouts
            .structs
            .iter()
            .find(|layout| layout.name == *struct_name)
            .and_then(|layout| layout.fields.iter().find(|field| field.name == field_name))
            .map(|field| field.ty.clone())
            .unwrap_or(IrType::Unknown)
    }

    fn lower_expr(&mut self, expr: &Expr) -> KuResult<IrExpr> {
        match &expr.kind {
            // A backtick template must be desugared, not emitted as a literal: the
            // interpreter interpolates `{expr}` at run time, so native must too.
            ExprKind::Literal(Literal::TemplateString(raw)) => {
                self.lower_template_string(raw, expr.span)
            }
            ExprKind::Literal(literal) => Ok(IrExpr {
                kind: IrExprKind::Literal(literal_text(literal)),
                ty: literal_type(literal),
            }),
            ExprKind::Variable(name) => {
                if let Some(value) = self.pattern_bindings.get(name) {
                    return Ok(value.clone());
                }
                // Stage 6b: a captured cell read inside a closure body.
                if let Some(IrType::Cell(inner)) = self.captures.get(name) {
                    let inner = (**inner).clone();
                    return Ok(self.cell_load(IrExprKind::CapturedCell(name.clone()), inner));
                }
                // Stage 6b: a boxed local read in the scope that owns the cell.
                if let Some(IrType::Cell(inner)) = self.locals.get(name) {
                    let inner = (**inner).clone();
                    return Ok(self.cell_load(IrExprKind::Local(name.clone()), inner));
                }
                // A top-level function name used as a value (not as a direct call
                // callee — those are intercepted in Call lowering) lowers to a
                // closure over that function via its `__thunk` (Stage 6a).
                if !self.locals.contains_key(name) {
                    if let Some(signature) = self.signatures.get(name) {
                        let params = signature.params.clone();
                        let ret = Box::new(signature.returns.clone());
                        return Ok(IrExpr {
                            kind: IrExprKind::MakeClosure {
                                function_id: signature.id,
                                captures: Vec::new(),
                            },
                            ty: IrType::Closure { params, ret },
                        });
                    }
                }
                Ok(IrExpr {
                    kind: IrExprKind::Local(name.clone()),
                    ty: self.locals.get(name).cloned().unwrap_or(IrType::Unknown),
                })
            }
            ExprKind::Unary { op, expr } => {
                let expr = self.lower_expr(expr)?;
                let ty = match op {
                    UnaryOp::Negate => expr.ty.clone(),
                    UnaryOp::Not => IrType::Bool,
                };
                self.emit_temp(IrExpr {
                    kind: IrExprKind::Unary {
                        op: *op,
                        expr: Box::new(expr),
                    },
                    ty,
                })
            }
            ExprKind::Binary { left, op, right } => {
                let left = self.lower_expr(left)?;
                let right = self.lower_expr(right)?;
                let ty = binary_type(*op, &left.ty, &right.ty);
                self.emit_temp(IrExpr {
                    kind: IrExprKind::Binary {
                        left: Box::new(left),
                        op: *op,
                        right: Box::new(right),
                    },
                    ty,
                })
            }
            ExprKind::Call { callee, args } => {
                // Stage 6f: a self-recursive call inside a lifted local-named
                // function body. Resolve the callee to the lifted function itself
                // and thread the running `__env` as the leading argument, instead
                // of capturing the function into its own env (which would form a
                // reference cycle). Mirrors the interpreter's `self_name` binding.
                if let ExprKind::Variable(name) = &callee.kind {
                    if let Some((self_id, ret_ty)) = self.self_recurse_target(name) {
                        let mut lowered_args = Vec::with_capacity(args.len() + 1);
                        lowered_args.push(IrExpr {
                            kind: IrExprKind::Local("__env".to_string()),
                            ty: IrType::Unknown,
                        });
                        for arg in args {
                            lowered_args.push(self.lower_expr(arg)?);
                        }
                        return self.emit_temp(IrExpr {
                            kind: IrExprKind::Call {
                                callee: Box::new(IrExpr {
                                    kind: IrExprKind::Local(format!("__ku_closure_{}", self_id.0)),
                                    ty: IrType::Function,
                                }),
                                args: lowered_args,
                                kind: IrCallKind::Direct(self_id),
                            },
                            ty: ret_ty,
                        });
                    }
                }
                if let ExprKind::Field { target, name } = &callee.kind {
                    if name == "clone" && args.is_empty() {
                        // Read the receiver in place: `u.name.clone()` must clone
                        // the field without first moving (clearing) it. For a
                        // non-field receiver this is identical to `lower_expr`.
                        let target = self.lower_field_target(target)?;
                        let ty = target.ty.clone();
                        return self.emit_temp(IrExpr {
                            kind: IrExprKind::Call {
                                callee: Box::new(IrExpr {
                                    kind: IrExprKind::Local("__ku_clone".to_string()),
                                    ty: IrType::Function,
                                }),
                                args: vec![target],
                                kind: IrCallKind::Intrinsic("__ku_clone".to_string()),
                            },
                            ty,
                        });
                    }
                }
                // Stage 6f: `<array>.map(closure)`. Handled ahead of the generic
                // argument lowering so the mapper closure's *unannotated* parameter
                // is filled from the array's element type — the checker infers it
                // (so the interpreter accepts `map(x => x*2)`), and native must too
                // (rule 8). The whole call's IR type is `Array(<closure return
                // type>)` rather than the unknown a fall-through would leave it. The
                // checker guarantees any `.map(<1 arg>)` has an array receiver, so a
                // non-array receiver here is unreachable for a checked program.
                if let ExprKind::Field { target, name } = &callee.kind {
                    if name == "map" && args.len() == 1 {
                        let receiver = self.lower_expr(target)?;
                        let IrType::Array(element) = receiver.ty.clone() else {
                            return Err(KuError::runtime(
                                "array.map requires an array receiver",
                                expr.span,
                            ));
                        };
                        // Propagate the element type as the mapper's expected
                        // parameter type; the return type is recovered from the
                        // closure body (no body-driven inference of the parameter).
                        let expected = IrType::Closure {
                            params: vec![*element],
                            ret: Box::new(IrType::Unknown),
                        };
                        let mapper = self.lower_expr_with_expected(&args[0], Some(&expected))?;
                        let ret = match &mapper.ty {
                            IrType::Closure { ret, .. } => (**ret).clone(),
                            _ => IrType::Unknown,
                        };
                        return self.emit_temp(IrExpr {
                            kind: IrExprKind::Call {
                                callee: Box::new(IrExpr {
                                    kind: IrExprKind::Local("array.map".to_string()),
                                    ty: IrType::Function,
                                }),
                                args: vec![receiver, mapper],
                                kind: IrCallKind::Intrinsic("array.map".to_string()),
                            },
                            ty: IrType::Array(Box::new(ret)),
                        });
                    }
                }
                // Stage 8a: native HTTP server lifecycle. `app.get/post/put/del`
                // register a route; `app.listen` runs the accept loop. Detected by
                // the receiver lowering to the synthetic `__ku_http_server` type
                // (see `call_kind_and_type`), so a user object with a `.get` method
                // is never mistaken for a server.
                if let ExprKind::Field { target, name } = &callee.kind {
                    if matches!(
                        name.as_str(),
                        "get" | "post" | "put" | "del" | "listen"
                    ) && is_pure_path(target)
                    {
                        let receiver = self.lower_expr(target)?;
                        if matches!(&receiver.ty, IrType::Named(n) if n == HTTP_SERVER_TYPE) {
                            return self.lower_http_server_method(receiver, name, args, expr.span);
                        }
                    }
                }
                // For a direct call to a known top-level function, thread each
                // parameter's type into the argument so a closure argument's
                // unannotated parameters are filled from the expected function
                // type (e.g. `Apply((x) => x + 1, 41)`).
                let expected_param_types = match &callee.kind {
                    ExprKind::Variable(name) => {
                        self.signatures.get(name).map(|sig| sig.params.clone())
                    }
                    _ => None,
                };
                let mut lowered_args = args
                    .iter()
                    .enumerate()
                    .map(|(index, arg)| {
                        let expected = expected_param_types
                            .as_ref()
                            .and_then(|params| params.get(index));
                        self.lower_expr_with_expected(arg, expected)
                    })
                    .collect::<KuResult<Vec<_>>>()?;
                if let Some((layout, variant)) = self.enum_variant(callee) {
                    let fields = variant
                        .fields
                        .iter()
                        .zip(lowered_args)
                        .map(|(field, value)| (field.name.clone(), value))
                        .collect::<Vec<_>>();
                    let field_names = fields
                        .iter()
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    return self.emit_temp(IrExpr {
                        kind: IrExprKind::Call {
                            callee: Box::new(IrExpr {
                                kind: IrExprKind::Local(format!(
                                    "{enum_name}.{variant_name}",
                                    enum_name = layout.name,
                                    variant_name = variant.name
                                )),
                                ty: IrType::Function,
                            }),
                            args: fields.into_iter().map(|(_, value)| value).collect(),
                            kind: IrCallKind::Intrinsic(format!(
                                "__ku_enum:{}:{}:{}:{field_names}",
                                layout.name, variant.name, variant.tag
                            )),
                        },
                        ty: enum_ir_type(&layout.name),
                    });
                }
                // Receiver-typed builtin method dispatch: `<array>.push(x)` /
                // `<array>.len()`. Resolve by the lowered receiver's type so it
                // becomes a typed `array.*` intrinsic instead of an unknown-typed
                // indirect call. Restricted to pure-path receivers so the
                // fall-through never re-lowers a side-effecting expression.
                if let ExprKind::Field { target, name } = &callee.kind {
                    // KuValue converters (as_int/as_str) dispatch even when the
                    // receiver is a `?` expression (not a pure path); the checker
                    // guarantees the receiver is a KuValue so it always matches.
                    let force_kuvalue = matches!(name.as_str(), "as_int" | "as_str");
                    if is_pure_path(target) || force_kuvalue {
                        let receiver = self.lower_expr(target)?;
                        let module = match &receiver.ty {
                            IrType::Array(_) => Some("array"),
                            IrType::Str => Some("string"),
                            IrType::Named(n) if n == "__ku_object" => Some("object"),
                            IrType::Named(n) if n == "__ku_value" => Some("kuvalue"),
                            _ => None,
                        };
                        if let Some(module) = module {
                            if let Some(signature) = metadata::dotted_signature(module, name) {
                                // `array.push` CLONES its value into the new array
                                // (matching the interpreter, which leaves the source
                                // usable). A fresh owned rvalue — e.g. a struct literal
                                // — is otherwise never dropped and leaks its owned
                                // fields, so materialize it into a temp that cleanup
                                // frees. A place (variable/field) is left alone: it is
                                // borrowed, and its own binding still owns it.
                                if module == "array" && name == "push" {
                                    let needs_temp = lowered_args.first().is_some_and(|value| {
                                        ir_type_is_owned(&value.ty) && !ir_expr_is_place(value)
                                    });
                                    if needs_temp {
                                        let value = lowered_args[0].clone();
                                        lowered_args[0] = self.emit_temp(value)?;
                                    }
                                }
                                let mut all_args = Vec::with_capacity(lowered_args.len() + 1);
                                all_args.push(receiver);
                                all_args.extend(lowered_args.iter().cloned());
                                // Dynamic-object methods (`get_or`) yield a KuValue.
                                let ty = if module == "object" {
                                    IrType::Named("__ku_value".to_string())
                                } else {
                                    signature_return_type(&signature, &all_args)
                                };
                                return self.emit_temp(IrExpr {
                                    kind: IrExprKind::Call {
                                        callee: Box::new(IrExpr {
                                            kind: IrExprKind::Local(format!("{module}.{name}")),
                                            ty: IrType::Function,
                                        }),
                                        args: all_args,
                                        kind: IrCallKind::Intrinsic(format!("{module}.{name}")),
                                    },
                                    ty,
                                });
                            }
                        }
                    }
                }
                let (kind, mut ty) = call_kind_and_type(callee, &lowered_args, self.signatures);
                // For intrinsics the backend dispatches by name and ignores the
                // callee, so avoid lowering a dotted callee (e.g. `time.millis`)
                // into an unknown-typed Field temp — use a Function placeholder.
                // Direct calls likewise keep the bare function name as a
                // placeholder: lowering the callee would turn a top-level
                // function name into a MakeClosure and break the direct call.
                let callee = match &kind {
                    IrCallKind::Intrinsic(intrinsic) => IrExpr {
                        kind: IrExprKind::Local(intrinsic.clone()),
                        ty: IrType::Function,
                    },
                    IrCallKind::Direct(_) => {
                        let name = match &callee.kind {
                            ExprKind::Variable(name) => name.clone(),
                            _ => {
                                return Err(KuError::runtime(
                                    "native direct call requires a function name",
                                    expr.span,
                                ))
                            }
                        };
                        IrExpr {
                            kind: IrExprKind::Local(name),
                            ty: IrType::Function,
                        }
                    }
                    IrCallKind::Indirect => {
                        let lowered = self.lower_expr(callee)?;
                        // Calling a closure value: the call yields the closure's
                        // return type (otherwise it stays Unknown).
                        if let IrType::Closure { ret, .. } = &lowered.ty {
                            ty = (**ret).clone();
                        }
                        lowered
                    }
                };
                self.emit_temp(IrExpr {
                    kind: IrExprKind::Call {
                        callee: Box::new(callee),
                        args: lowered_args,
                        kind,
                    },
                    ty,
                })
            }
            ExprKind::Array(values) => {
                let values = values
                    .iter()
                    .map(|value| self.lower_expr(value))
                    .collect::<KuResult<Vec<_>>>()?;
                let element = values
                    .first()
                    .map(|value| value.ty.clone())
                    .unwrap_or(IrType::Unknown);
                self.emit_temp(IrExpr {
                    kind: IrExprKind::Array(values),
                    ty: IrType::Array(Box::new(element)),
                })
            }
            ExprKind::Index { target, index } => {
                let target = self.lower_expr(target)?;
                let index = self.lower_expr(index)?;
                // `obj[key]` on a dynamic object yields Result<KuValue>: Ok(value)
                // when present, Err{object, missing_key} when absent. `?` unwraps
                // it to a KuValue.
                if let IrType::Named(name) = &target.ty {
                    if name == "__ku_object" || name == "__ku_value" {
                        // `obj[key]?` on a KuObject or a KuValue (e.g. json.parse
                        // result) yields Result<KuValue>. The KuValue variant checks
                        // the tag is an object at runtime.
                        let intrinsic = if name == "__ku_value" {
                            // A KuValue index dispatches by key type: an int key
                            // reads an array element (__ku_value_index), a str key
                            // reads an object member (__ku_value_get).
                            if matches!(&index.ty, IrType::Int) {
                                "__ku_value_index"
                            } else {
                                "__ku_value_get"
                            }
                        } else {
                            "__ku_object_get"
                        };
                        return self.emit_temp(IrExpr {
                            kind: IrExprKind::Call {
                                callee: Box::new(IrExpr {
                                    kind: IrExprKind::Local(intrinsic.to_string()),
                                    ty: IrType::Function,
                                }),
                                args: vec![target, index],
                                kind: IrCallKind::Intrinsic(intrinsic.to_string()),
                            },
                            ty: IrType::Result(Box::new(IrType::Named("__ku_value".to_string()))),
                        });
                    }
                }
                let ty = match &target.ty {
                    IrType::Array(inner) => *inner.clone(),
                    _ => IrType::Unknown,
                };
                self.emit_temp(IrExpr {
                    kind: IrExprKind::Index {
                        target: Box::new(target),
                        index: Box::new(index),
                    },
                    ty,
                })
            }
            ExprKind::Field { target, name } => {
                if let ExprKind::Variable(enum_name) = &target.kind {
                    if let Some(layout) = self
                        .layouts
                        .enums
                        .iter()
                        .find(|layout| layout.name == *enum_name)
                    {
                        if let Some(variant) = layout
                            .variants
                            .iter()
                            .find(|variant| variant.name == *name && variant.fields.is_empty())
                        {
                            return self.emit_temp(IrExpr {
                                kind: IrExprKind::Call {
                                    callee: Box::new(IrExpr {
                                        kind: IrExprKind::Local(format!(
                                            "{}.{}",
                                            layout.name, variant.name
                                        )),
                                        ty: IrType::Function,
                                    }),
                                    args: Vec::new(),
                                    kind: IrCallKind::Intrinsic(format!(
                                        "__ku_enum:{}:{}:{}:",
                                        layout.name, variant.name, variant.tag
                                    )),
                                },
                                ty: enum_ir_type(&layout.name),
                            });
                        }
                    }
                }
                // Stage 8b: `req.params.<key>` / `req.query.<key>` / `req.headers.<key>`
                // read a string out of the request's dynamic map (KuObject). It lowers
                // to a `__ku_http_map_get` intrinsic that returns an owned KuString
                // (empty when the key is absent), matching the interpreter's
                // StringMap `.field` -> str access on `req.params`/`query`/`headers`.
                if let ExprKind::Field {
                    target: inner,
                    name: map_name,
                } = &target.kind
                {
                    if matches!(map_name.as_str(), "params" | "query" | "headers")
                        && is_pure_path(inner)
                    {
                        let inner_lowered = self.lower_expr(inner)?;
                        if matches!(&inner_lowered.ty, IrType::Named(n) if n == HTTP_REQUEST_TYPE) {
                            let map_obj = IrExpr {
                                kind: IrExprKind::Field {
                                    target: Box::new(inner_lowered),
                                    name: map_name.clone(),
                                },
                                ty: IrType::Named("__ku_object".to_string()),
                            };
                            let key = IrExpr {
                                kind: IrExprKind::Literal(format!("{name:?}")),
                                ty: IrType::Str,
                            };
                            return self.emit_temp(IrExpr {
                                kind: IrExprKind::Call {
                                    callee: Box::new(IrExpr {
                                        kind: IrExprKind::Local("__ku_http_map_get".to_string()),
                                        ty: IrType::Function,
                                    }),
                                    args: vec![map_obj, key],
                                    kind: IrCallKind::Intrinsic("__ku_http_map_get".to_string()),
                                },
                                ty: IrType::Str,
                            });
                        }
                    }
                }
                // Lower the target in place: a nested struct-field read
                // (`c.user.name`) must NOT materialize the intermediate struct
                // into a temp, because that moves the whole struct — and its
                // sibling fields — out. `lower_field_target` builds the projection
                // chain without an intervening move.
                let target = self.lower_field_target(target)?;
                let ty = self.field_type(&target.ty, name);
                let field = IrExpr {
                    kind: IrExprKind::Field {
                        target: Box::new(target),
                        name: name.clone(),
                    },
                    ty: ty.clone(),
                };
                if ir_type_is_owned(&ty) {
                    // A value-position field read is a BORROW: it must leave the
                    // struct intact for later reads and for the struct's own drop.
                    // Cloning yields an independent value that satisfies both. A
                    // genuine consuming move could clear the field instead (more
                    // efficient), but the IR cannot tell a read from a move here
                    // without the checker's per-occurrence move info (the deferred
                    // BorrowPlace/MovePlace/ClonePlace) — and moving a read is the
                    // one unsafe direction (it empties a field the source still
                    // owns), so the conservative clone is always correct.
                    self.emit_temp(IrExpr {
                        kind: IrExprKind::Call {
                            callee: Box::new(IrExpr {
                                kind: IrExprKind::Local("__ku_clone".to_string()),
                                ty: IrType::Function,
                            }),
                            args: vec![field],
                            kind: IrCallKind::Intrinsic("__ku_clone".to_string()),
                        },
                        ty,
                    })
                } else {
                    self.emit_temp(field)
                }
            }
            ExprKind::OptionalField { .. } => Err(KuError::runtime(
                "optional chaining is not supported by IR/native lowering yet",
                expr.span,
            )),
            ExprKind::Await(_) => Err(KuError::runtime(
                "async/await is not supported by IR/native lowering yet",
                expr.span,
            )),
            ExprKind::TryUnwrap { expr } => {
                let expr = self.lower_expr(expr)?;
                let ty = match &expr.ty {
                    IrType::Result(inner) => *inner.clone(),
                    _ => IrType::Unknown,
                };
                self.emit_try_unwrap(expr, ty)
            }
            ExprKind::StructLiteral { name, fields } => {
                // A struct field whose declared type is a function type supplies
                // the expected function type for a closure written in that field.
                let field_types = self
                    .layouts
                    .structs
                    .iter()
                    .find(|layout| layout.name == *name)
                    .map(|layout| {
                        layout
                            .fields
                            .iter()
                            .map(|field| (field.name.clone(), field.ty.clone()))
                            .collect::<HashMap<_, _>>()
                    })
                    .unwrap_or_default();
                let fields = fields
                    .iter()
                    .map(|(field, value)| {
                        let expected = field_types.get(field);
                        Ok((field.clone(), self.lower_expr_with_expected(value, expected)?))
                    })
                    .collect::<KuResult<Vec<_>>>()?;
                Ok(IrExpr {
                    kind: IrExprKind::StructLiteral {
                        name: name.clone(),
                        fields,
                    },
                    ty: IrType::Named(name.clone()),
                })
            }
            ExprKind::ObjectLiteral { fields } => {
                // Lower to a `__ku_object` intrinsic carrying [key, value, ...].
                // The backend builds a runtime open-addressing hash (KuObject*),
                // wrapping each value into a tagged KuValue by its IR type.
                let mut args = Vec::with_capacity(fields.len() * 2);
                for (name, value) in fields {
                    args.push(IrExpr {
                        kind: IrExprKind::Literal(format!("{name:?}")),
                        ty: IrType::Str,
                    });
                    args.push(self.lower_expr(value)?);
                }
                self.emit_temp(IrExpr {
                    kind: IrExprKind::Call {
                        callee: Box::new(IrExpr {
                            kind: IrExprKind::Local("__ku_object".to_string()),
                            ty: IrType::Function,
                        }),
                        args,
                        kind: IrCallKind::Intrinsic("__ku_object".to_string()),
                    },
                    ty: IrType::Named("__ku_object".to_string()),
                })
            }
            ExprKind::Match { value, arms } => self.lower_match(value, arms, expr.span),
            ExprKind::Function {
                params,
                return_type: _,
                body,
            } => self.lower_closure_literal(params, body, expr.span, None),
        }
    }

    /// Like [`lower_expr`], but threads an expected type from context into a
    /// closure literal so an unannotated parameter can be filled from the
    /// expected function type. Mirrors the checker's `check_expr_expecting` so
    /// every closure the checker accepts also lowers to native code (rule 8).
    fn lower_expr_with_expected(
        &mut self,
        expr: &Expr,
        expected: Option<&IrType>,
    ) -> KuResult<IrExpr> {
        if let ExprKind::Function { params, body, .. } = &expr.kind {
            let expected_params = match expected {
                Some(IrType::Closure { params, .. }) => Some(params.as_slice()),
                _ => None,
            };
            return self.lower_closure_literal(params, body, expr.span, expected_params);
        }
        self.lower_expr(expr)
    }

    /// Lower a closure literal into a lifted, globally-unique IrFunction plus a
    /// `MakeClosure` value (Stage 6a: no captures, env is NULL). An unannotated
    /// parameter is filled from `expected` — the parameter types of the expected
    /// function type supplied by context — matching the checker's rules; with no
    /// annotation and no expected type it is an error (the checker rejects this
    /// first, so reaching here means a missing context propagation).
    fn lower_closure_literal(
        &mut self,
        params: &[crate::ast::FunctionParam],
        body: &[Stmt],
        span: Span,
        expected: Option<&[IrType]>,
    ) -> KuResult<IrExpr> {
        let mut ir_params = Vec::with_capacity(params.len());
        for (index, param) in params.iter().enumerate() {
            let ty = if param.ty.is_some() {
                lower_optional_type(&param.ty, self.layouts)
            } else if let Some(expected_ty) = expected.and_then(|expected| expected.get(index)) {
                // Filled from the expected function type from context (a typed
                // binding, a higher-order parameter, or an API signature).
                // Inferring it from how the body uses the parameter is not done.
                expected_ty.clone()
            } else {
                // The checker rejects the no-context case, so reaching here means
                // the annotation was neither present nor supplied by context;
                // fail loudly rather than guess.
                return Err(KuError::runtime(
                    "closure parameter needs a type annotation or an expected function type from context",
                    span,
                ));
            };
            ir_params.push(IrParam {
                name: param.name.clone(),
                ty,
            });
        }

        // Stage 6b: the cells this closure captures = its free variables that
        // are boxed cells in the enclosing scope (a boxed local, or — for nested
        // closures — a cell already captured here). Sorted for a stable env-field
        // and argument order shared by the body and every `MakeClosure`.
        let mut capture_names = crate::runtime::interpreter::closure_capture_names(params, body)
            .into_iter()
            .filter(|name| {
                matches!(self.locals.get(name), Some(IrType::Cell(_)))
                    || matches!(self.captures.get(name), Some(IrType::Cell(_)))
            })
            .collect::<Vec<_>>();
        capture_names.sort();
        let captures = capture_names
            .into_iter()
            .map(|name| {
                let ty = self
                    .locals
                    .get(&name)
                    .or_else(|| self.captures.get(&name))
                    .cloned()
                    .unwrap_or(IrType::Cell(Box::new(IrType::Unknown)));
                (name, ty)
            })
            .collect::<Vec<_>>();

        let mut child = FunctionLowerer::new(
            self.signatures,
            self.layouts,
            IrType::Unknown,
            self.next_function_id.clone(),
            self.lifted_functions.clone(),
        );
        child.captures = captures.iter().cloned().collect();
        for param in &ir_params {
            child.locals.insert(param.name.clone(), param.ty.clone());
        }
        child.lower_block_body("entry", body, span)?;

        // Recover the real return type from the first `return <value>` the body
        // produced (6a closure bodies contain no `?`, so the Unknown seed above
        // never leaks into a Return terminator).
        let return_type = child
            .blocks
            .iter()
            .find_map(|block| match &block.terminator {
                IrTerminator::Return(Some(value)) => Some(value.ty.clone()),
                _ => None,
            })
            .unwrap_or(IrType::Null);

        let cid = FunctionId(self.next_function_id.get());
        self.next_function_id.set(cid.0 + 1);
        let param_types = ir_params.iter().map(|param| param.ty.clone()).collect();
        self.lifted_functions.borrow_mut().push(IrFunction {
            id: cid,
            name: format!("__ku_closure_{}", cid.0),
            params: ir_params,
            return_type: return_type.clone(),
            blocks: child.blocks,
            is_closure_body: true,
            captures: captures.clone(),
        });

        self.emit_temp(IrExpr {
            kind: IrExprKind::MakeClosure {
                function_id: cid,
                captures,
            },
            ty: IrType::Closure {
                params: param_types,
                ret: Box::new(return_type),
            },
        })
    }

    /// Stage 6f: lower a local named function (`fn name(...) {...}` defined inside
    /// another function body) by reusing the closure-literal machinery. The body
    /// is lifted into a globally-unique `__ku_closure_{id}` function; the name is
    /// bound in the enclosing scope to a `MakeClosure` closure value so it can be
    /// called, passed as an argument, or stored (parity with the interpreter).
    ///
    /// Captures use `function_capture_names`, which excludes the function's own
    /// name, so the function never captures itself. Self-recursive calls in the
    /// body instead go straight to the lifted function with the running `__env`
    /// (see `self_recurse`), keeping the env free of a self-reference — no RC
    /// cycle, no leak.
    fn lower_local_function(&mut self, function: &crate::ast::FnDecl) -> KuResult<()> {
        let name = function.name.clone();
        // Allocate the lifted function id first so the body can reference it for
        // self-recursive calls while it is still being lowered.
        let cid = FunctionId(self.next_function_id.get());
        self.next_function_id.set(cid.0 + 1);

        // Parameters must be annotated, exactly like a top-level function (rule 1).
        let mut ir_params = Vec::with_capacity(function.params.len());
        for param in &function.params {
            if param.ty.is_none() {
                return Err(KuError::runtime(
                    "local function parameter needs a type annotation for native lowering",
                    param.span,
                ));
            }
            ir_params.push(IrParam {
                name: param.name.clone(),
                ty: lower_optional_type(&param.ty, self.layouts),
            });
        }
        let param_types = ir_params
            .iter()
            .map(|param| param.ty.clone())
            .collect::<Vec<_>>();
        // A declared return type is authoritative (and available up front, unlike
        // a closure literal's recovered type); default to Null when omitted.
        let return_type = function
            .return_type
            .as_ref()
            .map(|ty| lower_type(ty, self.layouts))
            .unwrap_or(IrType::Null);

        // Captures = the function's free variables (its own name excluded) that
        // are boxed cells in the enclosing scope. Sorted for a stable env layout,
        // matching `lower_closure_literal`.
        let mut capture_names = crate::runtime::interpreter::function_capture_names(function)
            .into_iter()
            .filter(|name| {
                matches!(self.locals.get(name), Some(IrType::Cell(_)))
                    || matches!(self.captures.get(name), Some(IrType::Cell(_)))
            })
            .collect::<Vec<_>>();
        capture_names.sort();
        let captures = capture_names
            .into_iter()
            .map(|name| {
                let ty = self
                    .locals
                    .get(&name)
                    .or_else(|| self.captures.get(&name))
                    .cloned()
                    .unwrap_or(IrType::Cell(Box::new(IrType::Unknown)));
                (name, ty)
            })
            .collect::<Vec<_>>();

        let mut child = FunctionLowerer::new(
            self.signatures,
            self.layouts,
            return_type.clone(),
            self.next_function_id.clone(),
            self.lifted_functions.clone(),
        );
        child.captures = captures.iter().cloned().collect();
        for param in &ir_params {
            child.locals.insert(param.name.clone(), param.ty.clone());
        }
        // Wire self-recursion: a call to `name` in the body reuses the running env.
        child.self_recurse = Some((name.clone(), cid, return_type.clone()));
        child.lower_block_body("entry", &function.body, function.span)?;

        self.lifted_functions.borrow_mut().push(IrFunction {
            id: cid,
            name: format!("__ku_closure_{}", cid.0),
            params: ir_params,
            return_type: return_type.clone(),
            blocks: child.blocks,
            is_closure_body: true,
            captures: captures.clone(),
        });

        // Bind the name in the enclosing scope as a first-class closure value.
        let closure_ty = IrType::Closure {
            params: param_types,
            ret: Box::new(return_type),
        };
        let value = self.emit_temp(IrExpr {
            kind: IrExprKind::MakeClosure {
                function_id: cid,
                captures,
            },
            ty: closure_ty.clone(),
        })?;
        self.locals.insert(name.clone(), closure_ty.clone());
        self.current.instructions.push(IrInst::Let {
            name,
            ty: closure_ty,
            value,
        });
        Ok(())
    }

    /// Stage 6f: when `name` is the enclosing local function's own name inside its
    /// lifted body, return the lifted function id and return type so the call site
    /// can emit a direct self-recursive call.
    fn self_recurse_target(&self, name: &str) -> Option<(FunctionId, IrType)> {
        self.self_recurse
            .as_ref()
            .filter(|(self_name, _, _)| self_name == name)
            .map(|(_, id, ret)| (*id, ret.clone()))
    }

    fn emit_temp(&mut self, value: IrExpr) -> KuResult<IrExpr> {
        // A void value (a call to a function with no return) has no result to
        // bind — binding it would emit invalid `void t0 = f(...)`. Hand the value
        // straight back; its only consumer is a statement position, which emits it
        // as `f(...);`.
        if value.ty == IrType::Void {
            return Ok(value);
        }
        let id = TempId(self.next_temp_id);
        self.next_temp_id += 1;
        let ty = value.ty.clone();
        self.current.instructions.push(IrInst::Temp {
            id,
            ty: ty.clone(),
            value,
        });
        Ok(IrExpr {
            kind: IrExprKind::Temp(id),
            ty,
        })
    }

    /// Stage 6b: build a `CellLoad` over `pointer` (a `Local`/`CapturedCell`
    /// cell expression) whose payload is `inner`.
    fn cell_load(&self, pointer: IrExprKind, inner: IrType) -> IrExpr {
        IrExpr {
            ty: inner.clone(),
            kind: IrExprKind::CellLoad(Box::new(IrExpr {
                kind: pointer,
                ty: IrType::Cell(Box::new(inner)),
            })),
        }
    }

    /// Stage 6b: the `CapturedCell` pointer expression for a captured name.
    fn captured_cell_expr(&self, name: &str) -> IrExpr {
        let ty = self
            .captures
            .get(name)
            .cloned()
            .unwrap_or(IrType::Cell(Box::new(IrType::Unknown)));
        IrExpr {
            kind: IrExprKind::CapturedCell(name.to_string()),
            ty,
        }
    }

    /// Stage 6b: the payload type if `name` is a boxed local cell in this scope.
    fn boxed_local_inner(&self, name: &str) -> Option<IrType> {
        match self.locals.get(name) {
            Some(IrType::Cell(inner)) => Some((**inner).clone()),
            _ => None,
        }
    }

    /// Stage 6b: box a captured Copy local into a fresh cell (rc=1), recording
    /// its `Cell(inner)` type. Owned payloads are rejected (Stage 6c).
    fn push_cell_new(
        &mut self,
        name: String,
        inner: IrType,
        init: IrExpr,
        span: Span,
    ) -> KuResult<()> {
        // Stage 6c: str/array/object captures are boxed into a shared cell (the
        // cell owns the payload and drops it exactly once). Other owned payloads
        // (struct/enum/Result/function/KuValue) remain unsupported.
        let is_dynamic_object = matches!(&inner, IrType::Named(name) if name == "__ku_object");
        let supported = is_copy_ir_type(&inner)
            || inner == IrType::Str
            || matches!(&inner, IrType::Array(_))
            || is_dynamic_object;
        if !supported {
            return Err(KuError::runtime(
                format!("native closure capture of {inner} not supported yet (Stage 6c)"),
                span,
            ));
        }
        self.locals
            .insert(name.clone(), IrType::Cell(Box::new(inner.clone())));
        self.current.instructions.push(IrInst::CellNew {
            name,
            ty: inner,
            init,
        });
        Ok(())
    }

    fn enum_variant(&self, expr: &Expr) -> Option<(&IrEnumLayout, &IrVariantLayout)> {
        let ExprKind::Field { target, name } = &expr.kind else {
            return None;
        };
        let ExprKind::Variable(enum_name) = &target.kind else {
            return None;
        };
        let layout = self
            .layouts
            .enums
            .iter()
            .find(|layout| layout.name == *enum_name)?;
        let variant = layout
            .variants
            .iter()
            .find(|variant| variant.name == *name)?;
        Some((layout, variant))
    }

    fn lower_match(&mut self, value: &Expr, arms: &[MatchArm], span: Span) -> KuResult<IrExpr> {
        let subject = self.lower_expr(value)?;
        let origin = self.current.id;
        let result_name = format!("__ku_match_{}", self.next_temp_id);
        self.next_temp_id += 1;
        let after_id = self.next_block("match_after");
        let mut result_ty = None;

        for arm in arms {
            let arm_id = self.next_block("match_arm");
            let next_id = self.next_block("match_next");
            let mut bindings = HashMap::new();
            let condition =
                self.lower_match_pattern(&arm.pattern, subject.clone(), &mut bindings)?;
            self.current.terminator = IrTerminator::Branch {
                condition,
                then_block: arm_id,
                else_block: next_id,
            };
            self.finish_current();

            self.start_block(arm_id, "match_arm");
            let saved_bindings = std::mem::replace(&mut self.pattern_bindings, bindings);
            // Materialize each binding that is a computed projection (an enum
            // payload access, which moves-and-clears the slot when its type is
            // owned) into a single temp, so using the binding more than once reads
            // that temp instead of re-moving the value out of the enum each time.
            let binding_names: Vec<String> = self.pattern_bindings.keys().cloned().collect();
            for name in binding_names {
                let bound = self.pattern_bindings[&name].clone();
                if matches!(
                    bound.kind,
                    IrExprKind::Local(_) | IrExprKind::Temp(_) | IrExprKind::Literal(_)
                ) {
                    continue;
                }
                let temp = self.emit_temp(bound)?;
                self.pattern_bindings.insert(name, temp);
            }
            if let Some(guard) = &arm.guard {
                let guard = self.lower_expr(guard)?;
                let value_id = self.next_block("match_value");
                self.current.terminator = IrTerminator::Branch {
                    condition: guard,
                    then_block: value_id,
                    else_block: next_id,
                };
                self.finish_current();
                self.start_block(value_id, "match_value");
            }
            let arm_value = self.lower_expr(&arm.value)?;
            if let Some(expected) = &result_ty {
                if expected != &arm_value.ty {
                    self.pattern_bindings = saved_bindings;
                    return Err(KuError::runtime(
                        "match arm result types changed after checking",
                        arm.span,
                    ));
                }
            } else {
                result_ty = Some(arm_value.ty.clone());
            }
            // A void-result match (its arms are statements) has no value to store —
            // run the arm's expression for its side effects instead of storing it
            // into a (would-be `void`) result local.
            if arm_value.ty == IrType::Void {
                self.current.instructions.push(IrInst::Expr(arm_value));
            } else {
                self.current.instructions.push(IrInst::Store {
                    target: IrLValue::Local(result_name.clone()),
                    value: arm_value,
                });
            }
            if self.current.terminator == IrTerminator::Next {
                self.current.terminator = IrTerminator::Jump(after_id);
            }
            self.finish_current();
            self.pattern_bindings = saved_bindings;
            self.start_block(next_id, "match_next");
        }

        self.current.instructions.push(IrInst::Panic(IrExpr {
            kind: IrExprKind::Literal("\"match expression did not match any arm\"".to_string()),
            ty: IrType::Str,
        }));
        self.current.terminator = IrTerminator::Unreachable;
        self.finish_current();

        let ty =
            result_ty.ok_or_else(|| KuError::runtime("match requires at least one arm", span))?;
        // A void match produces no value, so it needs no result local.
        if ty == IrType::Void {
            self.start_block(after_id, "match_after");
            return Ok(IrExpr {
                kind: IrExprKind::Literal("0".to_string()),
                ty: IrType::Void,
            });
        }
        let origin_block = self
            .blocks
            .iter_mut()
            .find(|block| block.id == origin)
            .ok_or_else(|| KuError::runtime("missing match origin block", span))?;
        origin_block.instructions.push(IrInst::Let {
            name: result_name.clone(),
            ty: ty.clone(),
            value: zero_expr(ty.clone()),
        });
        self.start_block(after_id, "match_after");
        Ok(IrExpr {
            kind: IrExprKind::Local(result_name),
            ty,
        })
    }

    fn lower_match_pattern(
        &self,
        pattern: &MatchPattern,
        value: IrExpr,
        bindings: &mut HashMap<String, IrExpr>,
    ) -> KuResult<IrExpr> {
        match pattern {
            MatchPattern::Wildcard => Ok(bool_literal(true)),
            MatchPattern::Binding(name) => {
                bindings.insert(name.clone(), value);
                Ok(bool_literal(true))
            }
            MatchPattern::Literal(literal) => Ok(IrExpr {
                kind: IrExprKind::Binary {
                    left: Box::new(value),
                    op: BinaryOp::Equal,
                    right: Box::new(IrExpr {
                        kind: IrExprKind::Literal(literal_text(literal)),
                        ty: literal_type(literal),
                    }),
                },
                ty: IrType::Bool,
            }),
            MatchPattern::EnumVariant {
                enum_name,
                variant,
                fields,
            } => {
                let layout = self
                    .layouts
                    .enums
                    .iter()
                    .find(|layout| layout.name == *enum_name)
                    .ok_or_else(|| {
                        KuError::runtime(format!("undefined enum '{enum_name}'"), Span::default())
                    })?;
                let variant_layout = layout
                    .variants
                    .iter()
                    .find(|candidate| candidate.name == *variant)
                    .ok_or_else(|| {
                        KuError::runtime(
                            format!("enum '{enum_name}' has no variant '{variant}'"),
                            Span::default(),
                        )
                    })?;
                let mut condition = IrExpr {
                    kind: IrExprKind::Binary {
                        left: Box::new(intrinsic_expr(
                            "__ku_enum_tag",
                            vec![value.clone()],
                            IrType::Int,
                        )),
                        op: BinaryOp::Equal,
                        right: Box::new(IrExpr {
                            kind: IrExprKind::Literal(variant_layout.tag.to_string()),
                            ty: IrType::Int,
                        }),
                    },
                    ty: IrType::Bool,
                };
                for (pattern, field) in fields.iter().zip(&variant_layout.fields) {
                    let payload = intrinsic_expr(
                        format!("__ku_enum_payload:{variant}:{}", field.name),
                        vec![value.clone()],
                        field.ty.clone(),
                    );
                    let field_condition = self.lower_match_pattern(pattern, payload, bindings)?;
                    condition = and_expr(condition, field_condition);
                }
                Ok(condition)
            }
        }
    }

    fn emit_try_unwrap(&mut self, result: IrExpr, ty: IrType) -> KuResult<IrExpr> {
        let result = match result.kind {
            IrExprKind::Local(_) | IrExprKind::Temp(_) => result,
            _ => self.emit_temp(result)?,
        };
        let id = TempId(self.next_temp_id);
        self.next_temp_id += 1;
        let ok_block = self.next_block("try_ok");
        let err_block = self.next_block("try_err");
        self.current.terminator = IrTerminator::ResultBranch {
            result: result.clone(),
            ok_block,
            err_block,
        };
        self.finish_current();

        self.start_block(err_block, "try_err");
        self.current.terminator = self.err_terminator(result.clone());
        self.finish_current();

        self.start_block(ok_block, "try_ok");
        self.current.instructions.push(IrInst::BindOk {
            id,
            ty: ty.clone(),
            result,
        });
        Ok(IrExpr {
            kind: IrExprKind::Temp(id),
            ty,
        })
    }

    fn err_terminator(&mut self, result: IrExpr) -> IrTerminator {
        // The try error slot stores just the bare KuError — the part shared by
        // every Result type — so `?` operators unwrapping different Result types
        // inside one try block all target a single, consistently-typed slot
        // (otherwise the first `?`'s Result type would pin the slot and a later
        // `?` of another type would clash). `result` is either a failed Result
        // (take its `.error`) or an already-extracted error being re-propagated
        // out of a finally block (use it as-is).
        let error_ty = error_ir_type();
        let is_error = matches!(&result.ty, IrType::Named(name) if name == "__ku_error_type");
        let error_value = if is_error {
            result.clone()
        } else {
            IrExpr {
                kind: IrExprKind::Field {
                    target: Box::new(result.clone()),
                    name: "error".to_string(),
                },
                ty: error_ty.clone(),
            }
        };
        if let Some(handler) = self.try_handlers.last().cloned() {
            let error_name = handler.error_name;
            if self.locals.contains_key(&error_name) {
                self.current.instructions.push(IrInst::Store {
                    target: IrLValue::Local(error_name.clone()),
                    value: error_value,
                });
            } else {
                self.locals.insert(error_name.clone(), error_ty.clone());
                self.current.instructions.push(IrInst::Let {
                    name: error_name,
                    ty: error_ty,
                    value: error_value,
                });
            }
            IrTerminator::JumpErr {
                result,
                target: handler.error_block,
            }
        } else {
            IrTerminator::PropagateErr(result)
        }
    }

    fn return_terminator(&mut self, value: Option<IrExpr>) -> IrTerminator {
        let Some(handler) = self.try_handlers.last().cloned() else {
            return IrTerminator::Return(value);
        };
        let Some(return_block) = handler.return_block else {
            return IrTerminator::Return(value);
        };
        if let (Some(name), Some(value)) = (handler.return_name, value) {
            self.current.instructions.push(IrInst::Store {
                target: IrLValue::Local(name),
                value,
            });
        }
        IrTerminator::Jump(return_block)
    }

    fn next_block(&mut self, _name: &str) -> BlockId {
        let id = BlockId(self.next_block_id);
        self.next_block_id += 1;
        id
    }

    fn start_block(&mut self, id: BlockId, name: &str) {
        self.current = IrBlock {
            id,
            name: format!("{name}{}", id.0),
            instructions: Vec::new(),
            terminator: IrTerminator::Next,
        };
    }

    fn finish_current(&mut self) {
        let mut next = IrBlock {
            id: BlockId(self.next_block_id),
            name: format!("block{}", self.next_block_id),
            instructions: Vec::new(),
            terminator: IrTerminator::Next,
        };
        std::mem::swap(&mut self.current, &mut next);
        self.blocks.push(next);
    }
}

fn lower_type(ty: &TypeName, layouts: &IrLayoutTable) -> IrType {
    match ty {
        TypeName::Int => IrType::Int,
        TypeName::Float => IrType::Float,
        TypeName::Bool => IrType::Bool,
        TypeName::String => IrType::Str,
        TypeName::Null => IrType::Null,
        TypeName::Array(inner) => IrType::Array(Box::new(lower_type(inner, layouts))),
        TypeName::Result(inner) => IrType::Result(Box::new(lower_type(inner, layouts))),
        // A function-type annotation (`fn(int): int`) lowers to the same
        // monomorphized closure type as a closure value, so typed function
        // bindings and function-typed parameters share one representation.
        TypeName::Function {
            params,
            return_type,
            ..
        } => IrType::Closure {
            params: params.iter().map(|p| lower_type(p, layouts)).collect(),
            ret: Box::new(lower_type(return_type, layouts)),
        },
        TypeName::Union(_) => IrType::Unknown,
        TypeName::Custom(name) if layouts.enums.iter().any(|layout| layout.name == *name) => {
            enum_ir_type(name)
        }
        TypeName::Custom(name) => IrType::Named(name.clone()),
    }
}

fn lower_optional_type(ty: &Option<TypeName>, layouts: &IrLayoutTable) -> IrType {
    ty.as_ref()
        .map(|ty| lower_type(ty, layouts))
        .unwrap_or(IrType::Unknown)
}

fn lower_layouts(program: &Program) -> IrLayoutTable {
    let enum_names = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Enum(decl) => Some(decl.name.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut structs = Vec::new();
    let mut enums = Vec::new();
    for item in &program.items {
        match item {
            Item::Struct(decl) => structs.push(lower_struct_layout(decl, &enum_names)),
            Item::Enum(decl) => enums.push(lower_enum_layout(decl, &enum_names)),
            _ => {}
        }
    }
    IrLayoutTable { structs, enums }
}

fn lower_struct_layout(decl: &StructDecl, enum_names: &HashSet<String>) -> IrStructLayout {
    IrStructLayout {
        name: decl.name.clone(),
        fields: decl
            .fields
            .iter()
            .enumerate()
            .map(|(offset, field)| IrFieldLayout {
                name: field.name.clone(),
                ty: lower_layout_type(&field.ty, enum_names),
                offset,
            })
            .collect(),
    }
}

fn lower_enum_layout(decl: &EnumDecl, enum_names: &HashSet<String>) -> IrEnumLayout {
    IrEnumLayout {
        name: decl.name.clone(),
        variants: decl
            .variants
            .iter()
            .enumerate()
            .map(|(tag, variant)| IrVariantLayout {
                name: variant.name.clone(),
                tag,
                fields: variant
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(offset, field)| IrFieldLayout {
                        name: field.name.clone(),
                        ty: lower_layout_type(&field.ty, enum_names),
                        offset,
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn lower_layout_type(ty: &Option<TypeName>, enum_names: &HashSet<String>) -> IrType {
    fn lower(ty: &TypeName, enum_names: &HashSet<String>) -> IrType {
        match ty {
            TypeName::Int => IrType::Int,
            TypeName::Float => IrType::Float,
            TypeName::Bool => IrType::Bool,
            TypeName::String => IrType::Str,
            TypeName::Null => IrType::Null,
            TypeName::Array(inner) => IrType::Array(Box::new(lower(inner, enum_names))),
            TypeName::Result(inner) => IrType::Result(Box::new(lower(inner, enum_names))),
            TypeName::Function {
                params,
                return_type,
                ..
            } => IrType::Closure {
                params: params.iter().map(|p| lower(p, enum_names)).collect(),
                ret: Box::new(lower(return_type, enum_names)),
            },
            TypeName::Union(_) => IrType::Unknown,
            TypeName::Custom(name) if enum_names.contains(name) => enum_ir_type(name),
            TypeName::Custom(name) => IrType::Named(name.clone()),
        }
    }
    ty.as_ref()
        .map(|ty| lower(ty, enum_names))
        .unwrap_or(IrType::Unknown)
}

fn bool_literal(value: bool) -> IrExpr {
    IrExpr {
        kind: IrExprKind::Literal(value.to_string()),
        ty: IrType::Bool,
    }
}

fn and_expr(left: IrExpr, right: IrExpr) -> IrExpr {
    if left == bool_literal(true) {
        return right;
    }
    if right == bool_literal(true) {
        return left;
    }
    IrExpr {
        kind: IrExprKind::Binary {
            left: Box::new(left),
            op: BinaryOp::And,
            right: Box::new(right),
        },
        ty: IrType::Bool,
    }
}

const ENUM_TYPE_PREFIX: &str = "__ku_enum_type:";

fn enum_ir_type(name: &str) -> IrType {
    IrType::Named(format!("{ENUM_TYPE_PREFIX}{name}"))
}

fn enum_type_name(name: &str) -> Option<&str> {
    name.strip_prefix(ENUM_TYPE_PREFIX)
}

fn intrinsic_expr(name: impl Into<String>, args: Vec<IrExpr>, ty: IrType) -> IrExpr {
    let name = name.into();
    IrExpr {
        kind: IrExprKind::Call {
            callee: Box::new(IrExpr {
                kind: IrExprKind::Local(name.clone()),
                ty: IrType::Function,
            }),
            args,
            kind: IrCallKind::Intrinsic(name),
        },
        ty,
    }
}

fn zero_expr(ty: IrType) -> IrExpr {
    IrExpr {
        kind: IrExprKind::Literal("<native-zero>".to_string()),
        ty,
    }
}

fn error_ir_type() -> IrType {
    IrType::Named("__ku_error_type".to_string())
}

fn literal_text(literal: &Literal) -> String {
    match literal {
        Literal::Int(value) => value.to_string(),
        Literal::Float(value) => value.to_string(),
        Literal::Bool(value) => value.to_string(),
        Literal::String(value) | Literal::TemplateString(value) => format!("{value:?}"),
        Literal::Null => "null".to_string(),
    }
}

fn literal_type(literal: &Literal) -> IrType {
    match literal {
        Literal::Int(_) => IrType::Int,
        Literal::Float(_) => IrType::Float,
        Literal::Bool(_) => IrType::Bool,
        Literal::String(_) | Literal::TemplateString(_) => IrType::Str,
        Literal::Null => IrType::Null,
    }
}

fn binary_type(op: BinaryOp, left: &IrType, right: &IrType) -> IrType {
    match op {
        BinaryOp::Equal
        | BinaryOp::NotEqual
        | BinaryOp::Less
        | BinaryOp::LessEqual
        | BinaryOp::Greater
        | BinaryOp::GreaterEqual
        | BinaryOp::And
        | BinaryOp::Or => IrType::Bool,
        BinaryOp::Add
        | BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::Remainder => {
            if left == &IrType::Float || right == &IrType::Float {
                IrType::Float
            } else if left == &IrType::Int && right == &IrType::Int {
                IrType::Int
            } else if left == &IrType::Str && right == &IrType::Str && op == BinaryOp::Add {
                IrType::Str
            } else {
                IrType::Unknown
            }
        }
    }
}

fn call_kind_and_type(
    callee: &Expr,
    args: &[IrExpr],
    signatures: &HashMap<String, FunctionSig>,
) -> (IrCallKind, IrType) {
    if let ExprKind::Variable(name) = &callee.kind {
        if let Some(signature) = signatures.get(name) {
            return (IrCallKind::Direct(signature.id), signature.returns.clone());
        }
        if let Some(signature) = metadata::builtin_signature(name) {
            return (
                IrCallKind::Intrinsic(name.clone()),
                signature_return_type(&signature, args),
            );
        }
    }
    if let Some(name) = dotted_name(callee) {
        if let Some((module, function)) = name.split_once('.') {
            if let Some(signature) = metadata::dotted_signature(module, function) {
                // Stage 8a: give native HTTP builtins concrete synthetic types so
                // the server/response values flow through the backend as dedicated
                // C structs rather than the unknown-typed dynamic objects the
                // interpreter uses. Only affects native lowering.
                if let Some(ty) = http_builtin_ir_type(module, function) {
                    return (IrCallKind::Intrinsic(name), ty);
                }
                return (
                    IrCallKind::Intrinsic(name),
                    signature_return_type(&signature, args),
                );
            }
        }
        return (IrCallKind::Intrinsic(name), IrType::Unknown);
    }
    (IrCallKind::Indirect, IrType::Unknown)
}

/// Stage 8a: the synthetic IR type of the native HTTP server value, produced by
/// `http.server()`/`http.service()` and consumed by `app.get/listen`.
const HTTP_SERVER_TYPE: &str = "__ku_http_server";
/// Stage 8a: the synthetic IR type of an HTTP response produced by the response
/// helpers (`http.text`/`html`/`empty`/`redirect`).
const HTTP_RESPONSE_TYPE: &str = "__ku_http_response";
/// Stage 8a: the synthetic IR type of the request struct passed to `fn(req)`
/// route handlers (fields `method`/`path`/`body`).
const HTTP_REQUEST_TYPE: &str = "__ku_http_request";

/// The synthetic native IR type for a native-lowered HTTP builtin, or `None` for
/// builtins that keep their metadata-derived type.
fn http_builtin_ir_type(module: &str, function: &str) -> Option<IrType> {
    match (module, function) {
        ("http", "server" | "service") => Some(IrType::Named(HTTP_SERVER_TYPE.to_string())),
        ("http", "text" | "html" | "empty" | "redirect") => {
            Some(IrType::Named(HTTP_RESPONSE_TYPE.to_string()))
        }
        _ => None,
    }
}

fn signature_return_type(signature: &Signature, args: &[IrExpr]) -> IrType {
    pattern_to_ir_type(&signature.returns, args).unwrap_or(IrType::Unknown)
}

fn pattern_to_ir_type(pattern: &TypePattern, args: &[IrExpr]) -> Option<IrType> {
    match pattern {
        TypePattern::Int => Some(IrType::Int),
        TypePattern::Bool => Some(IrType::Bool),
        TypePattern::String => Some(IrType::Str),
        TypePattern::Null => Some(IrType::Null),
        TypePattern::KuValue => Some(IrType::Named("__ku_value".to_string())),
        TypePattern::Native(name) => Some(IrType::Named(name.to_string())),
        TypePattern::Unknown
        | TypePattern::Any
        | TypePattern::ObjectAny
        | TypePattern::ObjectFields(_)
        | TypePattern::StringOrStringArray => Some(IrType::Unknown),
        TypePattern::ArrayAny => Some(IrType::Array(Box::new(IrType::Unknown))),
        TypePattern::ArrayOf(inner) => {
            Some(IrType::Array(Box::new(pattern_to_ir_type(inner, args)?)))
        }
        TypePattern::ArrayElementOfArg(index) => match args.get(*index).map(|arg| &arg.ty) {
            Some(IrType::Array(inner)) => Some(*inner.clone()),
            _ => Some(IrType::Unknown),
        },
        TypePattern::ResultOf(inner) => {
            Some(IrType::Result(Box::new(pattern_to_ir_type(inner, args)?)))
        }
        TypePattern::SameAsArg(index) => args.get(*index).map(|arg| arg.ty.clone()),
    }
}

/// A receiver expression with no side effects, safe to lower more than once
/// while probing for builtin method dispatch.
fn is_pure_path(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Variable(_) | ExprKind::Literal(_) => true,
        ExprKind::Field { target, .. } => is_pure_path(target),
        ExprKind::Index { target, index } => is_pure_path(target) && is_pure_path(index),
        _ => false,
    }
}

fn dotted_name(expr: &Expr) -> Option<String> {
    let ExprKind::Field { target, name } = &expr.kind else {
        return None;
    };
    let ExprKind::Variable(module) = &target.kind else {
        return None;
    };
    Some(format!("{module}.{name}"))
}

fn unsupported_expr(reason: impl Into<String>) -> IrExpr {
    IrExpr {
        kind: IrExprKind::Literal(format!("<unsupported {}>", reason.into())),
        ty: IrType::Unknown,
    }
}

/// Stage 6b: only Copy scalars can be boxed into a shared cell for now; owned
/// payloads (str/array/object/struct/enum) are deferred to Stage 6c.
fn is_copy_ir_type(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::Int | IrType::Float | IrType::Bool | IrType::Null
    )
}

/// Stage 6b: collect the names captured by every closure literal (arrow function
/// or nested named function) reachable in `body`. Their intersection with the
/// locals a function declares is what must be boxed. Reuses the interpreter's
/// free-variable analysis so the native capture set matches the interpreter's
/// exactly.
fn collect_boxed_candidates(body: &[Stmt], out: &mut HashSet<String>) {
    for stmt in body {
        collect_boxed_candidates_stmt(stmt, out);
    }
}

fn collect_boxed_candidates_stmt(stmt: &Stmt, out: &mut HashSet<String>) {
    match stmt {
        Stmt::VarDecl { value, .. } | Stmt::Assign { value, .. } => {
            collect_boxed_candidates_expr(value, out)
        }
        Stmt::AssignTarget { target, value, .. } | Stmt::CompoundAssign { target, value, .. } => {
            collect_boxed_candidates_assign_target(target, out);
            collect_boxed_candidates_expr(value, out);
        }
        Stmt::DestructureAssign { values, .. } => {
            for value in values {
                collect_boxed_candidates_expr(value, out);
            }
        }
        Stmt::ObjectDestructureAssign {
            bindings, value, ..
        } => {
            collect_boxed_candidates_expr(value, out);
            for binding in bindings {
                if let Some(default) = &binding.default {
                    collect_boxed_candidates_expr(default, out);
                }
            }
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_boxed_candidates_expr(condition, out);
            collect_boxed_candidates(then_branch, out);
            collect_boxed_candidates(else_branch, out);
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_boxed_candidates_expr(condition, out);
            collect_boxed_candidates(body, out);
        }
        Stmt::For { iterable, body, .. } => {
            collect_boxed_candidates_expr(iterable, out);
            collect_boxed_candidates(body, out);
        }
        Stmt::Function(function) => {
            out.extend(crate::runtime::interpreter::function_capture_names(function));
            collect_boxed_candidates(&function.body, out);
        }
        Stmt::Try {
            body,
            catch_body,
            finally_body,
            ..
        } => {
            collect_boxed_candidates(body, out);
            collect_boxed_candidates(catch_body, out);
            collect_boxed_candidates(finally_body, out);
        }
        Stmt::Fail { value, .. } | Stmt::Panic { value, .. } | Stmt::Print { value, .. } => {
            collect_boxed_candidates_expr(value, out)
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                collect_boxed_candidates_expr(value, out);
            }
        }
        Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Expr { expr, .. } => collect_boxed_candidates_expr(expr, out),
    }
}

fn collect_boxed_candidates_assign_target(target: &AssignTarget, out: &mut HashSet<String>) {
    match target {
        AssignTarget::Variable(_) => {}
        AssignTarget::Index { target, index } => {
            collect_boxed_candidates_expr(target, out);
            collect_boxed_candidates_expr(index, out);
        }
        AssignTarget::Field { target, .. } => collect_boxed_candidates_expr(target, out),
    }
}

fn collect_boxed_candidates_expr(expr: &Expr, out: &mut HashSet<String>) {
    match &expr.kind {
        ExprKind::Function { params, body, .. } => {
            out.extend(crate::runtime::interpreter::closure_capture_names(
                params, body,
            ));
            collect_boxed_candidates(body, out);
        }
        ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } | ExprKind::Await(expr) => {
            collect_boxed_candidates_expr(expr, out)
        }
        ExprKind::Binary { left, right, .. } => {
            collect_boxed_candidates_expr(left, out);
            collect_boxed_candidates_expr(right, out);
        }
        ExprKind::Call { callee, args } => {
            collect_boxed_candidates_expr(callee, out);
            for arg in args {
                collect_boxed_candidates_expr(arg, out);
            }
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_boxed_candidates_expr(value, out);
            }
        }
        ExprKind::Index { target, index } => {
            collect_boxed_candidates_expr(target, out);
            collect_boxed_candidates_expr(index, out);
        }
        ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
            collect_boxed_candidates_expr(target, out)
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            for (_, value) in fields {
                collect_boxed_candidates_expr(value, out);
            }
        }
        ExprKind::Match { value, arms } => {
            collect_boxed_candidates_expr(value, out);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_boxed_candidates_expr(guard, out);
                }
                collect_boxed_candidates_expr(&arm.value, out);
            }
        }
        ExprKind::Literal(_) | ExprKind::Variable(_) => {}
    }
}

fn unary_text(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Negate => "-",
        UnaryOp::Not => "!",
    }
}

fn binary_text(op: BinaryOp) -> &'static str {
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
